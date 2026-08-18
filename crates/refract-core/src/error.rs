//! 网关错误分类。
//!
//! 错误必须携带足够信息让 API 层把它渲染成**四种协议各自的**错误体格式，
//! 所以这里区分「错误种类」（决定 HTTP 状态码与语义）与「消息」（人类可读）。

use crate::protocol::Protocol;

/// 错误种类。映射到 HTTP 状态码与各协议的错误 `type` 字段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// 请求体不合法、缺字段、字段类型错。400
    InvalidRequest,
    /// 鉴权失败。401
    Unauthenticated,
    /// 已鉴权但无权限。403
    PermissionDenied,
    /// 模型或资源不存在。404
    NotFound,
    /// 请求过大。413
    PayloadTooLarge,
    /// 触发限流。429
    RateLimited,
    /// 上游返回错误。502
    UpstreamError,
    /// 上游返回 HTTP 200，但响应体不符合所配置的协议。500
    InvalidUpstreamResponse,
    /// 无可用渠道。503
    NoAvailableChannel,
    /// 上游超时。504
    Timeout,
    /// 协议转换不被允许（需求 4 的显式拒绝）。400
    TranscodeNotPermitted,
    /// 协议转换过程中出错。500
    TranscodeFailed,
    /// 配置错误。500
    Configuration,
    /// 其他内部错误。500
    Internal,
}

impl ErrorKind {
    /// 对应的 HTTP 状态码。
    pub const fn status(self) -> u16 {
        match self {
            ErrorKind::InvalidRequest | ErrorKind::TranscodeNotPermitted => 400,
            ErrorKind::Unauthenticated => 401,
            ErrorKind::PermissionDenied => 403,
            ErrorKind::NotFound => 404,
            ErrorKind::PayloadTooLarge => 413,
            ErrorKind::RateLimited => 429,
            ErrorKind::Internal
            | ErrorKind::TranscodeFailed
            | ErrorKind::Configuration
            | ErrorKind::InvalidUpstreamResponse => 500,
            ErrorKind::UpstreamError => 502,
            ErrorKind::NoAvailableChannel => 503,
            ErrorKind::Timeout => 504,
        }
    }

    /// OpenAI 风格的错误 `type` 值。
    pub const fn openai_type(self) -> &'static str {
        match self {
            ErrorKind::InvalidRequest
            | ErrorKind::TranscodeNotPermitted
            | ErrorKind::PayloadTooLarge
            | ErrorKind::NotFound => "invalid_request_error",
            ErrorKind::Unauthenticated => "authentication_error",
            ErrorKind::PermissionDenied => "permission_error",
            ErrorKind::RateLimited => "rate_limit_error",
            ErrorKind::UpstreamError | ErrorKind::NoAvailableChannel => "api_error",
            ErrorKind::Timeout => "timeout_error",
            ErrorKind::TranscodeFailed
            | ErrorKind::Configuration
            | ErrorKind::Internal
            | ErrorKind::InvalidUpstreamResponse => "internal_server_error",
        }
    }

    /// Anthropic 风格的错误 `type` 值。
    pub const fn anthropic_type(self) -> &'static str {
        match self {
            ErrorKind::InvalidRequest | ErrorKind::TranscodeNotPermitted => "invalid_request_error",
            ErrorKind::Unauthenticated => "authentication_error",
            ErrorKind::PermissionDenied => "permission_error",
            ErrorKind::NotFound => "not_found_error",
            ErrorKind::PayloadTooLarge => "request_too_large",
            ErrorKind::RateLimited => "rate_limit_error",
            ErrorKind::UpstreamError | ErrorKind::NoAvailableChannel => "api_error",
            ErrorKind::Timeout => "timeout_error",
            ErrorKind::TranscodeFailed
            | ErrorKind::Configuration
            | ErrorKind::Internal
            | ErrorKind::InvalidUpstreamResponse => "api_error",
        }
    }

    /// Google 风格的 `status` 值。
    pub const fn google_status(self) -> &'static str {
        match self {
            ErrorKind::InvalidRequest | ErrorKind::TranscodeNotPermitted => "INVALID_ARGUMENT",
            ErrorKind::Unauthenticated => "UNAUTHENTICATED",
            ErrorKind::PermissionDenied => "PERMISSION_DENIED",
            ErrorKind::NotFound => "NOT_FOUND",
            ErrorKind::PayloadTooLarge => "OUT_OF_RANGE",
            ErrorKind::RateLimited => "RESOURCE_EXHAUSTED",
            ErrorKind::UpstreamError => "INTERNAL",
            ErrorKind::NoAvailableChannel => "UNAVAILABLE",
            ErrorKind::Timeout => "DEADLINE_EXCEEDED",
            ErrorKind::TranscodeFailed
            | ErrorKind::Configuration
            | ErrorKind::Internal
            | ErrorKind::InvalidUpstreamResponse => "INTERNAL",
        }
    }

    /// 该错误是否值得换一个渠道重试。
    ///
    /// 判断标准是「错在上游还是错在请求」：
    /// - 错在请求（400/404/413）：换渠道也一样错，重试只是浪费配额。
    /// - 错在上游（限流、5xx、超时）：换渠道很可能就好了。
    /// - **鉴权失败也算错在上游**：上游返回 401/403 意味着这个渠道的密钥过期
    ///   或被封，不代表客户端的请求有问题 —— 恰恰应该换到别的渠道去。这是
    ///   聚合网关与单上游代理的关键差别。客户端自身的鉴权失败发生在路由之前，
    ///   走不到重试逻辑，因此这里返回 true 不会导致误重试。
    pub const fn is_retryable(self) -> bool {
        matches!(
            self,
            ErrorKind::RateLimited
                | ErrorKind::UpstreamError
                | ErrorKind::Timeout
                | ErrorKind::Unauthenticated
                | ErrorKind::PermissionDenied
        )
    }

    /// 该错误是否说明**这把 API key 本身**有问题（过期、无权限、配额耗尽）。
    ///
    /// 与 `is_retryable` 的差别：限流/上游错误换渠道可能就好，而鉴权族错误
    /// 是「key 维度」的失败 —— 同一渠道换一把 key 往往就能解决。执行器只在
    /// 这些错误上轮转密钥池，其他错误轮转只是徒劳。
    pub const fn is_key_failure(self) -> bool {
        matches!(
            self,
            ErrorKind::Unauthenticated | ErrorKind::PermissionDenied | ErrorKind::RateLimited
        )
    }
}

/// 网关错误。
#[derive(Debug, Clone, thiserror::Error)]
#[error("{kind:?}: {message}")]
pub struct GatewayError {
    /// 错误种类。
    pub kind: ErrorKind,
    /// 人类可读消息。
    pub message: String,
    /// 出错时涉及的上游协议（若有）。
    pub protocol: Option<Protocol>,
    /// 出错时涉及的渠道（若有）。
    pub channel_id: Option<crate::channel::ChannelId>,
    /// 出错时涉及的渠道名（用于失败日志）。
    pub channel_name: Option<String>,
    /// 已实际尝试的候选数。
    pub attempts: u8,
    /// 最后一次尝试使用的上游模型名。
    pub upstream_model: Option<String>,
    /// 上游原始错误体，透传给客户端时保留细节。
    pub upstream_body: Option<String>,
    /// 上游返回的 HTTP 状态码。
    pub upstream_status: Option<u16>,
    /// 上游通过 `Retry-After` 头声明的重试等待时长。
    ///
    /// 健康度记录用它来悬停端点：上游明确说了「多久之后再来」，固定
    /// 指数退避就不该更早去打扰它。
    pub retry_after: Option<std::time::Duration>,
    /// 最后一次尝试使用的密钥脱敏形式（失败日志定位坏 key 用）。
    pub credential_hint: Option<String>,
}

impl GatewayError {
    /// 构造一个错误。
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            protocol: None,
            channel_id: None,
            channel_name: None,
            attempts: 0,
            upstream_model: None,
            upstream_body: None,
            upstream_status: None,
            retry_after: None,
            credential_hint: None,
        }
    }

    /// 请求不合法。
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::InvalidRequest, message)
    }

    /// 鉴权失败。
    pub fn unauthenticated(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Unauthenticated, message)
    }

    /// 未找到。
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::NotFound, message)
    }

    /// 无可用渠道。
    pub fn no_channel(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::NoAvailableChannel, message)
    }

    /// 协议转换被策略拒绝（需求 4）。
    pub fn transcode_not_permitted(inbound: Protocol, native: Protocol) -> Self {
        Self::new(
            ErrorKind::TranscodeNotPermitted,
            format!(
                "channel endpoint speaks `{native}` and does not accept transcoding from `{inbound}`"
            ),
        )
        .with_protocol(native)
    }

    /// 协议转换失败。
    pub fn transcode_failed(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::TranscodeFailed, message)
    }

    /// 内部错误。
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Internal, message)
    }

    /// 附加协议信息。
    pub fn with_protocol(mut self, protocol: Protocol) -> Self {
        self.protocol = Some(protocol);
        self
    }

    /// 附加渠道信息。
    pub fn with_channel(mut self, id: crate::channel::ChannelId) -> Self {
        self.channel_id = Some(id);
        self
    }

    /// 附加上游原始响应。
    pub fn with_upstream(mut self, status: u16, body: impl Into<String>) -> Self {
        self.upstream_status = Some(status);
        self.upstream_body = Some(body.into());
        self
    }

    /// 附加上游声明的重试等待时长。
    pub fn with_retry_after(mut self, wait: std::time::Duration) -> Self {
        self.retry_after = Some(wait);
        self
    }

    /// 对外 HTTP 状态码。
    pub const fn status(&self) -> u16 {
        self.kind.status()
    }

    /// 是否值得换渠道重试。
    pub const fn is_retryable(&self) -> bool {
        self.kind.is_retryable()
    }
}

impl From<crate::address::AddressError> for GatewayError {
    fn from(err: crate::address::AddressError) -> Self {
        Self::new(ErrorKind::Configuration, err.to_string())
    }
}

impl From<crate::channel::ChannelError> for GatewayError {
    fn from(err: crate::channel::ChannelError) -> Self {
        Self::new(ErrorKind::InvalidRequest, err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_codes_match_semantics() {
        assert_eq!(ErrorKind::InvalidRequest.status(), 400);
        assert_eq!(ErrorKind::Unauthenticated.status(), 401);
        assert_eq!(ErrorKind::RateLimited.status(), 429);
        assert_eq!(ErrorKind::UpstreamError.status(), 502);
        assert_eq!(ErrorKind::InvalidUpstreamResponse.status(), 500);
        assert_eq!(ErrorKind::NoAvailableChannel.status(), 503);
        assert_eq!(ErrorKind::Timeout.status(), 504);
        // 需求 4 的拒绝是客户端配置/用法问题，不是服务端故障。
        assert_eq!(ErrorKind::TranscodeNotPermitted.status(), 400);
    }

    #[test]
    fn client_errors_are_not_retried() {
        assert!(!ErrorKind::InvalidRequest.is_retryable());
        assert!(!ErrorKind::NotFound.is_retryable());
        assert!(!ErrorKind::TranscodeNotPermitted.is_retryable());
        assert!(!ErrorKind::InvalidUpstreamResponse.is_retryable());
        // 401/403 可能只是这一个渠道的 key 坏了，值得换一个。
        assert!(ErrorKind::Unauthenticated.is_retryable());
        assert!(ErrorKind::RateLimited.is_retryable());
        assert!(ErrorKind::Timeout.is_retryable());
    }

    #[test]
    fn transcode_rejection_names_both_protocols() {
        let err = GatewayError::transcode_not_permitted(Protocol::Messages, Protocol::Chat);
        assert_eq!(err.kind, ErrorKind::TranscodeNotPermitted);
        assert!(err.message.contains("messages"), "{}", err.message);
        assert!(err.message.contains("chat"), "{}", err.message);
        assert_eq!(err.protocol, Some(Protocol::Chat));
    }

    #[test]
    fn every_kind_has_all_three_protocol_renderings() {
        let kinds = [
            ErrorKind::InvalidRequest,
            ErrorKind::Unauthenticated,
            ErrorKind::PermissionDenied,
            ErrorKind::NotFound,
            ErrorKind::PayloadTooLarge,
            ErrorKind::RateLimited,
            ErrorKind::UpstreamError,
            ErrorKind::InvalidUpstreamResponse,
            ErrorKind::NoAvailableChannel,
            ErrorKind::Timeout,
            ErrorKind::TranscodeNotPermitted,
            ErrorKind::TranscodeFailed,
            ErrorKind::Configuration,
            ErrorKind::Internal,
        ];
        for k in kinds {
            assert!(!k.openai_type().is_empty());
            assert!(!k.anthropic_type().is_empty());
            assert!(!k.google_status().is_empty());
            assert!((400..=504).contains(&k.status()));
        }
    }

    #[test]
    fn builder_attaches_context() {
        let err = GatewayError::internal("boom")
            .with_protocol(Protocol::Gemini)
            .with_channel(42)
            .with_upstream(502, r#"{"error":"upstream exploded"}"#);
        assert_eq!(err.protocol, Some(Protocol::Gemini));
        assert_eq!(err.channel_id, Some(42));
        assert_eq!(err.upstream_status, Some(502));
        assert!(err.upstream_body.unwrap().contains("exploded"));
    }
}
