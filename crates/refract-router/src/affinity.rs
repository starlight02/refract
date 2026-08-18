//! 渠道亲和性引擎：把「身份值」钉在「渠道」上的内存缓存。
//!
//! 热路径语义（对齐 new-api 的 ChannelAffinity）：
//! 1. **resolve**（路由前）：按规则顺序求值，首个能抽出身份值的规则决定本次
//!    请求的缓存键；命中缓存 → 返回绑定的渠道，网关把它钉到候选首位。
//! 2. **record**（成功后）：首次绑定必写；已有绑定但赢家是别的渠道时，只有
//!    `switch_on_success` 开着才改绑。每次成功都刷新 TTL —— 活跃会话的绑定
//!    不该在对话中途过期。
//! 3. **forget**（失败后）：钉住渠道失败就解除绑定，让请求参与正常竞争 ——
//!    除非规则声明 `skip_retry_on_failure`（会话一致性优先，绑定保留）。
//!
//! 缓存是 TTL + 容量上限的哈希表：超限时先清过期项，仍满则淘汰最久未访问的
//! 条目。没有引入外部缓存 crate —— 条目只是一个 `ChannelId` 加两个时间戳，
//! 手写淘汰比拉依赖更可控。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;
use refract_core::ChannelId;
use refract_core::affinity::{AffinityKeySource, AffinityRule, AffinitySettings};
use regex::Regex;
use serde::Serialize;
use serde_json::Value;

/// 一条规则编译后的形态。正则编译失败（core 只做轻量括号检查）的规则在
/// 装载时被丢弃并记日志 —— 病态配置不能拖垮整个亲和功能。
#[derive(Debug)]
struct CompiledRule {
    name: String,
    model_re: Option<Regex>,
    path_re: Option<Regex>,
    sources: Vec<AffinityKeySource>,
    value_re: Option<Regex>,
    ttl_secs: u32,
    include_model: bool,
    skip_retry_on_failure: bool,
}

#[derive(Debug)]
struct Entry {
    channel_id: ChannelId,
    expires_at: Instant,
    last_used: Instant,
}

/// 引擎内部状态：规则、缓存、统计。
#[derive(Debug, Default)]
struct Inner {
    enabled: bool,
    switch_on_success: bool,
    keep_on_channel_disabled: bool,
    rules: Vec<CompiledRule>,
    max_entries: usize,
    entries: HashMap<String, Entry>,
    stats: AffinityStats,
}

/// 亲和性运行统计。全部是累计计数（进程生命周期内）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct AffinityStats {
    /// 缓存命中次数（取到了有效绑定）。
    pub hits: u64,
    /// 规则匹配出身份值但无绑定/绑定过期的次数。
    pub misses: u64,
    /// 写入/刷新绑定的次数。
    pub records: u64,
    /// 主动解除绑定的次数（失败清除、渠道删除）。
    pub forgets: u64,
    /// 因容量或过期被清理的条目数。
    pub evictions: u64,
    /// 缓存中当前条目数。
    pub entries: u64,
}

/// resolve 的输入。全部借用，零拷贝。
///
/// `body` 是**懒解析**的：网关只在确有规则需要 Body 来源时才解析请求体，
/// 不需要时这里传 `None`，热路径不付 JSON 解析的成本。
#[derive(Debug)]
pub struct AffinityContext<'a> {
    /// 客户端请求的模型名。
    pub model: &'a str,
    /// 入站请求路径（如 `/v1/chat/completions`）。
    pub path: &'a str,
    /// 网关 API key 的主键（`ApiKeyId` 来源）。
    pub api_key_id: Option<u64>,
    /// 入站请求头。
    pub headers: &'a http::HeaderMap,
    /// 已解析的请求体 JSON（若有且需要）。
    pub body: Option<&'a Value>,
}

/// resolve 的结果：规则已匹配出身份值。
#[derive(Debug, Clone)]
pub struct AffinityDecision {
    /// 命中规则的名字（写请求日志用）。
    pub rule_name: String,
    /// 缓存键 —— record/forget 用它定位绑定。
    pub cache_key: String,
    /// 现有绑定（`None` = 身份匹配但尚未绑定）。
    pub binding: Option<ChannelId>,
    /// 该规则是否要求钉住失败时不再重试其他渠道。
    pub skip_retry_on_failure: bool,
    /// 生效 TTL（秒），record 时用。
    pub ttl_secs: u32,
}

/// 渠道亲和性引擎。`Clone` 是廉价的 Arc 复制。
#[derive(Debug, Clone, Default)]
pub struct AffinityEngine {
    inner: Arc<Mutex<Inner>>,
}

impl AffinityEngine {
    /// 空引擎：未加载任何规则，热路径恒短路。
    pub fn new() -> Self {
        Self::default()
    }

    /// 用新配置重建规则集。**缓存条目跨重载保留** —— 管理员改一条规则
    /// 不应把所有会话的绑定清零。
    pub fn load(&self, settings: AffinitySettings) {
        let mut rules = Vec::with_capacity(settings.rules.len());
        for rule in &settings.rules {
            match compile_rule(rule, settings.default_ttl_secs) {
                Ok(compiled) => rules.push(compiled),
                Err(error) => {
                    // 病态规则只伤自己：记日志、跳过，其余规则照常生效。
                    tracing::warn!(
                        rule = %rule.name,
                        error = %error,
                        "affinity rule failed to compile; rule disabled"
                    );
                }
            }
        }
        let mut inner = self.inner.lock();
        inner.enabled = settings.enabled;
        inner.switch_on_success = settings.switch_on_success;
        inner.keep_on_channel_disabled = settings.keep_on_channel_disabled;
        inner.max_entries = settings.max_entries as usize;
        inner.rules = rules;
    }

    /// 总开关与规则集是否使亲和功能实际参与热路径。
    pub fn is_active(&self) -> bool {
        let inner = self.inner.lock();
        inner.enabled && !inner.rules.is_empty()
    }

    /// 是否有规则需要 Body 来源 —— 网关只在为真时才解析请求体 JSON，
    /// 其余请求的热路径不付解析成本。
    pub fn needs_body(&self) -> bool {
        let inner = self.inner.lock();
        inner.enabled
            && inner.rules.iter().any(|rule| {
                rule.sources
                    .iter()
                    .any(|s| matches!(s, refract_core::AffinityKeySource::Body { .. }))
            })
    }

    /// 成功路径的全局开关。
    pub fn switch_on_success(&self) -> bool {
        self.inner.lock().switch_on_success
    }

    /// 渠道停用路径的全局开关。
    pub fn keep_on_channel_disabled(&self) -> bool {
        self.inner.lock().keep_on_channel_disabled
    }

    /// 路由前解析：返回身份匹配结果与现有绑定（若有）。
    ///
    /// 没有规则匹配、或所有来源都取不到值时返回 `None` —— 请求走普通竞争。
    pub fn resolve(&self, ctx: &AffinityContext<'_>) -> Option<AffinityDecision> {
        let mut inner = self.inner.lock();
        if !inner.enabled {
            return None;
        }
        // 第一步：规则匹配，产出所有需要的值 —— 之后不再借用 rules，
        // 才能安全地改写 entries/stats。
        let matched = inner.rules.iter().find_map(|rule| {
            if let Some(model_re) = &rule.model_re
                && !model_re.is_match(ctx.model)
            {
                return None;
            }
            if let Some(path_re) = &rule.path_re
                && !path_re.is_match(ctx.path)
            {
                return None;
            }
            let value = extract_value(&rule.sources, ctx)?;
            if let Some(value_re) = &rule.value_re
                && !value_re.is_match(&value)
            {
                return None;
            }
            Some(RuleMatch {
                rule_name: rule.name.clone(),
                cache_key: build_cache_key(&rule.name, rule.include_model, ctx.model, &value),
                skip_retry_on_failure: rule.skip_retry_on_failure,
                ttl_secs: rule.ttl_secs,
            })
        });
        let m = matched?;
        // 第二步：缓存查询/续命/清理。先拷贝条目数据再改状态，
        // 避免同时持有 entries 的借用与 stats 的可变借用。
        let cached = inner
            .entries
            .get(&m.cache_key)
            .map(|entry| (entry.channel_id, entry.expires_at > Instant::now()));
        match cached {
            Some((binding, true)) => {
                inner.stats.hits += 1;
                // 命中即续命：滑动过期，活跃会话不掉线。
                if let Some(entry) = inner.entries.get_mut(&m.cache_key) {
                    entry.last_used = Instant::now();
                    entry.expires_at =
                        Instant::now() + std::time::Duration::from_secs(u64::from(m.ttl_secs));
                }
                Some(AffinityDecision {
                    rule_name: m.rule_name,
                    cache_key: m.cache_key,
                    binding: Some(binding),
                    skip_retry_on_failure: m.skip_retry_on_failure,
                    ttl_secs: m.ttl_secs,
                })
            }
            Some((_, false)) => {
                // 过期条目就地清理。
                inner.entries.remove(&m.cache_key);
                inner.stats.evictions += 1;
                inner.stats.misses += 1;
                Some(AffinityDecision {
                    rule_name: m.rule_name,
                    cache_key: m.cache_key,
                    binding: None,
                    skip_retry_on_failure: m.skip_retry_on_failure,
                    ttl_secs: m.ttl_secs,
                })
            }
            None => {
                inner.stats.misses += 1;
                Some(AffinityDecision {
                    rule_name: m.rule_name,
                    cache_key: m.cache_key,
                    binding: None,
                    skip_retry_on_failure: m.skip_retry_on_failure,
                    ttl_secs: m.ttl_secs,
                })
            }
        }
    }

    /// 成功后写入/刷新绑定。
    ///
    /// 语义对齐 new-api：无绑定 → 必写；绑定的就是赢家 → 刷新 TTL；
    /// 绑定的是别人 → 仅 `switch_on_success` 时改绑。
    pub fn record(&self, decision: &AffinityDecision, winner: ChannelId) {
        let mut inner = self.inner.lock();
        // 依据锁内的当前条目判断，而不是 resolve 时的陈旧 binding：
        // 决策产生后可能已有并发请求先写入过绑定。
        let existing = inner.entries.get(&decision.cache_key).map(|e| e.channel_id);
        match existing {
            None => {}
            Some(bound) if bound == winner => {}
            Some(_) if !inner.switch_on_success => return,
            Some(_) => {}
        }
        if inner.entries.len() >= inner.max_entries {
            evict_expired_or_oldest(&mut inner);
        }
        let now = Instant::now();
        inner.entries.insert(
            decision.cache_key.clone(),
            Entry {
                channel_id: winner,
                expires_at: now + std::time::Duration::from_secs(u64::from(decision.ttl_secs)),
                last_used: now,
            },
        );
        inner.stats.records += 1;
    }

    /// 解除一个绑定（钉住渠道失败且允许重试时）。
    pub fn forget(&self, cache_key: &str) {
        let mut inner = self.inner.lock();
        if inner.entries.remove(cache_key).is_some() {
            inner.stats.forgets += 1;
        }
    }

    /// 仅当绑定指向指定渠道时才解除（`switch_on_success` 关着时，
    /// 兜底成功可能已把绑定改到别的渠道，不能误伤）。
    pub fn forget_if_bound_to(&self, cache_key: &str, channel_id: ChannelId) {
        let mut inner = self.inner.lock();
        let matches = inner
            .entries
            .get(cache_key)
            .is_some_and(|entry| entry.channel_id == channel_id);
        if matches && inner.entries.remove(cache_key).is_some() {
            inner.stats.forgets += 1;
        }
    }

    /// 解除指向某渠道的所有绑定（渠道被删除时）。
    pub fn forget_channel(&self, channel_id: ChannelId) {
        let mut inner = self.inner.lock();
        let before = inner.entries.len();
        inner
            .entries
            .retain(|_, entry| entry.channel_id != channel_id);
        inner.stats.forgets += (before - inner.entries.len()) as u64;
    }

    /// 清空全部绑定（管理接口）。返回被清除的条目数。
    pub fn clear(&self) -> u64 {
        let mut inner = self.inner.lock();
        let n = inner.entries.len() as u64;
        inner.entries.clear();
        inner.stats.forgets += n;
        n
    }

    /// 运行统计快照。
    pub fn stats(&self) -> AffinityStats {
        let mut inner = self.inner.lock();
        // 顺手清掉已过期项，让 `entries` 反映真实占用。
        let now = Instant::now();
        let before = inner.entries.len();
        inner.entries.retain(|_, e| e.expires_at > now);
        inner.stats.evictions += (before - inner.entries.len()) as u64;
        inner.stats.entries = inner.entries.len() as u64;
        inner.stats
    }
}

/// 规则匹配阶段的所有权产出：缓存键与规则参数，供缓存查询/写入使用。
struct RuleMatch {
    rule_name: String,
    cache_key: String,
    skip_retry_on_failure: bool,
    ttl_secs: u32,
}

/// 缓存键：`规则名 \0 [模型] \0 身份值`。
///
/// 用 NUL 分隔而不是格式化字符串 —— 这三个分量里出现 NUL 的概率为零
/// （身份值来自 HTTP 头/JSON 字符串，规则名在保存时已校验），避免了
/// 分隔符歧义导致的键碰撞。
fn build_cache_key(rule_name: &str, include_model: bool, model: &str, value: &str) -> String {
    if include_model {
        format!("{rule_name}\u{0}{model}\u{0}{value}")
    } else {
        format!("{rule_name}\u{0}\u{0}{value}")
    }
}

/// 按来源顺序抽取身份值；首个非空者胜出。
fn extract_value(sources: &[AffinityKeySource], ctx: &AffinityContext<'_>) -> Option<String> {
    for source in sources {
        let value = match source {
            AffinityKeySource::ApiKeyId => ctx.api_key_id.map(|id| id.to_string()),
            AffinityKeySource::Header { name } => ctx
                .headers
                .get(name.as_str())
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
                .filter(|s| !s.is_empty()),
            AffinityKeySource::Body { path } => {
                ctx.body.and_then(|body| json_pointer_scalar(body, path))
            }
        };
        if value.is_some() {
            return value;
        }
    }
    None
}

/// 按 RFC 6901 JSON Pointer 取标量值。
///
/// 只接受字符串/数字/布尔 —— 对象与数组做身份值没有意义。数字/布尔转成
/// 字符串形式（对齐 new-api 的 gjson 取值行为）。
fn json_pointer_scalar(body: &Value, pointer: &str) -> Option<String> {
    if pointer.is_empty() {
        return None;
    }
    let value = body.pointer(pointer)?;
    match value {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn compile_rule(rule: &AffinityRule, default_ttl_secs: u32) -> Result<CompiledRule, regex::Error> {
    let compile = |text: &str| -> Result<Option<Regex>, regex::Error> {
        if text.is_empty() {
            Ok(None)
        } else {
            Regex::new(text).map(Some)
        }
    };
    Ok(CompiledRule {
        name: rule.name.clone(),
        model_re: compile(&rule.model_regex)?,
        path_re: compile(&rule.path_regex)?,
        sources: rule.sources.clone(),
        value_re: compile(&rule.value_regex)?,
        ttl_secs: rule.effective_ttl_secs(default_ttl_secs),
        include_model: rule.include_model,
        skip_retry_on_failure: rule.skip_retry_on_failure,
    })
}

/// 容量超限时的清理：先扫过期项；仍满则淘汰最久未访问的条目。
/// O(n) 扫描只发生在写入撞上限的罕见时刻，不影响常态热路径。
fn evict_expired_or_oldest(inner: &mut Inner) {
    let now = Instant::now();
    let before = inner.entries.len();
    inner.entries.retain(|_, e| e.expires_at > now);
    inner.stats.evictions += (before - inner.entries.len()) as u64;
    if inner.entries.len() >= inner.max_entries
        && let Some(oldest) = inner
            .entries
            .iter()
            .min_by_key(|(_, e)| e.last_used)
            .map(|(k, _)| k.clone())
    {
        inner.entries.remove(&oldest);
        inner.stats.evictions += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use refract_core::affinity::{AffinityRule, AffinitySettings};

    fn engine_with_session_rule() -> AffinityEngine {
        let engine = AffinityEngine::new();
        let settings = AffinitySettings {
            enabled: true,
            rules: vec![AffinityRule {
                name: "codex-session".into(),
                model_regex: String::new(),
                path_regex: String::new(),
                sources: vec![AffinityKeySource::Header {
                    name: "session_id".into(),
                }],
                value_regex: String::new(),
                ttl_secs: Some(60),
                include_model: true,
                skip_retry_on_failure: false,
            }],
            ..AffinitySettings::default()
        };
        engine.load(settings);
        engine
    }

    fn ctx<'a>(
        model: &'a str,
        headers: &'a http::HeaderMap,
        body: Option<&'a Value>,
    ) -> AffinityContext<'a> {
        AffinityContext {
            model,
            path: "/v1/chat/completions",
            api_key_id: Some(1),
            headers,
            body,
        }
    }

    #[test]
    fn resolve_miss_then_record_then_hit() {
        let engine = engine_with_session_rule();
        let mut headers = http::HeaderMap::new();
        headers.insert("session_id", "sess-42".parse().unwrap());

        let decision = engine
            .resolve(&ctx("gpt-5", &headers, None))
            .expect("rule must match");
        assert_eq!(decision.binding, None, "first request has no binding yet");

        engine.record(&decision, 7);

        let again = engine
            .resolve(&ctx("gpt-5", &headers, None))
            .expect("rule must match");
        assert_eq!(again.binding, Some(7), "same session must pin");
        assert_eq!(again.cache_key, decision.cache_key);

        // 不同 session 不共享绑定。
        let mut other = http::HeaderMap::new();
        other.insert("session_id", "sess-43".parse().unwrap());
        let d = engine.resolve(&ctx("gpt-5", &other, None)).unwrap();
        assert_eq!(d.binding, None);
    }

    #[test]
    fn include_model_false_shares_binding_across_models() {
        let engine = AffinityEngine::new();
        let settings = AffinitySettings {
            enabled: true,
            rules: vec![AffinityRule {
                name: "per-user".into(),
                model_regex: String::new(),
                path_regex: String::new(),
                sources: vec![AffinityKeySource::ApiKeyId],
                value_regex: String::new(),
                ttl_secs: None,
                include_model: false,
                skip_retry_on_failure: false,
            }],
            ..AffinitySettings::default()
        };
        engine.load(settings);

        let empty = http::HeaderMap::new();
        let d1 = engine.resolve(&ctx("model-a", &empty, None)).unwrap();
        engine.record(&d1, 3);
        let d2 = engine.resolve(&ctx("model-b", &empty, None)).unwrap();
        assert_eq!(
            d2.binding,
            Some(3),
            "include_model=false must share one binding across models"
        );
    }

    #[test]
    fn switch_on_success_false_keeps_original_binding() {
        let engine = AffinityEngine::new();
        let settings = AffinitySettings {
            enabled: true,
            switch_on_success: false,
            rules: vec![AffinityRule {
                name: "r".into(),
                model_regex: String::new(),
                path_regex: String::new(),
                sources: vec![AffinityKeySource::ApiKeyId],
                value_regex: String::new(),
                ttl_secs: None,
                include_model: true,
                skip_retry_on_failure: false,
            }],
            ..AffinitySettings::default()
        };
        engine.load(settings);

        let empty = http::HeaderMap::new();
        let d = engine.resolve(&ctx("m", &empty, None)).unwrap();
        engine.record(&d, 1);
        // 钉住渠道故障、别的渠道成功：switch_on_success=false 不改绑。
        engine.record(&d, 2);
        let d2 = engine.resolve(&ctx("m", &empty, None)).unwrap();
        assert_eq!(d2.binding, Some(1));
    }

    #[test]
    fn switch_on_success_true_rebinds() {
        let engine = engine_with_session_rule();
        let mut headers = http::HeaderMap::new();
        headers.insert("session_id", "s".parse().unwrap());

        let d = engine.resolve(&ctx("m", &headers, None)).unwrap();
        engine.record(&d, 1);
        // 模拟钉住失败后由渠道 2 兜底成功。
        let d2 = engine.resolve(&ctx("m", &headers, None)).unwrap();
        assert_eq!(d2.binding, Some(1));
        engine.record(&d2, 2);
        let d3 = engine.resolve(&ctx("m", &headers, None)).unwrap();
        assert_eq!(d3.binding, Some(2));
    }

    #[test]
    fn forget_releases_binding() {
        let engine = engine_with_session_rule();
        let mut headers = http::HeaderMap::new();
        headers.insert("session_id", "s".parse().unwrap());
        let d = engine.resolve(&ctx("m", &headers, None)).unwrap();
        engine.record(&d, 9);
        engine.forget(&d.cache_key);
        let d2 = engine.resolve(&ctx("m", &headers, None)).unwrap();
        assert_eq!(d2.binding, None);
        assert_eq!(engine.stats().forgets, 1);
    }

    #[test]
    fn disabled_engine_resolves_nothing() {
        let engine = AffinityEngine::new();
        let settings = AffinitySettings {
            enabled: false,
            rules: vec![AffinityRule {
                name: "r".into(),
                model_regex: String::new(),
                path_regex: String::new(),
                sources: vec![AffinityKeySource::ApiKeyId],
                value_regex: String::new(),
                ttl_secs: None,
                include_model: true,
                skip_retry_on_failure: false,
            }],
            ..AffinitySettings::default()
        };
        engine.load(settings);
        assert!(!engine.is_active());
        let empty = http::HeaderMap::new();
        assert!(engine.resolve(&ctx("m", &empty, None)).is_none());
    }

    #[test]
    fn bad_regex_rule_is_dropped_not_fatal() {
        let engine = AffinityEngine::new();
        let settings = AffinitySettings {
            enabled: true,
            rules: vec![
                AffinityRule {
                    name: "broken".into(),
                    model_regex: "(".into(), // 病态正则
                    path_regex: String::new(),
                    sources: vec![AffinityKeySource::ApiKeyId],
                    value_regex: String::new(),
                    ttl_secs: None,
                    include_model: true,
                    skip_retry_on_failure: false,
                },
                AffinityRule {
                    name: "ok".into(),
                    model_regex: String::new(),
                    path_regex: String::new(),
                    sources: vec![AffinityKeySource::ApiKeyId],
                    value_regex: String::new(),
                    ttl_secs: None,
                    include_model: true,
                    skip_retry_on_failure: false,
                },
            ],
            ..AffinitySettings::default()
        };
        engine.load(settings);
        let empty = http::HeaderMap::new();
        let d = engine.resolve(&ctx("m", &empty, None)).unwrap();
        assert_eq!(d.rule_name, "ok", "broken rule must be skipped, not fatal");
    }

    #[test]
    fn body_pointer_source_extracts_scalar() {
        let engine = AffinityEngine::new();
        let settings = AffinitySettings {
            enabled: true,
            rules: vec![AffinityRule {
                name: "anthropic-user".into(),
                model_regex: String::new(),
                path_regex: String::new(),
                sources: vec![AffinityKeySource::Body {
                    path: "/metadata/user_id".into(),
                }],
                value_regex: String::new(),
                ttl_secs: None,
                include_model: true,
                skip_retry_on_failure: false,
            }],
            ..AffinitySettings::default()
        };
        engine.load(settings);

        let body: Value = serde_json::json!({"metadata": {"user_id": "user-123"}});
        let empty = http::HeaderMap::new();
        let d = engine
            .resolve(&ctx("m", &empty, Some(&body)))
            .expect("body source must match");
        engine.record(&d, 5);

        let d2 = engine.resolve(&ctx("m", &empty, Some(&body))).unwrap();
        assert_eq!(d2.binding, Some(5));

        // 无 body 时该规则取不到值 → 无决策。
        assert!(engine.resolve(&ctx("m", &empty, None)).is_none());
    }

    #[test]
    fn model_and_path_regex_gate_rules() {
        let engine = AffinityEngine::new();
        let settings = AffinitySettings {
            enabled: true,
            rules: vec![AffinityRule {
                name: "gpt-only".into(),
                model_regex: "^gpt-".into(),
                path_regex: "^/v1/chat".into(),
                sources: vec![AffinityKeySource::ApiKeyId],
                value_regex: String::new(),
                ttl_secs: None,
                include_model: true,
                skip_retry_on_failure: false,
            }],
            ..AffinitySettings::default()
        };
        engine.load(settings);
        let empty = http::HeaderMap::new();

        assert!(
            engine.resolve(&ctx("claude-x", &empty, None)).is_none(),
            "model gate"
        );
        assert!(
            engine.resolve(&ctx("gpt-5", &empty, None)).is_some(),
            "matching model passes"
        );
    }

    #[test]
    fn capacity_evicts_oldest_used() {
        let engine = AffinityEngine::new();
        let settings = AffinitySettings {
            enabled: true,
            max_entries: 2,
            rules: vec![AffinityRule {
                name: "r".into(),
                model_regex: String::new(),
                path_regex: String::new(),
                sources: vec![AffinityKeySource::ApiKeyId],
                value_regex: String::new(),
                ttl_secs: None,
                include_model: true,
                skip_retry_on_failure: false,
            }],
            ..AffinitySettings::default()
        };
        engine.load(settings);
        let empty = http::HeaderMap::new();

        // 三个不同身份，容量 2：最旧的必须被淘汰。
        for model in ["a", "b", "c"] {
            let d = engine.resolve(&ctx(model, &empty, None)).unwrap();
            engine.record(&d, 1);
        }
        let stats = engine.stats();
        assert_eq!(stats.entries, 2, "capacity must cap the cache");
        assert_eq!(stats.evictions, 1);
    }

    #[test]
    fn json_pointer_handles_rfc6901_escapes_and_scalars() {
        let body: Value = serde_json::json!({
            "a/b": {"~tilde": 42},
            "flag": true,
            "obj": {"x": 1},
            "null": null
        });
        assert_eq!(
            json_pointer_scalar(&body, "/a~1b/~0tilde").as_deref(),
            Some("42")
        );
        assert_eq!(json_pointer_scalar(&body, "/flag").as_deref(), Some("true"));
        assert_eq!(
            json_pointer_scalar(&body, "/obj"),
            None,
            "objects are not identity values"
        );
        assert_eq!(json_pointer_scalar(&body, "/null"), None);
        assert_eq!(
            json_pointer_scalar(&body, ""),
            None,
            "empty pointer rejected"
        );
        assert_eq!(json_pointer_scalar(&body, "/missing"), None);
    }

    #[test]
    fn expired_binding_is_treated_as_miss() {
        let engine = AffinityEngine::new();
        let settings = AffinitySettings {
            enabled: true,
            rules: vec![AffinityRule {
                name: "short-lived".into(),
                model_regex: String::new(),
                path_regex: String::new(),
                sources: vec![AffinityKeySource::Header {
                    name: "session_id".into(),
                }],
                value_regex: String::new(),
                // TTL 会被钳制到至少 1 秒：等它自然过期即可。
                ttl_secs: Some(1),
                include_model: true,
                skip_retry_on_failure: false,
            }],
            ..AffinitySettings::default()
        };
        engine.load(settings);

        let mut headers = http::HeaderMap::new();
        headers.insert("session_id", "sess-x".parse().unwrap());
        let d = engine.resolve(&ctx("gpt-5", &headers, None)).unwrap();
        engine.record(&d, 7);

        std::thread::sleep(std::time::Duration::from_millis(1100));
        let again = engine.resolve(&ctx("gpt-5", &headers, None)).unwrap();
        assert_eq!(again.binding, None, "expired binding must not pin");
    }

    #[test]
    fn forget_if_bound_to_only_releases_matching_channel() {
        let engine = engine_with_session_rule();
        let mut headers = http::HeaderMap::new();
        headers.insert("session_id", "sess-9".parse().unwrap());
        let d = engine.resolve(&ctx("gpt-5", &headers, None)).unwrap();
        engine.record(&d, 7);

        // 兜底已把绑定改到别的渠道时，不能误删仍指向 7 的新绑定。
        engine.forget_if_bound_to(&d.cache_key, 9);
        let still = engine.resolve(&ctx("gpt-5", &headers, None)).unwrap();
        assert_eq!(still.binding, Some(7));

        // 绑定确实指向 7 才释放。
        engine.forget_if_bound_to(&d.cache_key, 7);
        let released = engine.resolve(&ctx("gpt-5", &headers, None)).unwrap();
        assert_eq!(released.binding, None);
    }
}
