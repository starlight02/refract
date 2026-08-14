//! 上游地址构造。
//!
//! 需求 2 与需求 3 的核心：渠道（以及聚合渠道的每个协议端点）可以自定义
//! base URL、版本前缀、路径，三者拼接成完整请求地址；也可以开启「完整地址」
//! 模式直接指定最终 URL，跳过拼接与校验。
//!
//! 这个模块与渠道类型正交 —— 它只关心「给定协议，算出该打哪个 URL」。

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::protocol::Protocol;

/// Gemini 路径中的模型占位符。
pub const MODEL_PLACEHOLDER: &str = "{model}";
/// Gemini 路径中的动作占位符（`generateContent` / `streamGenerateContent`）。
pub const ACTION_PLACEHOLDER: &str = "{action}";

/// 上游地址的构造规则。
///
/// # 解析优先级
///
/// | `unofficial` | `full_address` | 行为 |
/// |---|---|---|
/// | `false` | — | 协议官方默认地址，忽略所有自定义字段 |
/// | `true` | `false` | `base_url + version_prefix + path`，缺省段回落到协议默认值，拼接后做协议校验 |
/// | `true` | `true` | `base_url` 原样作为最终 URL，不拼接、不校验 |
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UpstreamAddress {
    /// 非官方开关。关闭时（默认）一律使用协议官方地址。
    pub unofficial: bool,
    /// 完整地址开关。仅在 `unofficial` 为真时有意义。
    pub full_address: bool,
    /// 基础地址，如 `https://api.example.com`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// 版本前缀，如 `/v1`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_prefix: Option<String>,
    /// 端点路径，如 `/chat/completions`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// 请求的目标动作，决定 Gemini 的 URL 后缀。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// 一次性返回完整响应。
    Generate,
    /// 流式返回。
    Stream,
    /// 列出模型。
    ListModels,
    /// 非对话类端点的字节直通（嵌入、图像、音频等）。
    Passthrough(PassKind),
}

/// 直通端点的种类。
///
/// 这些端点没有跨协议转换语义（Anthropic 没有图像 API，Gemini 的嵌入形状
/// 完全不同），只在**同协议原生端点**之间路由：请求与响应字节原样往返，
/// 网关提供的是路由、重试、熔断、密钥治理与日志，而不是格式转换。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PassKind {
    /// OpenAI `POST /v1/embeddings`。
    Embeddings,
    /// OpenAI `POST /v1/completions`（legacy text completion / FIM）。
    Completions,
    /// OpenAI `POST /v1/images/generations`。
    Images,
    /// OpenAI `POST /v1/images/edits`（multipart）。
    ImageEdits,
    /// OpenAI `POST /v1/audio/speech`（TTS，响应为音频字节）。
    AudioSpeech,
    /// OpenAI `POST /v1/audio/transcriptions`（STT，multipart）。
    AudioTranscriptions,
    /// OpenAI `POST /v1/audio/translations`（multipart）。
    AudioTranslations,
    /// OpenAI `POST /v1/moderations`。
    Moderations,
    /// Cohere/Jina 形状的 `POST /v1/rerank`。
    Rerank,
    /// Anthropic `POST /v1/messages/count_tokens`。
    CountTokens,
    /// Gemini `POST /v1beta/models/{model}:countTokens`。
    GeminiCountTokens,
    /// Gemini `POST /v1beta/models/{model}:embedContent`。
    GeminiEmbed,
    /// Gemini `POST /v1beta/models/{model}:batchEmbedContents`。
    GeminiBatchEmbed,
}

impl PassKind {
    /// 该端点挂靠的协议 —— 直通只路由到此协议的原生端点。
    pub const fn protocol(self) -> Protocol {
        match self {
            PassKind::CountTokens => Protocol::Messages,
            PassKind::GeminiCountTokens | PassKind::GeminiEmbed | PassKind::GeminiBatchEmbed => {
                Protocol::Gemini
            }
            _ => Protocol::Chat,
        }
    }

    /// 官方地址与拼接模式下使用的默认路径。
    pub const fn default_path(self) -> &'static str {
        match self {
            PassKind::Embeddings => "/embeddings",
            PassKind::Completions => "/completions",
            PassKind::Images => "/images/generations",
            PassKind::ImageEdits => "/images/edits",
            PassKind::AudioSpeech => "/audio/speech",
            PassKind::AudioTranscriptions => "/audio/transcriptions",
            PassKind::AudioTranslations => "/audio/translations",
            PassKind::Moderations => "/moderations",
            PassKind::Rerank => "/rerank",
            PassKind::CountTokens => "/messages/count_tokens",
            // Gemini 的模型与动作编码在路径里，交给占位符替换。
            PassKind::GeminiCountTokens | PassKind::GeminiEmbed | PassKind::GeminiBatchEmbed => {
                "/models/{model}:{action}"
            }
        }
    }

    /// 非完整地址模式下的路径形状校验后缀。
    const fn path_suffix(self) -> &'static str {
        match self {
            PassKind::GeminiCountTokens => ":countTokens",
            PassKind::GeminiEmbed => ":embedContent",
            PassKind::GeminiBatchEmbed => ":batchEmbedContents",
            kind => kind.default_path(),
        }
    }

    /// Gemini URL 中的动作片段。
    const fn gemini_verb(self) -> &'static str {
        match self {
            PassKind::GeminiCountTokens => "countTokens",
            PassKind::GeminiEmbed => "embedContent",
            PassKind::GeminiBatchEmbed => "batchEmbedContents",
            _ => "",
        }
    }
}

impl Action {
    /// Gemini URL 中的动作片段。
    const fn gemini_verb(self) -> &'static str {
        match self {
            Action::Generate => "generateContent",
            Action::Stream => "streamGenerateContent",
            Action::Passthrough(kind) => kind.gemini_verb(),
            // 列表动作不走 `{action}` 占位符，此值不会被使用。
            Action::ListModels => "",
        }
    }
}

/// 地址解析失败。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AddressError {
    /// 开启了非官方模式但没给 base URL。
    #[error("unofficial address requires a base_url")]
    MissingBaseUrl,
    /// 开启了完整地址模式但没给 base URL。
    #[error("full_address mode requires base_url to hold the complete URL")]
    MissingFullAddress,
    /// 拼接结果不是合法 URL。
    #[error("invalid upstream url `{url}`: {source}")]
    InvalidUrl {
        /// 拼接出的字符串。
        url: String,
        /// 底层解析错误。
        #[source]
        source: url::ParseError,
    },
    /// 拼接出的路径不像目标协议的端点。
    #[error(
        "path `{path}` does not look like a {protocol} endpoint; enable full_address to bypass this check"
    )]
    ProtocolMismatch {
        /// 拼接出的路径。
        path: String,
        /// 期望的协议。
        protocol: Protocol,
    },
}

impl UpstreamAddress {
    /// 全部使用协议官方默认值的地址。
    pub const OFFICIAL: Self = Self {
        unofficial: false,
        full_address: false,
        base_url: None,
        version_prefix: None,
        path: None,
    };

    /// 该地址是否完全没有被自定义过 —— 用于聚合渠道端点判断是否应继承渠道默认地址。
    pub fn is_inherited(&self) -> bool {
        !self.unofficial
            && !self.full_address
            && self.base_url.is_none()
            && self.version_prefix.is_none()
            && self.path.is_none()
    }

    /// 解析出最终请求 URL。
    ///
    /// `model` 仅在 Gemini（[`Protocol::path_is_model_dependent`]）时被使用。
    pub fn resolve(
        &self,
        protocol: Protocol,
        action: Action,
        model: &str,
    ) -> Result<url::Url, AddressError> {
        if !self.unofficial {
            return self.build_official(protocol, action, model);
        }

        let base = self
            .base_url
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());

        if self.full_address {
            // 完整地址模式：用户对整个 URL 负责。唯一的加工是替换占位符 ——
            // 不替换的话 Gemini 根本没法用流式，因为动作编码在路径里。
            let raw = base.ok_or(AddressError::MissingFullAddress)?;
            let substituted = substitute_placeholders(raw, action, model);
            return parse_url(&substituted);
        }

        let base = base.ok_or(AddressError::MissingBaseUrl)?;
        let prefix = self
            .version_prefix
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| protocol.default_version_prefix());
        // 自定义 `path` 描述的是**对话端点**。模型列表与各直通端点有自己的
        // 路径语义，继承对话 path 会把它们打到一个必然 4xx 的地址上。
        let path = match action {
            Action::ListModels | Action::Passthrough(_) => default_path_for(protocol, action),
            _ => self
                .path
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| default_path_for(protocol, action)),
        };

        let joined = join_segments(base, &[prefix, path]);
        let substituted = substitute_placeholders(&joined, action, model);
        let mut url = parse_url(&substituted)?;

        // 非完整地址模式下做协议校验：拼错了要尽早报错，而不是把请求打到
        // 一个语义不对的端点然后收到看不懂的 4xx。模型列表动作不校验 ——
        // 各家的列表路径长得都一样；直通动作校验自己的路径形状。
        let path_ok = match action {
            Action::ListModels => true,
            Action::Passthrough(kind) => url
                .path()
                .trim_end_matches('/')
                .ends_with(kind.path_suffix()),
            _ => protocol.path_looks_native(url.path()),
        };
        if !path_ok {
            return Err(AddressError::ProtocolMismatch {
                path: url.path().to_owned(),
                protocol,
            });
        }

        append_gemini_sse_query(&mut url, protocol, action);
        Ok(url)
    }

    fn build_official(
        &self,
        protocol: Protocol,
        action: Action,
        model: &str,
    ) -> Result<url::Url, AddressError> {
        let joined = join_segments(
            protocol.default_base_url(),
            &[
                protocol.default_version_prefix(),
                default_path_for(protocol, action),
            ],
        );
        let mut url = parse_url(&substitute_placeholders(&joined, action, model))?;
        append_gemini_sse_query(&mut url, protocol, action);
        Ok(url)
    }
}

/// Gemini 流式接口只有显式 `alt=sse` 才返回 SSE；缺少它会返回一个普通
/// JSON 数组，随后被流解析器当成损坏的事件流。
///
/// 官方地址与非官方拼接地址都需要 —— Gemini 反代同样遵循这个查询参数
/// 语义。只有完整地址模式不追加：用户对整个 URL（含查询串）负责。
fn append_gemini_sse_query(url: &mut url::Url, protocol: Protocol, action: Action) {
    if protocol != Protocol::Gemini || action != Action::Stream {
        return;
    }
    // 用户可能已经在 path 里写了 alt=sse，别重复追加。
    if url.query_pairs().any(|(k, _)| k == "alt") {
        return;
    }
    url.query_pairs_mut().append_pair("alt", "sse");
}

const fn default_path_for(protocol: Protocol, action: Action) -> &'static str {
    match action {
        Action::ListModels => protocol.default_models_path(),
        // 直通端点的路径由种类自带；协议归属由调用方（网关层）保证。
        Action::Passthrough(kind) => kind.default_path(),
        _ => protocol.default_path(),
    }
}

/// 把 base 与若干路径段拼起来，规范化斜杠。
///
/// 不使用 [`url::Url::join`]：它的相对解析语义会吃掉 base 里的路径段
/// （`https://x.com/proxy` join `/v1/chat` 得到 `https://x.com/v1/chat`），
/// 而中转站的 base 里带路径前缀是常态。
fn join_segments(base: &str, segments: &[&str]) -> String {
    let mut out =
        String::with_capacity(base.len() + segments.iter().map(|s| s.len() + 1).sum::<usize>());
    out.push_str(base.trim_end_matches('/'));
    for seg in segments {
        let seg = seg.trim();
        if seg.is_empty() {
            continue;
        }
        if !seg.starts_with('/') {
            out.push('/');
        }
        out.push_str(seg.trim_end_matches('/'));
    }
    out
}

fn substitute_placeholders(raw: &str, action: Action, model: &str) -> String {
    if !raw.contains('{') {
        return raw.to_owned();
    }
    raw.replace(MODEL_PLACEHOLDER, model)
        .replace(ACTION_PLACEHOLDER, action.gemini_verb())
}

fn parse_url(raw: &str) -> Result<url::Url, AddressError> {
    url::Url::parse(raw).map_err(|source| AddressError::InvalidUrl {
        url: raw.to_owned(),
        source,
    })
}

impl fmt::Display for UpstreamAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.unofficial {
            return f.write_str("<official>");
        }
        if self.full_address {
            return write!(f, "{}", self.base_url.as_deref().unwrap_or("<unset>"));
        }
        write!(
            f,
            "{}{}{}",
            self.base_url.as_deref().unwrap_or("<unset>"),
            self.version_prefix.as_deref().unwrap_or(""),
            self.path.as_deref().unwrap_or("")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn resolved(addr: &UpstreamAddress, p: Protocol, a: Action, m: &str) -> String {
        addr.resolve(p, a, m).unwrap().to_string()
    }

    #[test]
    fn official_mode_ignores_custom_fields() {
        // 关闭 unofficial 时，即使填了自定义字段也一律走官方地址。
        let addr = UpstreamAddress {
            unofficial: false,
            base_url: Some("https://evil.example.com".into()),
            path: Some("/nope".into()),
            ..Default::default()
        };
        assert_eq!(
            resolved(&addr, Protocol::Chat, Action::Generate, "gpt-4o"),
            "https://api.openai.com/v1/chat/completions"
        );
    }

    #[test]
    fn official_defaults_per_protocol() {
        let addr = UpstreamAddress::OFFICIAL;
        assert_eq!(
            resolved(&addr, Protocol::Responses, Action::Generate, "gpt-5"),
            "https://api.openai.com/v1/responses"
        );
        assert_eq!(
            resolved(
                &addr,
                Protocol::Messages,
                Action::Generate,
                "claude-sonnet-4-6"
            ),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            resolved(&addr, Protocol::Gemini, Action::Generate, "gemini-2.5-pro"),
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-pro:generateContent"
        );
    }

    #[test]
    fn gemini_stream_action_switches_url_verb() {
        let addr = UpstreamAddress::OFFICIAL;
        assert_eq!(
            resolved(&addr, Protocol::Gemini, Action::Stream, "gemini-2.5-flash"),
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse"
        );
    }

    #[test]
    fn gemini_non_streaming_official_url_has_no_alt_query() {
        let addr = UpstreamAddress::OFFICIAL;
        assert_eq!(
            resolved(
                &addr,
                Protocol::Gemini,
                Action::Generate,
                "gemini-2.5-flash"
            ),
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:generateContent"
        );
    }

    #[test]
    fn unofficial_gemini_stream_also_gets_alt_sse() {
        // 反代 Gemini 是最常见的非官方形态，缺 alt=sse 流式必坏。
        let addr = UpstreamAddress {
            unofficial: true,
            base_url: Some("https://gemini-proxy.example.com".into()),
            ..Default::default()
        };
        assert_eq!(
            resolved(&addr, Protocol::Gemini, Action::Stream, "gemini-2.5-pro"),
            "https://gemini-proxy.example.com/v1beta/models/gemini-2.5-pro:streamGenerateContent?alt=sse"
        );
        // 非流式不带。
        assert_eq!(
            resolved(&addr, Protocol::Gemini, Action::Generate, "gemini-2.5-pro"),
            "https://gemini-proxy.example.com/v1beta/models/gemini-2.5-pro:generateContent"
        );
    }

    #[test]
    fn gemini_alt_sse_is_not_duplicated() {
        // 用户在自定义 path 里已经写了 alt=sse：不能追加成 alt=sse&alt=sse。
        let addr = UpstreamAddress {
            unofficial: true,
            base_url: Some("https://gemini-proxy.example.com".into()),
            path: Some("/models/{model}:{action}?alt=sse".into()),
            ..Default::default()
        };
        assert_eq!(
            resolved(&addr, Protocol::Gemini, Action::Stream, "g-pro"),
            "https://gemini-proxy.example.com/v1beta/models/g-pro:streamGenerateContent?alt=sse"
        );
    }

    #[test]
    fn unofficial_joins_three_segments() {
        let addr = UpstreamAddress {
            unofficial: true,
            base_url: Some("https://relay.example.com".into()),
            version_prefix: Some("/v1".into()),
            path: Some("/chat/completions".into()),
            ..Default::default()
        };
        assert_eq!(
            resolved(&addr, Protocol::Chat, Action::Generate, "gpt-4o"),
            "https://relay.example.com/v1/chat/completions"
        );
    }

    #[test]
    fn unofficial_preserves_base_path_prefix() {
        // url::Url::join 会吃掉 `/openai`，手写拼接不会 —— 中转站常见形态。
        let addr = UpstreamAddress {
            unofficial: true,
            base_url: Some("https://gw.example.com/openai".into()),
            version_prefix: Some("/v1".into()),
            path: Some("/chat/completions".into()),
            ..Default::default()
        };
        assert_eq!(
            resolved(&addr, Protocol::Chat, Action::Generate, "gpt-4o"),
            "https://gw.example.com/openai/v1/chat/completions"
        );
    }

    #[test]
    fn unofficial_normalizes_slashes() {
        let addr = UpstreamAddress {
            unofficial: true,
            base_url: Some("https://relay.example.com/".into()),
            version_prefix: Some("v1/".into()),
            path: Some("chat/completions/".into()),
            ..Default::default()
        };
        assert_eq!(
            resolved(&addr, Protocol::Chat, Action::Generate, "gpt-4o"),
            "https://relay.example.com/v1/chat/completions"
        );
    }

    #[test]
    fn unofficial_falls_back_to_protocol_defaults_for_blank_segments() {
        let addr = UpstreamAddress {
            unofficial: true,
            base_url: Some("https://relay.example.com".into()),
            version_prefix: None,
            path: Some("   ".into()),
            ..Default::default()
        };
        assert_eq!(
            resolved(&addr, Protocol::Messages, Action::Generate, "claude"),
            "https://relay.example.com/v1/messages"
        );
    }

    #[test]
    fn unofficial_without_base_url_is_rejected() {
        let addr = UpstreamAddress {
            unofficial: true,
            ..Default::default()
        };
        assert_eq!(
            addr.resolve(Protocol::Chat, Action::Generate, "m"),
            Err(AddressError::MissingBaseUrl)
        );
    }

    #[test]
    fn unofficial_validates_protocol_shape() {
        // 把 messages 的路径配到 chat 协议上，应当被拦下。
        let addr = UpstreamAddress {
            unofficial: true,
            base_url: Some("https://relay.example.com".into()),
            path: Some("/messages".into()),
            ..Default::default()
        };
        let err = addr
            .resolve(Protocol::Chat, Action::Generate, "m")
            .unwrap_err();
        assert!(
            matches!(err, AddressError::ProtocolMismatch { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn full_address_bypasses_join_and_validation() {
        // 需求 2：完整地址模式下不再拼接和校验协议。
        let addr = UpstreamAddress {
            unofficial: true,
            full_address: true,
            base_url: Some("https://weird.example.com/some/odd/endpoint".into()),
            version_prefix: Some("/ignored".into()),
            path: Some("/ignored".into()),
        };
        assert_eq!(
            resolved(&addr, Protocol::Chat, Action::Generate, "gpt-4o"),
            "https://weird.example.com/some/odd/endpoint"
        );
    }

    #[test]
    fn full_address_still_substitutes_gemini_placeholders() {
        // 不替换的话 Gemini 在完整地址模式下无法切流式。
        let addr = UpstreamAddress {
            unofficial: true,
            full_address: true,
            base_url: Some("https://proxy.example.com/g/models/{model}:{action}".into()),
            ..Default::default()
        };
        assert_eq!(
            resolved(&addr, Protocol::Gemini, Action::Stream, "gemini-2.5-pro"),
            "https://proxy.example.com/g/models/gemini-2.5-pro:streamGenerateContent"
        );
    }

    #[test]
    fn full_address_without_base_url_is_rejected() {
        let addr = UpstreamAddress {
            unofficial: true,
            full_address: true,
            ..Default::default()
        };
        assert_eq!(
            addr.resolve(Protocol::Chat, Action::Generate, "m"),
            Err(AddressError::MissingFullAddress)
        );
    }

    #[test]
    fn malformed_url_is_reported() {
        let addr = UpstreamAddress {
            unofficial: true,
            full_address: true,
            base_url: Some("not a url".into()),
            ..Default::default()
        };
        assert!(matches!(
            addr.resolve(Protocol::Chat, Action::Generate, "m"),
            Err(AddressError::InvalidUrl { .. })
        ));
    }

    #[test]
    fn list_models_action_skips_protocol_validation() {
        let addr = UpstreamAddress {
            unofficial: true,
            base_url: Some("https://relay.example.com".into()),
            ..Default::default()
        };
        assert_eq!(
            resolved(&addr, Protocol::Chat, Action::ListModels, ""),
            "https://relay.example.com/v1/models"
        );
    }

    #[test]
    fn embeddings_action_uses_embeddings_path() {
        assert_eq!(
            resolved(
                &UpstreamAddress::OFFICIAL,
                Protocol::Chat,
                Action::Passthrough(PassKind::Embeddings),
                "text-embedding-3-small"
            ),
            "https://api.openai.com/v1/embeddings"
        );

        // 非官方模式：base + 前缀 + 默认嵌入路径。
        let addr = UpstreamAddress {
            unofficial: true,
            base_url: Some("https://relay.example.com/openai".into()),
            ..Default::default()
        };
        assert_eq!(
            resolved(
                &addr,
                Protocol::Chat,
                Action::Passthrough(PassKind::Embeddings),
                "m"
            ),
            "https://relay.example.com/openai/v1/embeddings"
        );
    }

    #[test]
    fn embeddings_and_list_models_ignore_the_chat_path_override() {
        // 自定义 `path` 是对话端点的路径；嵌入与模型列表必须用自己的默认路径，
        // 否则会打到一个必然 4xx 的地址。
        let addr = UpstreamAddress {
            unofficial: true,
            base_url: Some("https://relay.example.com".into()),
            path: Some("/chat/completions".into()),
            ..Default::default()
        };
        assert_eq!(
            resolved(
                &addr,
                Protocol::Chat,
                Action::Passthrough(PassKind::Embeddings),
                "m"
            ),
            "https://relay.example.com/v1/embeddings"
        );
        assert_eq!(
            resolved(&addr, Protocol::Chat, Action::ListModels, ""),
            "https://relay.example.com/v1/models"
        );
    }

    #[test]
    fn inherited_detects_untouched_address() {
        assert!(UpstreamAddress::default().is_inherited());
        assert!(
            !UpstreamAddress {
                base_url: Some("https://x".into()),
                ..Default::default()
            }
            .is_inherited()
        );
    }
}
