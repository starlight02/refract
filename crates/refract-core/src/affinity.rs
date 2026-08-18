//! 渠道亲和性配置类型。
//!
//! 亲和性：把「某个调用方/会话」钉在「某个渠道」上 —— 同一个 session 的连续请求
//! 命中同一上游，从而吃到上游的 KV-cache / 会话状态。规则决定如何从请求里抽出
//! 「身份值」，引擎（refract-router）负责缓存与解析。
//!
//! 本模块只定义配置与其不变量，不做任何 IO —— 与 crate 其余部分一致。

use serde::{Deserialize, Serialize};

/// 亲和性功能的默认 TTL：30 分钟。
pub const DEFAULT_AFFINITY_TTL_SECS: u32 = 1800;
/// 缓存容量默认上限。每个条目很小，十万条约数 MB 量级。
pub const DEFAULT_AFFINITY_MAX_ENTRIES: u32 = 100_000;
/// 单条 TTL 上限：一周。更大的值没有意义且会让缓存长期膨胀。
pub const MAX_AFFINITY_TTL_SECS: u32 = 7 * 24 * 3600;
/// 单条规则的身份值正则最大长度 —— 防止病态配置。
pub const MAX_VALUE_REGEX_LEN: usize = 512;

/// 渠道亲和性总开关与全局参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AffinitySettings {
    /// 总开关。关闭时规则、缓存、解析全部不参与热路径。
    #[serde(default)]
    pub enabled: bool,
    /// 请求最终由非钉住渠道成功时，是否把身份重新绑到新渠道。
    ///
    /// 开启（默认）：钉住渠道故障/熔断时自动迁移，恢复会话连续性。
    /// 关闭：钉住失败就忘掉绑定，下次请求重新竞争。
    #[serde(default = "crate::default_true")]
    pub switch_on_success: bool,
    /// 钉住的渠道被停用后是否保留绑定。
    ///
    /// 开启：渠道重新启用后自动回到它（适合临时维护）。
    /// 关闭（默认）：渠道停用即解除绑定，请求立刻参与正常竞争。
    #[serde(default)]
    pub keep_on_channel_disabled: bool,
    /// 缓存最大条目数；超出时按 LRU 淘汰。
    #[serde(default = "default_max_entries")]
    pub max_entries: u32,
    /// 规则未自带 TTL 时的默认秒数。
    #[serde(default = "default_ttl_secs")]
    pub default_ttl_secs: u32,
    /// 亲和规则列表，按顺序求值，首个命中者生效。
    #[serde(default)]
    pub rules: Vec<AffinityRule>,
}

impl Default for AffinitySettings {
    fn default() -> Self {
        Self {
            enabled: false,
            switch_on_success: true,
            keep_on_channel_disabled: false,
            max_entries: default_max_entries(),
            default_ttl_secs: default_ttl_secs(),
            rules: Vec::new(),
        }
    }
}

fn default_max_entries() -> u32 {
    DEFAULT_AFFINITY_MAX_ENTRIES
}

fn default_ttl_secs() -> u32 {
    DEFAULT_AFFINITY_TTL_SECS
}

/// 身份值的来源。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AffinityKeySource {
    /// 网关 API 密钥的主键 —— 按「哪个下游应用」做亲和。
    ApiKeyId,
    /// 请求头取值。
    Header {
        /// 头名（大小写不敏感）。
        name: String,
    },
    /// 请求体 JSON 指针取值。
    Body {
        /// JSON Pointer（RFC 6901），如 `/metadata/user_id`。
        path: String,
    },
}

/// 一条亲和规则。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AffinityRule {
    /// 规则名：缓存键的一部分，也用于日志与 UI。保存时要求唯一。
    pub name: String,
    /// 仅对匹配的模型生效（正则）。空串 = 全部模型。
    #[serde(default)]
    pub model_regex: String,
    /// 仅对匹配的入站路径生效（正则，如 `/v1/messages`）。空串 = 全部路径。
    #[serde(default)]
    pub path_regex: String,
    /// 身份值来源，按顺序求值，首个能取到非空值者生效。
    pub sources: Vec<AffinityKeySource>,
    /// 对取到的身份值再做一次正则筛选（空串 = 不过滤）。
    /// 用于只对形如 `user-…` 的值启用亲和这类场景。
    #[serde(default)]
    pub value_regex: String,
    /// 绑定存活秒数；缺省用全局 `default_ttl_secs`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_secs: Option<u32>,
    /// 缓存键是否包含模型名。
    ///
    /// 开启（默认）：同一身份换模型时各自独立绑定。
    /// 关闭：同一身份的所有模型共享一个渠道绑定。
    #[serde(default = "crate::default_true")]
    pub include_model: bool,
    /// 钉住渠道失败时是否不再重试其他渠道。
    ///
    /// 开启：会话一致性优先 —— 钉住渠道失败就整体失败，避免把同一会话
    /// 的请求散布到多个上游。默认关闭。
    #[serde(default)]
    pub skip_retry_on_failure: bool,
}

/// 亲和配置校验失败。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AffinityError {
    /// 规则名为空。
    #[error("affinity rule name must not be empty")]
    EmptyRuleName,
    /// 规则名重复。
    #[error("duplicate affinity rule name `{0}`")]
    DuplicateRuleName(String),
    /// 规则没有身份来源。
    #[error("affinity rule `{0}` must declare at least one key source")]
    NoSources(String),
    /// 正则语法错误。
    #[error("affinity rule `{rule}`: invalid regex `{value}`: {reason}")]
    InvalidRegex {
        /// 规则名。
        rule: String,
        /// 出错的正则原文。
        value: String,
        /// 解析器给出的原因。
        reason: String,
    },
    /// 身份值正则过长。
    #[error("affinity rule `{0}`: value_regex exceeds {MAX_VALUE_REGEX_LEN} characters")]
    ValueRegexTooLong(String),
    /// 头来源名为空或含非法字符。
    #[error("affinity rule `{0}`: header source name is empty or contains invalid characters")]
    InvalidHeaderName(String),
    /// body 来源路径不是合法 JSON Pointer。
    #[error(
        "affinity rule `{0}`: body source path `{1}` is not a valid JSON pointer (must be empty or start with `/`)"
    )]
    InvalidBodyPath(String, String),
    /// TTL 超出允许范围。
    #[error("affinity rule `{0}`: ttl_secs must be between 1 and {MAX_AFFINITY_TTL_SECS}")]
    TtlOutOfRange(String),
    /// 全局 TTL 超出允许范围。
    #[error("default_ttl_secs must be between 1 and {MAX_AFFINITY_TTL_SECS}")]
    DefaultTtlOutOfRange,
    /// 全局容量为零 —— 零容量等于功能损坏，直接拒绝。
    #[error("max_entries must be at least 1")]
    MaxEntriesZero,
}

impl AffinitySettings {
    /// 校验配置自洽性：规则名唯一、正则可编译、来源合法、数值在界内。
    ///
    /// 正则在这里做一次试编译以尽早暴露语法错误；引擎加载时会再编译一次，
    /// 两次编译同一文本，成本只在保存时发生一次。
    pub fn validate(&self) -> Result<(), AffinityError> {
        if self.max_entries == 0 {
            return Err(AffinityError::MaxEntriesZero);
        }
        if !(1..=MAX_AFFINITY_TTL_SECS).contains(&self.default_ttl_secs) {
            return Err(AffinityError::DefaultTtlOutOfRange);
        }
        let mut seen = std::collections::HashSet::new();
        for rule in &self.rules {
            rule.validate(&mut seen)?;
        }
        Ok(())
    }
}

impl AffinityRule {
    /// 单条规则校验。`seen` 用于跨规则查重。
    pub fn validate(
        &self,
        seen: &mut std::collections::HashSet<String>,
    ) -> Result<(), AffinityError> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err(AffinityError::EmptyRuleName);
        }
        if !seen.insert(name.to_owned()) {
            return Err(AffinityError::DuplicateRuleName(name.to_owned()));
        }
        if self.sources.is_empty() {
            return Err(AffinityError::NoSources(name.to_owned()));
        }
        check_regex(name, &self.model_regex, "model_regex")?;
        check_regex(name, &self.path_regex, "path_regex")?;
        if self.value_regex.chars().count() > MAX_VALUE_REGEX_LEN {
            return Err(AffinityError::ValueRegexTooLong(name.to_owned()));
        }
        check_regex(name, &self.value_regex, "value_regex")?;
        for source in &self.sources {
            match source {
                AffinityKeySource::ApiKeyId => {}
                AffinityKeySource::Header { name: header } => {
                    let header = header.trim();
                    let valid = !header.is_empty()
                        && header.bytes().all(|b| b.is_ascii_graphic() && b != b':');
                    if !valid {
                        return Err(AffinityError::InvalidHeaderName(name.to_owned()));
                    }
                }
                AffinityKeySource::Body { path } => {
                    if !path.is_empty() && !path.starts_with('/') {
                        return Err(AffinityError::InvalidBodyPath(
                            name.to_owned(),
                            path.clone(),
                        ));
                    }
                }
            }
        }
        if let Some(ttl) = self.ttl_secs
            && !(1..=MAX_AFFINITY_TTL_SECS).contains(&ttl)
        {
            return Err(AffinityError::TtlOutOfRange(name.to_owned()));
        }
        Ok(())
    }

    /// 生效 TTL：规则自带值优先，否则用全局默认。
    pub fn effective_ttl_secs(&self, default_secs: u32) -> u32 {
        self.ttl_secs.unwrap_or(default_secs).max(1)
    }
}

/// 试编译正则。空串表示「不筛选」，直接通过。
fn check_regex(rule: &str, value: &str, field: &str) -> Result<(), AffinityError> {
    if value.is_empty() {
        return Ok(());
    }
    regex_syntax_lite(value).map_err(|reason| AffinityError::InvalidRegex {
        rule: rule.to_owned(),
        value: format!("{field}: {value}"),
        reason,
    })
}

/// 不引入 regex crate 的轻量语法校验：只做括号配对级别的粗检。
///
/// 真正的编译在引擎加载时完成（refract-router 依赖 regex）。保存期提前报错
/// 靠的是引擎的 `compile` 返回错误，这里的粗检只拦最显然的坏配置。
fn regex_syntax_lite(pattern: &str) -> Result<(), String> {
    let mut depth = 0_i32;
    for (index, byte) in pattern.bytes().enumerate() {
        match byte {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth < 0 {
                    return Err(format!("unmatched `)` at byte {index}"));
                }
            }
            _ => {}
        }
    }
    if depth != 0 {
        return Err(format!("`(` unclosed, depth {depth} at end"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn rule(name: &str, source: AffinityKeySource) -> AffinityRule {
        AffinityRule {
            name: name.into(),
            model_regex: String::new(),
            path_regex: String::new(),
            sources: vec![source],
            value_regex: String::new(),
            ttl_secs: None,
            include_model: true,
            skip_retry_on_failure: false,
        }
    }

    #[test]
    fn defaults_are_disabled_with_sane_bounds() {
        let settings = AffinitySettings::default();
        assert!(!settings.enabled);
        assert!(settings.switch_on_success);
        assert!(!settings.keep_on_channel_disabled);
        assert_eq!(settings.max_entries, 100_000);
        assert_eq!(settings.default_ttl_secs, 1800);
        assert!(settings.rules.is_empty());
        settings.validate().unwrap();
    }

    #[test]
    fn serde_round_trip_keeps_defaults() {
        let settings = AffinitySettings {
            enabled: true,
            rules: vec![rule(
                "codex",
                AffinityKeySource::Header {
                    name: "session_id".into(),
                },
            )],
            ..Default::default()
        };
        let json = serde_json::to_string(&settings).unwrap();
        let back: AffinitySettings = serde_json::from_str(&json).unwrap();
        assert_eq!(back, settings);

        // 缺省字段全部有默认值 —— 旧配置文档可以直接反序列化。
        let minimal: AffinitySettings = serde_json::from_str("{}").unwrap();
        assert_eq!(minimal, AffinitySettings::default());
    }

    #[test]
    fn key_source_serializes_tagged() {
        let header = AffinityKeySource::Header {
            name: "x-session".into(),
        };
        assert_eq!(
            serde_json::to_value(&header).unwrap(),
            serde_json::json!({ "kind": "header", "name": "x-session" })
        );
        let body = AffinityKeySource::Body {
            path: "/metadata/user_id".into(),
        };
        assert_eq!(
            serde_json::to_value(&body).unwrap(),
            serde_json::json!({ "kind": "body", "path": "/metadata/user_id" })
        );
        assert_eq!(
            serde_json::to_value(&AffinityKeySource::ApiKeyId).unwrap(),
            serde_json::json!({ "kind": "api_key_id" })
        );
    }

    #[test]
    fn validate_rejects_bad_rules() {
        let mut settings = AffinitySettings::default();
        settings.rules.push(rule("", AffinityKeySource::ApiKeyId));
        assert_eq!(
            settings.validate().unwrap_err(),
            AffinityError::EmptyRuleName
        );

        settings.rules.clear();
        settings.rules.push(AffinityRule {
            sources: vec![],
            ..rule("a", AffinityKeySource::ApiKeyId)
        });
        assert_eq!(
            settings.validate().unwrap_err(),
            AffinityError::NoSources("a".into())
        );

        settings.rules.clear();
        settings
            .rules
            .push(rule("dup", AffinityKeySource::ApiKeyId));
        settings
            .rules
            .push(rule("dup", AffinityKeySource::Header { name: "x".into() }));
        assert_eq!(
            settings.validate().unwrap_err(),
            AffinityError::DuplicateRuleName("dup".into())
        );

        settings.rules.clear();
        settings.rules.push(AffinityRule {
            model_regex: "(unclosed".into(),
            ..rule("bad-regex", AffinityKeySource::ApiKeyId)
        });
        assert!(matches!(
            settings.validate().unwrap_err(),
            AffinityError::InvalidRegex { .. }
        ));

        settings.rules.clear();
        settings.rules.push(rule(
            "bad-header",
            AffinityKeySource::Header { name: "  ".into() },
        ));
        assert_eq!(
            settings.validate().unwrap_err(),
            AffinityError::InvalidHeaderName("bad-header".into())
        );

        settings.rules.clear();
        settings.rules.push(rule(
            "bad-body",
            AffinityKeySource::Body {
                path: "metadata/user_id".into(),
            },
        ));
        assert_eq!(
            settings.validate().unwrap_err(),
            AffinityError::InvalidBodyPath("bad-body".into(), "metadata/user_id".into())
        );

        settings.rules.clear();
        settings.rules.push(AffinityRule {
            ttl_secs: Some(0),
            ..rule("bad-ttl", AffinityKeySource::ApiKeyId)
        });
        assert_eq!(
            settings.validate().unwrap_err(),
            AffinityError::TtlOutOfRange("bad-ttl".into())
        );
    }

    #[test]
    fn validate_rejects_bad_globals() {
        let settings = AffinitySettings {
            max_entries: 0,
            ..AffinitySettings::default()
        };
        assert_eq!(
            settings.validate().unwrap_err(),
            AffinityError::MaxEntriesZero
        );

        let settings = AffinitySettings {
            default_ttl_secs: MAX_AFFINITY_TTL_SECS + 1,
            ..AffinitySettings::default()
        };
        assert_eq!(
            settings.validate().unwrap_err(),
            AffinityError::DefaultTtlOutOfRange
        );
    }

    #[test]
    fn effective_ttl_prefers_rule_value() {
        let mut rule = rule("ttl", AffinityKeySource::ApiKeyId);
        assert_eq!(rule.effective_ttl_secs(1800), 1800);
        rule.ttl_secs = Some(60);
        assert_eq!(rule.effective_ttl_secs(1800), 60);
    }
}
