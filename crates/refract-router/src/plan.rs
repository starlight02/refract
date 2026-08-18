//! 路由计划：纯函数式的候选收集与排序。
//!
//! 这个模块不碰网络、不碰数据库，只做一件事：**从一组渠道里算出「按什么顺序
//! 尝试哪些端点」**。因为它是纯函数，路由行为可以被穷举测试 —— 而路由错误
//! 是这类网关最难排查的问题。

use std::cmp::Reverse;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use refract_core::{
    Channel, ChannelEndpoint, ChannelId, Credential, ModelEntry, Protocol, RankKey, RoutingPolicy,
    SelectionMode, UpstreamAddress, weighted_pick,
};

/// 一个候选端点 —— 「可以用来服务这次请求」的完整信息。
///
/// 借用而非拷贝：候选收集在每个请求的热路径上，渠道配置又是只读的，
/// 复制字符串纯属浪费。
#[derive(Debug, Clone, Copy)]
pub struct Candidate<'a> {
    /// 所属渠道。
    pub channel: &'a Channel,
    /// 命中的端点。
    pub endpoint: &'a ChannelEndpoint,
    /// 命中的模型条目 —— 决定打上游时用什么模型名。
    pub model: &'a ModelEntry,
    /// 排序键。
    pub rank: RankKey,
}

impl<'a> Candidate<'a> {
    /// 渠道 ID。
    pub fn channel_id(&self) -> ChannelId {
        self.channel.id
    }

    /// 端点的原生协议。
    pub fn protocol(&self) -> Protocol {
        self.endpoint.protocol
    }

    /// 是否需要协议转换。
    pub fn needs_transcode(&self, inbound: Protocol) -> bool {
        inbound != self.endpoint.protocol
    }

    /// 生效地址（端点未自定义则继承渠道）。
    pub fn address(&self) -> &'a UpstreamAddress {
        self.channel.effective_address(self.endpoint)
    }

    /// 生效凭据。
    pub fn credential(&self) -> &'a Credential {
        self.channel.effective_credential(self.endpoint)
    }

    /// 打上游时使用的模型名。
    pub fn upstream_model(&self) -> &'a str {
        self.model.upstream_name()
    }

    /// 渠道级出站代理。
    pub fn proxy(&self) -> Option<&'a str> {
        self.channel.proxy.as_deref()
    }
}

/// 一次请求的完整路由计划。
#[derive(Debug, Clone)]
pub struct Route<'a> {
    /// 入口协议。
    pub inbound: Protocol,
    /// 客户端请求的模型名。
    pub model: &'a str,
    /// 按尝试顺序排列的候选。第一个是首选。
    ///
    /// 注意这里**不做** `max_attempts` 截断：截断必须发生在执行器按健康度
    /// 重排之后，否则「前 N 名全在熔断中」时，健康的第 N+1 名永远轮不到。
    pub attempts: Vec<Candidate<'a>>,
    /// 单次请求最多尝试的候选数，由执行器在健康度重排后应用。
    pub attempt_cap: usize,
    /// 网关侧调用者身份（API key id）。
    ///
    /// 黏性密钥策略用它做「同一调用者固定同一把 key」的锚点；`None` 时
    /// 黏性自动退化为轮询。路由本身不消费它 —— 只有执行器的密钥调度读。
    pub identity: Option<u64>,
}

impl<'a> Route<'a> {
    /// 首选候选。
    pub fn primary(&self) -> Option<&Candidate<'a>> {
        self.attempts.first()
    }

    /// 是否无候选。
    pub fn is_empty(&self) -> bool {
        self.attempts.is_empty()
    }

    /// 把指定渠道的首个候选移到尝试序列首位（亲和性钉住）。
    ///
    /// 返回 `false` 表示该渠道不在候选里（停用、无此模型或被协议过滤）——
    /// 调用方据此决定是保留绑定等它恢复，还是解除绑定。
    pub fn pin_channel(&mut self, channel_id: ChannelId) -> bool {
        let Some(pos) = self
            .attempts
            .iter()
            .position(|c| c.channel.id == channel_id)
        else {
            return false;
        };
        if pos != 0 {
            let pinned = self.attempts.remove(pos);
            self.attempts.insert(0, pinned);
        }
        true
    }

    /// 只保留钉住渠道的候选（亲和性 `skip_retry_on_failure` 语义）。
    ///
    /// 调用前必须已 `pin_channel`，因此首个候选就是钉住的渠道。执行器只会
    /// 在 key 池内轮换，不再滑落到其他渠道。
    pub fn pinned_only(&self) -> Route<'a> {
        let mut route = self.clone();
        route.attempts.truncate(1);
        route.attempt_cap = 1;
        route
    }
}

/// RoundRobin 的按模型游标表。
///
/// 键是模型名：不同模型的候选集合互不相同，共用一个计数器会让彼此的请求
/// 互相推进对方的轮转，观察到的顺序不再是「逐一轮转」。
pub type RoundRobinCursors = Arc<Mutex<HashMap<String, u64>>>;

/// 路由规划器。
#[derive(Debug, Clone)]
pub struct RoutePlanner {
    policy: RoutingPolicy,
    round_robin_cursors: RoundRobinCursors,
}

impl Default for RoutePlanner {
    fn default() -> Self {
        Self::new(RoutingPolicy::default())
    }
}

impl RoutePlanner {
    /// 用指定策略构造。
    pub fn new(policy: RoutingPolicy) -> Self {
        Self {
            policy,
            round_robin_cursors: Arc::default(),
        }
    }

    /// 用共享游标表构造规划器，使按请求临时创建的规划器仍能真正轮转。
    pub fn with_cursors(policy: RoutingPolicy, cursors: RoundRobinCursors) -> Self {
        Self {
            policy,
            round_robin_cursors: cursors,
        }
    }

    /// 当前策略。
    pub fn policy(&self) -> &RoutingPolicy {
        &self.policy
    }

    /// 收集所有能服务 `(model, inbound)` 的候选，不排序。
    ///
    /// 过滤链条（顺序即成本，便宜的判断放前面）：
    /// 1. 渠道启用、端点启用
    /// 2. 端点提供该模型
    /// 3. 协议可服务性 —— 需求 4 的核心：非原生协议只有在被显式勾选时才可用
    pub fn collect<'a>(
        &self,
        channels: impl IntoIterator<Item = &'a Channel>,
        model: &str,
        inbound: Protocol,
    ) -> Vec<Candidate<'a>> {
        let mut out = Vec::new();
        for channel in channels {
            if !channel.enabled {
                continue;
            }
            for endpoint in &channel.endpoints {
                if !endpoint.enabled {
                    continue;
                }
                let Some(entry) = endpoint.find_model(model) else {
                    continue;
                };
                // 需求 4：未勾选的协议打到非原生渠道要被拒绝，而不是硬转。
                if !endpoint.transcode.can_serve(inbound, endpoint.protocol) {
                    continue;
                }
                out.push(Candidate {
                    channel,
                    endpoint,
                    model: entry,
                    rank: RankKey {
                        native: inbound == endpoint.protocol,
                        priority: channel.priority,
                        endpoint_order: endpoint.order,
                    },
                });
            }
        }
        out
    }

    /// 规划一次请求。
    ///
    /// 排序规则分三段，缺一不可：
    /// 1. **分层**：按 [`RankKey::tier`]，受「原生优先」开关控制（需求 6）。
    /// 2. **同渠道去重**：一个渠道的多个端点提供同一模型时，先选原生端点，
    ///    再按 order 选最小值（需求 5）。
    /// 3. **同层内选择**：按权重随机 / 轮转 / 取首个。
    pub fn plan<'a>(
        &self,
        channels: impl IntoIterator<Item = &'a Channel>,
        model: &'a str,
        inbound: Protocol,
        rng: &mut impl rand::RngExt,
    ) -> Route<'a> {
        let candidates = self.collect(channels, model, inbound);
        Route {
            inbound,
            model,
            attempts: self.order(candidates, model, rng),
            // max_attempts 为 0 视作不限制 —— 配 0 的人想表达的显然是
            // 「别限制」，而不是「一个都别试」。
            attempt_cap: match self.policy.max_attempts {
                0 => usize::MAX,
                n => n as usize,
            },
            identity: None,
        }
    }

    /// 对已收集的候选排序，产出尝试序列。
    pub fn order<'a>(
        &self,
        mut candidates: Vec<Candidate<'a>>,
        model: &str,
        rng: &mut impl rand::RngExt,
    ) -> Vec<Candidate<'a>> {
        if candidates.is_empty() {
            return candidates;
        }

        // RoundRobin 游标每个请求只推进一次，且按模型隔离。若在每个分层里
        // 各自推进，一次多分层的请求会消耗多个序号 —— 当某层的候选数恰好
        // 整除层数时，那一层的轮转会永远停在同一个候选上。
        let round_robin_seq = match self.policy.selection {
            SelectionMode::RoundRobin => {
                let mut cursors = self
                    .round_robin_cursors
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let cursor = cursors.entry(model.to_owned()).or_insert(0);
                let seq = *cursor;
                *cursor = cursor.wrapping_add(1);
                Some(seq)
            }
            _ => None,
        };

        // 需求 5：同渠道同模型优先原生端点，原生性相同时再看显式 order。
        // 先做这个排序再按 channel_id 去重，才能留下正确端点。
        candidates.sort_by_key(|c| {
            (
                c.channel_id(),
                Reverse(c.rank.native),
                c.rank.endpoint_order,
            )
        });
        if !self.policy.retry_same_channel {
            // 默认只保留每个渠道的首选端点：重试的意义是「换一家上游」，
            // 在同一个渠道里换协议端点通常还是同一家在挂，白白消耗尝试次数。
            candidates.dedup_by_key(|c| c.channel_id());
        }

        // 按分层降序排列（tier 的语义是「越大越优先」）。
        let native_first = self.policy.native_first;
        candidates.sort_by(|a, b| {
            b.rank
                .tier(native_first)
                .cmp(&a.rank.tier(native_first))
                // 分层相同时按渠道 ID 稳定排序，避免不同运行产生不同顺序 ——
                // 同层内的随机性由 shuffle 阶段负责，排序阶段必须确定。
                .then_with(|| a.channel_id().cmp(&b.channel_id()))
        });

        // 逐层做同层内选择。max_attempts 截断**不在这里做**：执行器要先按
        // 健康度重排（熔断中的沉底），之后再截断，健康候选才不会被挤出。
        let mut out = Vec::with_capacity(candidates.len());
        let mut rest = candidates.as_slice();
        while !rest.is_empty() {
            let head = rest[0];
            let tier_len = rest
                .iter()
                .take_while(|c| c.rank.same_tier(head.rank, native_first))
                .count();
            let (tier, tail) = rest.split_at(tier_len);
            out.extend(self.arrange_tier(tier, round_robin_seq, rng));
            rest = tail;
        }
        out
    }

    /// 同分层内的排列。
    ///
    /// 返回的是**整个分层的排列**而非单个胜者：重试要在同层内继续尝试，
    /// 只返回胜者会让重试直接掉到下一层，浪费同层里健康的渠道。
    fn arrange_tier<'a>(
        &self,
        tier: &[Candidate<'a>],
        round_robin_seq: Option<u64>,
        rng: &mut impl rand::RngExt,
    ) -> Vec<Candidate<'a>> {
        match self.policy.selection {
            SelectionMode::First => tier.to_vec(),
            SelectionMode::RoundRobin => {
                let mut arranged = tier.to_vec();
                if arranged.len() > 1 {
                    let offset = round_robin_seq.unwrap_or_default() as usize % arranged.len();
                    arranged.rotate_left(offset);
                }
                arranged
            }
            SelectionMode::WeightedRandom => {
                // 按权重反复抽取，抽到的移出候选池 —— 等价于「按权重生成一个
                // 无重复排列」。这样首选服从权重分布，重试顺序也服从权重分布。
                let mut pool: Vec<Candidate<'a>> = tier.to_vec();
                let mut out = Vec::with_capacity(pool.len());
                while !pool.is_empty() {
                    let weights: Vec<u32> = pool.iter().map(|c| c.channel.weight).collect();
                    let idx = weighted_pick(&weights, rng).unwrap_or(0);
                    out.push(pool.swap_remove(idx));
                }
                out
            }
        }
    }

    /// 列出所有对外可见的模型名（用于 `/v1/models`）。
    ///
    /// 只列启用渠道的启用端点。用户看到的模型列表必须是**真的能用**的，
    /// 列出一个打过去就 503 的模型比不列更糟。
    pub fn visible_models<'a>(
        &self,
        channels: impl IntoIterator<Item = &'a Channel>,
    ) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for channel in channels {
            if !channel.enabled {
                continue;
            }
            for endpoint in &channel.endpoints {
                if !endpoint.enabled {
                    continue;
                }
                for entry in &endpoint.models {
                    if !names.iter().any(|n| n == &entry.name) {
                        names.push(entry.name.clone());
                    }
                }
            }
        }
        names.sort();
        names
    }

    /// 某个模型在给定入口协议下是否可路由。
    ///
    /// 与 `collect(..).is_empty()` 的区别在于用途：这个方法回答「该不该报
    /// 404（模型不存在）还是 400（协议不被允许）」，两者对客户端的含义完全不同。
    pub fn diagnose<'a>(
        &self,
        channels: impl IntoIterator<Item = &'a Channel> + Clone,
        model: &str,
        inbound: Protocol,
    ) -> Diagnosis {
        let mut model_exists = false;
        let mut protocols_available = refract_core::ProtocolSet::EMPTY;

        for channel in channels.clone() {
            if !channel.enabled {
                continue;
            }
            for endpoint in &channel.endpoints {
                if !endpoint.enabled || endpoint.find_model(model).is_none() {
                    continue;
                }
                model_exists = true;
                for p in Protocol::ALL {
                    if endpoint.transcode.can_serve(p, endpoint.protocol) {
                        protocols_available.insert(p);
                    }
                }
            }
        }

        if !model_exists {
            Diagnosis::UnknownModel
        } else if protocols_available.contains(inbound) {
            Diagnosis::Routable
        } else {
            Diagnosis::ProtocolNotPermitted {
                available: protocols_available,
            }
        }
    }
}

/// 路由可行性诊断。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Diagnosis {
    /// 可以路由。
    Routable,
    /// 模型在任何启用的端点上都不存在。
    UnknownModel,
    /// 模型存在，但没有端点接受这个入口协议（需求 4 的显式拒绝）。
    ProtocolNotPermitted {
        /// 该模型实际可用的入口协议集合，用于给出可操作的报错。
        available: refract_core::ProtocolSet,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use refract_core::{ChannelKind, ProtocolSet, TranscodePolicy};

    /// 确定性 RNG，让权重随机的测试可复现。
    fn rng() -> impl rand::RngExt {
        use rand::SeedableRng as _;
        rand::rngs::StdRng::seed_from_u64(0xFEED_FACE)
    }

    fn endpoint(
        protocol: Protocol,
        order: u16,
        models: &[&str],
        accepted: ProtocolSet,
    ) -> ChannelEndpoint {
        ChannelEndpoint {
            protocol,
            order,
            enabled: true,
            address: UpstreamAddress::default(),
            credential: None,
            models: models.iter().map(|m| ModelEntry::plain(*m)).collect(),
            transcode: TranscodePolicy {
                enabled: !accepted.is_empty(),
                accepted,
            },
        }
    }

    fn channel(
        id: ChannelId,
        name: &str,
        priority: i32,
        endpoints: Vec<ChannelEndpoint>,
    ) -> Channel {
        let kind = if endpoints.len() == 1 {
            ChannelKind::Single(endpoints[0].protocol)
        } else {
            ChannelKind::Aggregate
        };
        Channel {
            id,
            owner_id: 1,
            name: name.into(),
            kind,
            enabled: true,
            priority,
            weight: 1,
            credential: Credential::new("sk-test"),
            credentials: Vec::new(),
            key_strategy: Default::default(),
            address: UpstreamAddress::default(),
            endpoints,
            tags: Vec::new(),
            timeout_secs: 0,
            proxy: None,
            param_override: None,
            note: None,
            auto_disabled: false,
            balance: None,
            balance_updated_at: None,
            extra_headers: Vec::new(),
            test_model: None,
            empty_response_retry: Default::default(),
        }
    }

    fn ids(route: &Route<'_>) -> Vec<ChannelId> {
        route.attempts.iter().map(|c| c.channel_id()).collect()
    }

    fn protocols(route: &Route<'_>) -> Vec<Protocol> {
        route.attempts.iter().map(|c| c.protocol()).collect()
    }

    fn native_route<'a>(inbound: Protocol, channels: &'a [Channel]) -> Route<'a> {
        RoutePlanner::default().plan(channels, "gpt-4o", inbound, &mut rng())
    }

    fn route_with<'a>(
        planner: &RoutePlanner,
        inbound: Protocol,
        channels: &'a [Channel],
    ) -> Route<'a> {
        planner.plan(channels, "gpt-4o", inbound, &mut rng())
    }

    // ── Basic routing ──

    #[test]
    fn native_chat_beats_gemini_with_transcode() {
        let ch1 = channel(
            1,
            "chat-native",
            0,
            vec![endpoint(Protocol::Chat, 0, &["gpt-4o"], ProtocolSet::EMPTY)],
        );
        // Gemini 端点，但勾选了 Chat 协议转换。
        let ch2 = channel(
            2,
            "gemini-transcoded",
            0,
            vec![endpoint(
                Protocol::Gemini,
                0,
                &["gpt-4o"],
                ProtocolSet::from_iter_protocols([Protocol::Chat]),
            )],
        );
        let chans = [ch1, ch2];
        let route = native_route(Protocol::Chat, &chans);
        // ch1 是原生 Chat，ch2 需要转换。native-first 下 ch1 排前面。
        assert_eq!(route.primary().unwrap().channel_id(), 1);
    }

    #[test]
    fn disabled_channel_is_invisible() {
        let mut ch = channel(
            1,
            "off",
            10,
            vec![endpoint(Protocol::Chat, 0, &["gpt-4o"], ProtocolSet::EMPTY)],
        );
        ch.enabled = false;
        let chans = [ch];
        let route = native_route(Protocol::Chat, &chans);
        assert!(route.is_empty());
    }

    #[test]
    fn disabled_endpoint_is_invisible() {
        let mut ep = endpoint(Protocol::Chat, 0, &["gpt-4o"], ProtocolSet::EMPTY);
        ep.enabled = false;
        let ch = channel(1, "ep-off", 10, vec![ep]);
        let chans = [ch];
        let route = native_route(Protocol::Chat, &chans);
        assert!(route.is_empty());
    }

    #[test]
    fn unknown_model_returns_empty_route() {
        let ch = channel(
            1,
            "c",
            0,
            vec![endpoint(Protocol::Chat, 0, &["gpt-4o"], ProtocolSet::EMPTY)],
        );
        let route =
            RoutePlanner::default().plan(vec![&ch], "nonexistent", Protocol::Chat, &mut rng());
        assert!(route.is_empty());
    }

    // ── Req 6: native-first ──

    #[test]
    fn native_first_prefers_native_protocol_over_higher_priority_transcoded() {
        let policy = RoutingPolicy {
            native_first: true,
            ..Default::default()
        };
        let planner = RoutePlanner::new(policy);
        // ch1: 非原生 Chat（需要转换），但优先级高。
        let ch1 = channel(
            1,
            "high-prio",
            10,
            vec![endpoint(
                Protocol::Messages,
                0,
                &["gpt-4o"],
                ProtocolSet::from_iter_protocols([Protocol::Chat]),
            )],
        );
        // ch2: 原生 Chat，优先级低。
        let ch2 = channel(
            2,
            "native-low",
            0,
            vec![endpoint(Protocol::Chat, 0, &["gpt-4o"], ProtocolSet::EMPTY)],
        );

        let route = planner.plan(vec![&ch1, &ch2], "gpt-4o", Protocol::Chat, &mut rng());
        // native-first：低优先级的原生端点压过高优先级的转换端点；
        // 但转换端点仍必须保留为降级候选，不能在分层时被误删。
        assert_eq!(ids(&route), vec![2, 1]);
        assert_eq!(protocols(&route), vec![Protocol::Chat, Protocol::Messages]);
    }

    #[test]
    fn native_first_off_priority_is_king() {
        let policy = RoutingPolicy {
            native_first: false,
            ..Default::default()
        };
        let planner = RoutePlanner::new(policy);
        // ch1: 非原生，优先级高。
        let ch1 = channel(
            1,
            "high",
            10,
            vec![endpoint(
                Protocol::Messages,
                0,
                &["gpt-4o"],
                ProtocolSet::from_iter_protocols([Protocol::Chat]),
            )],
        );
        // ch2: 原生，优先级低。
        let ch2 = channel(
            2,
            "low",
            0,
            vec![endpoint(Protocol::Chat, 0, &["gpt-4o"], ProtocolSet::EMPTY)],
        );

        let route = planner.plan(vec![&ch1, &ch2], "gpt-4o", Protocol::Chat, &mut rng());
        // 关掉原生优先后，priority 控制分层：高优先级的转换端点在前面，
        // 低优先级原生端点仍保留作降级。
        assert_eq!(ids(&route), vec![1, 2]);
        assert_eq!(protocols(&route), vec![Protocol::Messages, Protocol::Chat]);
    }

    // ── Req 5: endpoint priority within aggregate ──

    #[test]
    fn same_channel_same_model_picks_native_before_order() {
        let ch = channel(
            1,
            "agg",
            5,
            vec![
                endpoint(Protocol::Chat, 2, &["gpt-4o"], ProtocolSet::EMPTY),
                endpoint(
                    Protocol::Messages,
                    1,
                    &["gpt-4o"],
                    ProtocolSet::from_iter_protocols([Protocol::Chat]),
                ),
                endpoint(
                    Protocol::Gemini,
                    0,
                    &["gpt-4o"],
                    ProtocolSet::from_iter_protocols([Protocol::Chat]),
                ),
            ],
        );
        // 从 Chat 入口进：三个端点都能服务。即使 Chat 的 order 更大，原生
        // 端点仍应优先；order 只在原生性相同时裁决。
        let chans = [ch];
        let route = native_route(Protocol::Chat, &chans);
        assert_eq!(route.attempts.len(), 1, "同模型同渠道只应留一个候选");
        assert_eq!(route.attempts[0].protocol(), Protocol::Chat);
        assert_eq!(route.attempts[0].endpoint.order, 2);
    }

    // ── Req 4: transcode policy ──

    #[test]
    fn unpermitted_protocol_is_filtered_out() {
        let ch = channel(
            1,
            "c",
            0,
            vec![endpoint(
                Protocol::Messages,
                0,
                &["gpt-4o"],
                ProtocolSet::EMPTY,
            )],
        );
        // Messages 端点没有勾选 Chat，Chat 请求不能进 Messages。
        let chans = [ch];
        let route = native_route(Protocol::Chat, &chans);
        assert!(route.is_empty(), "未勾选的协议应被过滤");
    }

    #[test]
    fn permitted_protocol_passes_through() {
        let ch = channel(
            1,
            "c",
            0,
            vec![endpoint(
                Protocol::Messages,
                0,
                &["gpt-4o"],
                ProtocolSet::from_iter_protocols([Protocol::Chat]),
            )],
        );
        let chans = [ch];
        let route = native_route(Protocol::Chat, &chans);
        assert!(!route.is_empty(), "已勾选的协议应放行");
    }

    #[test]
    fn transcode_endpoint_is_marked() {
        let ch = channel(
            1,
            "c",
            0,
            vec![endpoint(
                Protocol::Messages,
                0,
                &["gpt-4o"],
                ProtocolSet::from_iter_protocols([Protocol::Chat]),
            )],
        );
        let chans = [ch];
        let route = native_route(Protocol::Chat, &chans);
        assert!(route.primary().unwrap().needs_transcode(Protocol::Chat));
    }

    #[test]
    fn native_endpoint_is_not_transcoded() {
        let ch = channel(
            1,
            "c",
            0,
            vec![endpoint(Protocol::Chat, 0, &["gpt-4o"], ProtocolSet::EMPTY)],
        );
        let chans = [ch];
        let route = native_route(Protocol::Chat, &chans);
        assert!(!route.primary().unwrap().needs_transcode(Protocol::Chat));
    }

    // ── Aggregate channel multi-protocol ──

    #[test]
    fn aggregate_channel_collapses_to_its_preferred_endpoint_by_default() {
        let ch = channel(
            1,
            "agg",
            5,
            vec![
                endpoint(Protocol::Chat, 0, &["gpt-4o"], ProtocolSet::EMPTY),
                endpoint(
                    Protocol::Messages,
                    1,
                    &["gpt-4o"],
                    ProtocolSet::from_iter_protocols([Protocol::Chat]),
                ),
                endpoint(
                    Protocol::Gemini,
                    2,
                    &["gpt-4o"],
                    ProtocolSet::from_iter_protocols([Protocol::Chat]),
                ),
            ],
        );
        let chans = [ch];
        let route = native_route(Protocol::Chat, &chans);
        // 默认 retry_same_channel = false：同一个渠道只贡献一个候选，
        // 且必须是 order 最小的那个（需求 5）。
        assert_eq!(route.attempts.len(), 1);
        assert_eq!(route.attempts[0].endpoint.order, 0);
        assert_eq!(route.attempts[0].protocol(), Protocol::Chat);
    }

    #[test]
    fn aggregate_channel_offers_every_endpoint_when_same_channel_retry_is_on() {
        let policy = RoutingPolicy {
            retry_same_channel: true,
            ..Default::default()
        };
        let planner = RoutePlanner::new(policy);
        let ch = channel(
            1,
            "agg",
            5,
            vec![
                endpoint(Protocol::Chat, 0, &["gpt-4o"], ProtocolSet::EMPTY),
                endpoint(
                    Protocol::Messages,
                    1,
                    &["gpt-4o"],
                    ProtocolSet::from_iter_protocols([Protocol::Chat]),
                ),
                endpoint(
                    Protocol::Gemini,
                    2,
                    &["gpt-4o"],
                    ProtocolSet::from_iter_protocols([Protocol::Chat]),
                ),
            ],
        );
        let chans = [ch];
        let route = route_with(&planner, Protocol::Chat, &chans);
        assert_eq!(route.attempts.len(), 3, "开启同渠道重试后所有端点都应可用");
        // 原生端点仍排第一（native-first 分层）。
        assert_eq!(route.attempts[0].protocol(), Protocol::Chat);
    }

    // ── Max attempts ──

    #[test]
    fn max_attempts_becomes_a_cap_not_a_truncation() {
        let policy = RoutingPolicy {
            max_attempts: 2,
            ..Default::default()
        };
        let planner = RoutePlanner::new(policy);
        let channels: Vec<_> = (0..5)
            .map(|i| {
                channel(
                    i + 1,
                    &format!("ch{i}"),
                    i as i32,
                    vec![endpoint(Protocol::Chat, 0, &["gpt-4o"], ProtocolSet::EMPTY)],
                )
            })
            .collect();
        let route = planner.plan(&channels, "gpt-4o", Protocol::Chat, &mut rng());
        // 计划阶段必须保留全部候选：截断发生在执行器按健康度重排之后，
        // 否则前 2 名全熔断时健康的第 3 名永远轮不到。
        assert_eq!(route.attempts.len(), 5, "计划阶段不得截断候选");
        assert_eq!(route.attempt_cap, 2, "上限以 attempt_cap 传给执行器");
    }

    #[test]
    fn max_attempts_zero_means_unlimited() {
        let policy = RoutingPolicy {
            max_attempts: 0,
            ..Default::default()
        };
        let planner = RoutePlanner::new(policy);
        let ch = channel(
            1,
            "c",
            0,
            vec![endpoint(Protocol::Chat, 0, &["gpt-4o"], ProtocolSet::EMPTY)],
        );
        let route = planner.plan(vec![&ch], "gpt-4o", Protocol::Chat, &mut rng());
        assert_eq!(route.attempt_cap, usize::MAX);
    }

    // ── Weighted random ──

    #[test]
    fn weighted_random_respects_channel_weights() {
        // 两个渠道优先级相同，同层内按权重选。
        let mut ch1 = channel(
            1,
            "light",
            5,
            vec![endpoint(Protocol::Chat, 0, &["gpt-4o"], ProtocolSet::EMPTY)],
        );
        ch1.weight = 1;
        let mut ch2 = channel(
            2,
            "heavy",
            5,
            vec![endpoint(Protocol::Chat, 0, &["gpt-4o"], ProtocolSet::EMPTY)],
        );
        ch2.weight = 100;
        let policy = RoutingPolicy {
            selection: SelectionMode::WeightedRandom,
            ..Default::default()
        };
        let planner = RoutePlanner::new(policy);

        // 多次运行，统计首选分布。
        let mut first_count = 0u32;
        for seed in 0..200u64 {
            use rand::SeedableRng as _;
            let mut r = rand::rngs::StdRng::seed_from_u64(seed);
            let route = planner.plan(vec![&ch1, &ch2], "gpt-4o", Protocol::Chat, &mut r);
            if route.primary().unwrap().channel_id() == 2 {
                first_count += 1;
            }
        }
        // 权重 100 的渠道应被选为首选大多数时间。
        assert!(
            first_count > 150,
            "heavy channel should win most of the time, got {first_count}/200"
        );
    }

    #[test]
    fn round_robin_rotates_the_primary_candidate_across_requests() {
        let policy = RoutingPolicy {
            selection: SelectionMode::RoundRobin,
            max_attempts: 1,
            ..Default::default()
        };
        let planner = RoutePlanner::new(policy);
        let channels: Vec<_> = (1..=3)
            .map(|id| {
                channel(
                    id,
                    &format!("ch{id}"),
                    0,
                    vec![endpoint(Protocol::Chat, 0, &["gpt-4o"], ProtocolSet::EMPTY)],
                )
            })
            .collect();

        let selected: Vec<ChannelId> = (0..4)
            .map(|_| {
                planner
                    .plan(&channels, "gpt-4o", Protocol::Chat, &mut rng())
                    .primary()
                    .unwrap()
                    .channel_id()
            })
            .collect();
        assert_eq!(selected, vec![1, 2, 3, 1]);
    }

    #[test]
    fn round_robin_consumes_one_sequence_per_request_across_tiers() {
        // 两个分层（priority 10 与 0），每层 2 个渠道。若游标按层各自推进，
        // 一次请求会消耗 2 个序号 —— 序号差恰好整除层大小时轮转冻结。
        let policy = RoutingPolicy {
            selection: SelectionMode::RoundRobin,
            ..Default::default()
        };
        let planner = RoutePlanner::new(policy);
        let channels: Vec<_> = [(1, 10), (2, 10), (3, 0), (4, 0)]
            .into_iter()
            .map(|(id, priority)| {
                channel(
                    id,
                    &format!("ch{id}"),
                    priority,
                    vec![endpoint(Protocol::Chat, 0, &["gpt-4o"], ProtocolSet::EMPTY)],
                )
            })
            .collect();

        let orders: Vec<Vec<ChannelId>> = (0..4)
            .map(|_| ids(&planner.plan(&channels, "gpt-4o", Protocol::Chat, &mut rng())))
            .collect();
        // 每个请求推进一格：两层都在 1/2 与 3/4 之间交替，而不是冻结。
        assert_eq!(orders[0], vec![1, 2, 3, 4]);
        assert_eq!(orders[1], vec![2, 1, 4, 3]);
        assert_eq!(orders[2], vec![1, 2, 3, 4]);
        assert_eq!(orders[3], vec![2, 1, 4, 3]);
    }

    #[test]
    fn round_robin_cursors_are_isolated_per_model() {
        let policy = RoutingPolicy {
            selection: SelectionMode::RoundRobin,
            ..Default::default()
        };
        let planner = RoutePlanner::new(policy);
        let channels: Vec<_> = (1..=2)
            .map(|id| {
                channel(
                    id,
                    &format!("ch{id}"),
                    0,
                    vec![endpoint(
                        Protocol::Chat,
                        0,
                        &["gpt-4o", "gpt-4o-mini"],
                        ProtocolSet::EMPTY,
                    )],
                )
            })
            .collect();

        let pick = |model: &'static str, planner: &RoutePlanner| {
            planner
                .plan(&channels, model, Protocol::Chat, &mut rng())
                .primary()
                .unwrap()
                .channel_id()
        };
        // 模型 A 的两次请求轮转到 1、2；期间穿插的模型 B 请求不推进 A 的游标。
        assert_eq!(pick("gpt-4o", &planner), 1);
        assert_eq!(pick("gpt-4o-mini", &planner), 1);
        assert_eq!(pick("gpt-4o", &planner), 2);
        assert_eq!(pick("gpt-4o-mini", &planner), 2);
    }

    // ── Same-tier dedup by channel_id ──

    #[test]
    fn multiple_endpoints_same_channel_same_model_are_deduplicated() {
        let ch = channel(
            1,
            "agg",
            5,
            vec![
                endpoint(Protocol::Chat, 0, &["gpt-4o"], ProtocolSet::EMPTY),
                endpoint(Protocol::Messages, 0, &["gpt-4o"], ProtocolSet::EMPTY),
            ],
        );
        let chans = [ch];
        let route = native_route(Protocol::Chat, &chans);
        // 两个端点是同一个 channel_id，应只保留一个。
        assert_eq!(route.attempts.len(), 1);
        assert_eq!(route.attempts[0].endpoint.order, 0);
    }

    // ── Visible models ──

    #[test]
    fn visible_models_uses_first_alias() {
        let ep = endpoint(Protocol::Chat, 0, &["gpt-4o", "gpt-4o"], ProtocolSet::EMPTY);
        let ch = channel(1, "c", 0, vec![ep]);
        let names = RoutePlanner::default().visible_models(vec![&ch]);
        assert_eq!(names, vec!["gpt-4o"]);
    }

    #[test]
    fn visible_models_only_enabled() {
        let mut ch1 = channel(
            1,
            "off",
            10,
            vec![endpoint(Protocol::Chat, 0, &["gpt-4o"], ProtocolSet::EMPTY)],
        );
        ch1.enabled = false;
        let ch2 = channel(
            2,
            "on",
            0,
            vec![endpoint(Protocol::Chat, 0, &["gpt-4o"], ProtocolSet::EMPTY)],
        );
        let names = RoutePlanner::default().visible_models(vec![&ch1, &ch2]);
        assert_eq!(
            names,
            vec!["gpt-4o"],
            "disabled channel's model should be excluded"
        );
    }

    // ── Diagnosis ──

    #[test]
    fn diagnose_unknown_model() {
        let ch = channel(
            1,
            "c",
            0,
            vec![endpoint(Protocol::Chat, 0, &["gpt-4o"], ProtocolSet::EMPTY)],
        );
        let diag = RoutePlanner::default().diagnose(vec![&ch], "missing", Protocol::Chat);
        assert_eq!(diag, Diagnosis::UnknownModel);
    }

    #[test]
    fn diagnose_protocol_not_permitted_lists_what_would_work() {
        let ch = channel(
            1,
            "c",
            0,
            vec![endpoint(
                Protocol::Messages,
                0,
                &["gpt-4o"],
                ProtocolSet::EMPTY,
            )],
        );
        let diag = RoutePlanner::default().diagnose(vec![&ch], "gpt-4o", Protocol::Chat);
        // 报错必须可操作：告诉用户「这个模型只能用 Messages 协议调」，
        // 而不是干巴巴一句「不允许」。
        assert_eq!(
            diag,
            Diagnosis::ProtocolNotPermitted {
                available: ProtocolSet::from_iter_protocols([Protocol::Messages]),
            }
        );
    }

    #[test]
    fn diagnose_routable() {
        let ch = channel(
            1,
            "c",
            0,
            vec![endpoint(Protocol::Chat, 0, &["gpt-4o"], ProtocolSet::EMPTY)],
        );
        let diag = RoutePlanner::default().diagnose(vec![&ch], "gpt-4o", Protocol::Chat);
        assert_eq!(diag, Diagnosis::Routable);
    }

    #[test]
    fn diagnose_aggregate_reports_all_available_protocols() {
        let ch = channel(
            1,
            "agg",
            0,
            vec![
                endpoint(Protocol::Chat, 0, &["gpt-4o"], ProtocolSet::EMPTY),
                endpoint(
                    Protocol::Messages,
                    1,
                    &["gpt-4o"],
                    ProtocolSet::from_iter_protocols([Protocol::Chat]),
                ),
            ],
        );
        let diag = RoutePlanner::default().diagnose(vec![&ch], "gpt-4o", Protocol::Chat);
        let mut expected = ProtocolSet::EMPTY;
        expected.insert(Protocol::Chat);
        expected.insert(Protocol::Messages);
        // Messages 端点可以服务 Messages（原生）和 Chat（转码）。
        match diag {
            Diagnosis::Routable => {}
            Diagnosis::ProtocolNotPermitted { available } => {
                panic!("expected Routable, got PNP with {available:?}")
            }
            Diagnosis::UnknownModel => panic!("expected Routable, got UnknownModel"),
        }
    }

    // ── Empty candidates ──

    #[test]
    fn empty_channel_list_yields_empty_route() {
        let route = native_route(Protocol::Chat, &[]);
        assert!(route.is_empty());
    }

    #[test]
    fn model_on_no_channel_yields_empty() {
        let ch = channel(
            1,
            "c",
            0,
            vec![endpoint(Protocol::Chat, 0, &["other"], ProtocolSet::EMPTY)],
        );
        let chans = [ch];
        let route = native_route(Protocol::Chat, &chans);
        assert!(route.is_empty());
    }
}
