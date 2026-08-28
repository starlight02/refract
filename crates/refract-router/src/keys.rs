//! 多密钥池的调度状态：密钥选择 + 失败轮转 + 成功后提交。
//!
//! 策略语义：
//! - [`KeyStrategy::RoundRobin`]：每个渠道一个游标，选池内下一把 key，跨请求
//!   均匀分布负载与配额消耗。
//! - [`KeyStrategy::Sticky`]：同一身份（网关 API key 或请求头标识）在同一个
//!   渠道内持续用同一把 key —— 上游按 key 维度记账/限速时避免「身份漂移」。
//!   身份缺失时退化为轮询（没有身份就没有「黏」的锚点）。
//! - [`KeyStrategy::Random`]：每次均匀随机，无状态、无锁竞争。
//!
//! 轮转只发生在**鉴权族失败**（`ErrorKind::is_key_failure`：401/403/429 类）。
//! 其他错误（连接失败、上游 500、配置错误）与 key 无关，轮转只是徒劳地烧掉
//! 剩余 key。

use std::collections::HashMap;
use std::sync::Arc;

use rand::RngExt as _;
use refract_core::channel::{ChannelId, Credential, KeyPool, KeyStrategy};
use refract_core::error::{ErrorKind, GatewayError};

/// 密钥池调度器 —— 按渠道维护轮询游标与黏性绑定。
///
/// 黏性以「渠道 × 身份」为粒度：同一个网关 API key（或 `x-api-key-id` 头）
/// 在不同渠道各自有稳定的 key 下标。渠道删除时调用 [`Self::forget_channel`]
/// 回收两份映射，防止长期运行下无界增长。
#[derive(Debug, Default)]
pub struct KeySelector {
    inner: Arc<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    /// 渠道轮询游标：下一个要发的池内下标。
    cursors: parking_lot::Mutex<HashMap<ChannelId, u64>>,
    /// 黏性绑定：(渠道, 身份) → 池内下标。
    sticky: parking_lot::Mutex<HashMap<(ChannelId, u64), usize>>,
}

impl Clone for KeySelector {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl KeySelector {
    /// 空调度器：没有游标与绑定。
    pub fn new() -> Self {
        Self::default()
    }

    /// 渠道删除后清理两份映射，防止无界增长。
    pub fn forget_channel(&self, channel_id: ChannelId) {
        self.inner.cursors.lock().remove(&channel_id);
        self.inner
            .sticky
            .lock()
            .retain(|(cid, _), _| *cid != channel_id);
    }

    /// 为一次请求选出第一把 key，返回请求作用域的轮转器。
    ///
    /// `identity` 是网关侧身份（API key id）；黏性策略没有身份时自动退化为
    /// 轮询 —— 语义降级而不是报错。
    pub fn start<'a>(
        &self,
        channel_id: ChannelId,
        pool: KeyPool<'a>,
        identity: Option<u64>,
    ) -> KeyRotator<'a> {
        let first_index = self.first_index(channel_id, &pool, identity);
        KeyRotator {
            selector: self.clone(),
            channel_id,
            pool,
            identity,
            index: first_index,
            tried: 1,
            exhausted: false,
        }
    }

    /// 成功路径：把最终使用的 key 提交为黏性绑定（仅黏性策略有意义）。
    ///
    /// 在成功时才提交（而不是在选定时）：失败轮转之后，黏性跟随真正可用的
    /// key，而不是抱着已经 401 的 key 不放。
    pub fn commit_sticky(&self, channel_id: ChannelId, identity: Option<u64>, index: usize) {
        if let Some(identity) = identity {
            self.inner
                .sticky
                .lock()
                .insert((channel_id, identity), index);
        }
    }

    fn first_index(
        &self,
        channel_id: ChannelId,
        pool: &KeyPool<'_>,
        identity: Option<u64>,
    ) -> usize {
        match pool.strategy() {
            KeyStrategy::Sticky => {
                // 先拷出绑定再放锁：if-let 持锁期间不能二次 lock（非可重入）。
                let bound = identity
                    .and_then(|id| self.inner.sticky.lock().get(&(channel_id, id)).copied());
                if let Some(bound) = bound {
                    // 绑定可能来自旧池（管理员删了 key）：越界则失效，走轮询。
                    if bound < pool.len() {
                        return bound;
                    }
                    self.inner
                        .sticky
                        .lock()
                        .remove(&(channel_id, identity.unwrap()));
                }
                self.next_cursor(channel_id, pool.len())
            }
            KeyStrategy::Random => rand::rng().random_range(0..pool.len()),
            KeyStrategy::RoundRobin => self.next_cursor(channel_id, pool.len()),
        }
    }
    fn next_cursor(&self, channel_id: ChannelId, len: usize) -> usize {
        let mut cursors = self.inner.cursors.lock();
        let cursor = cursors.entry(channel_id).or_insert(0);
        let index = (*cursor % len as u64) as usize;
        *cursor = cursor.wrapping_add(1);
        index
    }
}

/// 一次请求内的 key 轮转器：持有池与当前下标，按策略推进。
///
/// 执行器在候选渠道层面消费它：鉴权族失败 → [`Self::rotate`] 换下一把 key
/// 重试同一端点；池耗尽 → [`Self::exhausted`]，把「整池全灭」作为渠道级失败
/// 报给健康度（单 key 的失败不污染渠道健康，避免一把坏 key 把整条渠道停职）。
pub struct KeyRotator<'a> {
    /// 拥有的调度器句柄（Arc 复制）—— 避免与池的生命周期纠缠。
    selector: KeySelector,
    channel_id: ChannelId,
    pool: KeyPool<'a>,
    identity: Option<u64>,
    index: usize,
    /// 本次请求已尝试的 key 数（含当前）—— 达到池长即耗尽。
    tried: usize,
    exhausted: bool,
}
impl<'a> KeyRotator<'a> {
    /// 当前要尝试的 key。
    pub fn current(&self) -> &'a Credential {
        self.pool.key_at(self.index)
    }

    /// 当前 key 的池内下标。
    pub fn index(&self) -> usize {
        self.index
    }

    /// 当前 key 的脱敏提示（写入请求日志，如 `sk-a…9f2c`）。
    pub fn hint(&self) -> String {
        self.current().masked()
    }

    /// 是否为单 key 渠道（无池可轮转）。
    pub fn is_single(&self) -> bool {
        self.pool.is_single()
    }

    /// 鉴权族失败时推进到下一把 key。
    ///
    /// 返回 `true` 表示还有 key 可试；`false` 表示池已耗尽。非鉴权族错误
    /// **不应**调用此方法 —— 与 key 无关的失败轮转只是浪费。
    pub fn rotate(&mut self, kind: ErrorKind) -> bool {
        if self.exhausted {
            return false;
        }
        if !kind.is_key_failure() {
            return false;
        }
        // 池内只有一把 key（含 endpoint 覆盖 / 仅默认 key）：没有可轮转的。
        if self.pool.is_single() || self.pool.len() < 2 {
            self.exhausted = true;
            return false;
        }
        if self.tried >= self.pool.len() {
            // 池内所有 key 都试过了。
            self.exhausted = true;
            return false;
        }
        self.index = (self.index + 1) % self.pool.len();
        self.tried += 1;
        true
    }

    /// 池耗尽后，执行器用它决定「按渠道级失败记录健康」。
    pub fn exhausted(&self) -> bool {
        self.exhausted
    }

    /// 池耗尽时返回聚合错误：把最后一次的错误种类带出去，附加池耗尽语义。
    /// 未真正耗尽（单 key 或非鉴权错误中断）时原样返回。
    pub fn exhausted_error(&self, last: GatewayError) -> GatewayError {
        if !self.exhausted || self.pool.len() < 2 {
            return last;
        }
        let mut error = GatewayError::new(
            last.kind,
            format!(
                "all {} keys in channel credential pool failed: {}",
                self.pool.len(),
                last.message
            ),
        );
        error.retry_after = last.retry_after;
        error
    }

    /// 成功路径：提交黏性绑定，返回最终 key 下标。
    pub fn commit(&self) {
        if matches!(self.pool.strategy(), KeyStrategy::Sticky) {
            self.selector
                .commit_sticky(self.channel_id, self.identity, self.index);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use refract_core::channel::{
        Channel, ChannelEndpoint, ChannelKind, Credential as Cred, KeyStrategy as KS, ModelEntry,
        TranscodePolicy,
    };
    use refract_core::{Protocol, UpstreamAddress};

    fn endpoint() -> ChannelEndpoint {
        ChannelEndpoint {
            protocol: Protocol::Chat,
            order: 0,
            enabled: true,
            address: UpstreamAddress {
                unofficial: true,
                full_address: true,
                base_url: Some("https://api.openai.com/v1".to_owned()),
                version_prefix: None,
                path: None,
            },
            credential: None,
            models: vec![ModelEntry::plain("gpt-4o")],
            transcode: TranscodePolicy::default(),
        }
    }

    fn pooled_channel(keys: &[&str], strategy: KS) -> Channel {
        Channel {
            id: 1,
            owner_id: 1,
            name: "pool-test".to_owned(),
            kind: ChannelKind::Single(Protocol::Chat),
            enabled: true,
            priority: 0,
            weight: 1,
            credential: Cred::new(keys[0]),
            credentials: keys[1..].iter().map(|k| Cred::new(*k)).collect(),
            key_strategy: strategy,
            address: UpstreamAddress::default(),
            endpoints: vec![endpoint()],
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
            visibility: Default::default(),
            user_id: None,
        }
    }

    #[test]
    fn round_robin_cycles_through_pool() {
        let selector = KeySelector::new();
        let channel = pooled_channel(&["k0", "k1", "k2"], KS::RoundRobin);
        let mut seen = Vec::new();
        for _ in 0..6 {
            let pool = channel.key_pool(&channel.endpoints[0]);
            let rotator = selector.start(channel.id, pool, None);
            seen.push(rotator.current().expose().to_owned());
        }
        // 三把 key 各出现两次，且是连续的轮转序列。
        assert_eq!(
            seen,
            vec!["k0", "k1", "k2", "k0", "k1", "k2"],
            "round-robin must cycle the pool in order"
        );
    }

    #[test]
    fn sticky_binds_identity_after_success() {
        let selector = KeySelector::new();
        let channel = pooled_channel(&["k0", "k1", "k2"], KS::Sticky);

        // 第一次：无绑定，走轮询；成功后提交。
        let pool = channel.key_pool(&channel.endpoints[0]);
        let rotator = selector.start(channel.id, pool, Some(7));
        let first = rotator.index();
        rotator.commit();

        // 之后同一身份稳定命中同一把 key。
        for _ in 0..3 {
            let pool = channel.key_pool(&channel.endpoints[0]);
            let rotator = selector.start(channel.id, pool, Some(7));
            assert_eq!(
                rotator.index(),
                first,
                "sticky identity must reuse the committed key"
            );
        }

        // 不同身份不互相影响。
        let pool = channel.key_pool(&channel.endpoints[0]);
        let other = selector.start(channel.id, pool, Some(8));
        other.commit();
        let pool = channel.key_pool(&channel.endpoints[0]);
        let again = selector.start(channel.id, pool, Some(8));
        assert_eq!(again.index(), other.index());
    }

    #[test]
    fn sticky_without_identity_falls_back_to_round_robin() {
        let selector = KeySelector::new();
        let channel = pooled_channel(&["k0", "k1"], KS::Sticky);
        let pool = channel.key_pool(&channel.endpoints[0]);
        let a = selector.start(channel.id, pool, None);
        let pool = channel.key_pool(&channel.endpoints[0]);
        let b = selector.start(channel.id, pool, None);
        assert_ne!(
            a.index(),
            b.index(),
            "without identity sticky must degrade to round-robin, not repeat"
        );
    }

    #[test]
    fn rotate_walks_pool_only_on_auth_failures() {
        let selector = KeySelector::new();
        let channel = pooled_channel(&["k0", "k1", "k2"], KS::RoundRobin);
        let pool = channel.key_pool(&channel.endpoints[0]);
        let mut rotator = selector.start(channel.id, pool, None);

        // 非鉴权错误不轮转。
        assert!(!rotator.rotate(ErrorKind::Timeout));
        assert!(!rotator.rotate(ErrorKind::UpstreamError));

        assert!(rotator.rotate(ErrorKind::Unauthenticated));
        assert_eq!(rotator.current().expose(), "k1");
        assert!(rotator.rotate(ErrorKind::PermissionDenied));
        assert_eq!(rotator.current().expose(), "k2");
        // 最后一把也失败 → 池耗尽。
        assert!(!rotator.rotate(ErrorKind::RateLimited));
        assert!(rotator.exhausted());
    }

    #[test]
    fn single_key_channel_exhausts_immediately() {
        let selector = KeySelector::new();
        let channel = pooled_channel(&["only-key"], KS::RoundRobin);
        let pool = channel.key_pool(&channel.endpoints[0]);
        let mut rotator = selector.start(channel.id, pool, None);
        assert!(rotator.is_single());
        assert!(!rotator.rotate(ErrorKind::Unauthenticated));
        assert!(rotator.exhausted());
        // 单 key 的聚合错误保留原错误，不包装「池耗尽」文案。
        let err = GatewayError::new(ErrorKind::Unauthenticated, "401");
        assert_eq!(rotator.exhausted_error(err).message, "401");
    }

    #[test]
    fn stale_sticky_binding_after_pool_shrink_is_evicted() {
        let selector = KeySelector::new();
        let big = pooled_channel(&["k0", "k1", "k2", "k3"], KS::Sticky);
        let pool = big.key_pool(&big.endpoints[0]);
        let rotator = selector.start(big.id, pool, Some(42));
        // 人为绑定到最后一把。
        selector.commit_sticky(big.id, Some(42), 3);
        let _ = rotator;

        // 管理员删 key 后池变小：旧绑定越界，必须回退轮询而不是 panic。
        let small = pooled_channel(&["k0", "k1"], KS::Sticky);
        let pool = small.key_pool(&small.endpoints[0]);
        let rotator = selector.start(big.id, pool, Some(42));
        assert!(
            rotator.index() < 2,
            "stale binding must be evicted, not reused"
        );
    }

    #[test]
    fn random_strategy_stays_inside_pool() {
        let selector = KeySelector::new();
        let channel = pooled_channel(&["k0", "k1", "k2"], KS::Random);

        // 无状态随机：每次选取都必须在池内，且多次抽取必然出现不止一把 key。
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..60 {
            let pool = channel.key_pool(&channel.endpoints[0]);
            let rotator = selector.start(channel.id, pool, None);
            let key = rotator.current().expose().to_owned();
            assert!(
                ["k0", "k1", "k2"].contains(&key.as_str()),
                "random pick must come from the pool, got {key}"
            );
            seen.insert(key);
        }
        assert!(
            seen.len() > 1,
            "60 random draws over 3 keys must hit more than one"
        );
    }
}
