//! HTTP 错误响应。
//!
//! **关键约束：错误体的形状必须匹配客户端所用的协议。** OpenAI SDK 读
//! `error.message`，Anthropic SDK 读 `error.type` + `error.message`，Google
//! SDK 读 `error.status`。返回一个「统一格式」的错误体，等于让所有官方 SDK
//! 都拿不到错误信息，只能抛出一个无意义的解析异常 —— 这是 new-api 上真实
//! 存在的体验问题。

use std::convert::Infallible;
use std::fmt;
use std::pin::pin;

use refract_core::{ErrorKind, GatewayError, Protocol};
use serde_json::{Value, json};
use xitca_web::WebContext;
use xitca_web::body::{BodyExt, RequestBody, ResponseBody};
use xitca_web::bytes::Bytes;
use xitca_web::error::{Error as WebError, Request};
use xitca_web::http::{HeaderValue, StatusCode, WebResponse, header};
use xitca_web::service::Service;

/// 管理 API 的错误体。
///
/// 管理端是我们自己的前端，形状由我们定 —— 用 `code` 而非协议特定的字段名，
/// 前端可以统一处理。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ErrorEnvelope {
    /// 机器可读的错误码。
    pub code: &'static str,
    /// 人类可读的消息。
    pub message: String,
    /// 附加细节，前端可选择展示。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// 管理 API 错误。
#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
pub struct ApiError(pub GatewayError);

impl From<GatewayError> for ApiError {
    fn from(err: GatewayError) -> Self {
        Self(err)
    }
}

impl From<refract_store::StoreError> for ApiError {
    fn from(err: refract_store::StoreError) -> Self {
        Self(store_to_gateway(err))
    }
}

/// 存储错误到网关错误的映射。
///
/// `NotFound` 必须映射成 404 而不是 500：前端要靠状态码区分「这条记录没了」
/// 和「服务器炸了」，两者的处理方式完全不同。
pub fn store_to_gateway(err: refract_store::StoreError) -> GatewayError {
    use refract_store::StoreError as S;
    match err {
        S::NotFound { .. } => GatewayError::not_found(err.to_string()),
        S::Invalid(msg) => GatewayError::invalid_request(msg),
        other => GatewayError::internal(other.to_string()),
    }
}

/// 把网关错误渲染成指定协议的错误体。
///
/// 这是四家错误格式的唯一真相来源。
pub fn error_body(err: &GatewayError, protocol: Protocol) -> Value {
    let message = &err.message;
    let mut body = match protocol {
        // OpenAI Chat / Responses 共用同一形状。
        Protocol::Chat | Protocol::Responses => json!({
            "error": {
                "message": message,
                "type": err.kind.openai_type(),
                "param": Value::Null,
                "code": Value::Null,
            }
        }),
        Protocol::Messages => json!({
            "type": "error",
            "error": {
                "type": err.kind.anthropic_type(),
                "message": message,
            }
        }),
        Protocol::Gemini => json!({
            "error": {
                "code": err.status(),
                "message": message,
                "status": err.kind.google_status(),
            }
        }),
    };
    // 结构化 details（如余额不足的 balance/type）并入 error 对象，
    // 让 SDK 直接读到机器可判的字段，而不是解析 message 字符串。
    if let Some(details) = &err.details
        && let (Some(error_obj), Some(detail_obj)) = (
            body.get_mut("error").and_then(Value::as_object_mut),
            details.as_object(),
        )
    {
        for (key, value) in detail_obj {
            error_obj.insert(key.clone(), value.clone());
        }
    }
    body
}

/// 构造 JSON 响应。
pub fn json_response(status: StatusCode, body: &impl serde::Serialize) -> WebResponse {
    let bytes =
        serde_json::to_vec(body).unwrap_or_else(|_| br#"{"error":"serialize failed"}"#.to_vec());
    let mut response = WebResponse::new(ResponseBody::bytes(bytes));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

/// 空 body 响应。
pub fn empty_response(status: StatusCode) -> WebResponse {
    let mut response = WebResponse::new(ResponseBody::empty());
    *response.status_mut() = status;
    response
}

/// 构造一个协议正确的错误响应。
pub fn protocol_error_reply(err: &GatewayError, protocol: Protocol) -> WebResponse {
    let status = StatusCode::from_u16(err.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    json_response(status, &error_body(err, protocol))
}

/// 管理 API 的错误码字符串。
fn admin_code(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::InvalidRequest => "invalid_request",
        ErrorKind::Unauthenticated => "unauthenticated",
        ErrorKind::PermissionDenied => "permission_denied",
        ErrorKind::NotFound => "not_found",
        ErrorKind::PayloadTooLarge => "payload_too_large",
        ErrorKind::RateLimited => "rate_limited",
        ErrorKind::UpstreamError => "upstream_error",
        ErrorKind::InvalidUpstreamResponse => "invalid_upstream_response",
        ErrorKind::NoAvailableChannel => "no_available_channel",
        ErrorKind::Timeout => "timeout",
        ErrorKind::TranscodeNotPermitted => "transcode_not_permitted",
        ErrorKind::TranscodeFailed => "transcode_failed",
        ErrorKind::Configuration => "configuration_error",
        ErrorKind::Internal => "internal_error",
    }
}

/// 管理 API 的错误响应。
pub fn admin_error_reply(err: &GatewayError) -> WebResponse {
    let status = StatusCode::from_u16(err.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let envelope = ErrorEnvelope {
        code: admin_code(err.kind),
        message: err.message.clone(),
        // 上游原始响应对排查渠道配置至关重要 —— 前端会把它折叠展示。
        detail: err.upstream_body.clone(),
    };
    let mut response = json_response(status, &envelope);
    if let Some(wait) = err.retry_after
        && let Ok(value) = HeaderValue::from_str(&wait.as_secs().max(1).to_string())
    {
        response.headers_mut().insert("retry-after", value);
    }
    response
}

fn admin_fallback(status: StatusCode, message: String) -> WebResponse {
    let envelope = ErrorEnvelope {
        code: match status {
            StatusCode::NOT_FOUND => "not_found",
            StatusCode::BAD_REQUEST => "invalid_request",
            StatusCode::METHOD_NOT_ALLOWED => "method_not_allowed",
            StatusCode::PAYLOAD_TOO_LARGE => "payload_too_large",
            _ => "internal_error",
        },
        message,
        detail: None,
    };
    json_response(status, &envelope)
}

fn protocol_fallback(status: StatusCode, message: String, protocol: Protocol) -> WebResponse {
    let err = match status {
        StatusCode::NOT_FOUND => GatewayError::not_found(message),
        StatusCode::PAYLOAD_TOO_LARGE => GatewayError::new(ErrorKind::PayloadTooLarge, message),
        StatusCode::INTERNAL_SERVER_ERROR => GatewayError::internal(message),
        _ => GatewayError::invalid_request(message),
    };
    let mut response = protocol_error_reply(&err, protocol);
    if response.status() != status {
        *response.status_mut() = status;
    }
    response
}

/// 按路径选择协议信封：`/v1beta` → Gemini，`/v1` → Chat，其余 → 管理面。
pub fn envelope_protocol(path: &str) -> Option<Protocol> {
    if path == "/v1beta" || path.starts_with("/v1beta/") {
        Some(Protocol::Gemini)
    } else if path == "/v1" || path.starts_with("/v1/") {
        Some(Protocol::Chat)
    } else {
        None
    }
}

/// 带协议信息的错误，让恢复路径能回出正确形状的错误体。
#[derive(Debug, Clone)]
pub struct ProtocolRejection {
    /// 底层错误。
    pub error: GatewayError,
    /// 客户端使用的协议。
    pub protocol: Protocol,
    /// 少数 HTTP 层错误（例如 405）不能只由领域错误类型表达。
    pub status: Option<StatusCode>,
    /// 网关生成的请求标识；有值时回写 `x-refract-request-id` 响应头。
    pub request_id: Option<String>,
}

impl ProtocolRejection {
    /// 构造一个协议感知的错误。
    pub fn new(error: GatewayError, protocol: Protocol) -> Self {
        Self {
            error,
            protocol,
            status: None,
            request_id: None,
        }
    }

    /// 构造带请求标识的协议错误 —— 客户端拿着 `x-refract-request-id` 报障时，
    /// 失败的请求也要能对上日志。
    pub fn with_id(error: GatewayError, protocol: Protocol, request_id: String) -> Self {
        Self {
            error,
            protocol,
            status: None,
            request_id: Some(request_id),
        }
    }

    /// 构造带显式 HTTP 状态码的协议错误。
    pub fn with_status(error: GatewayError, protocol: Protocol, status: StatusCode) -> Self {
        Self {
            error,
            protocol,
            status: Some(status),
            request_id: None,
        }
    }

    /// 渲染为协议信封响应，并补上网关请求标识与 Retry-After。
    pub fn into_response(&self) -> WebResponse {
        let mut response = protocol_error_reply(&self.error, self.protocol);
        if let Some(status) = self.status {
            *response.status_mut() = status;
        }
        if let Some(id) = &self.request_id
            && let Ok(value) = HeaderValue::from_str(id)
        {
            // 网关自有标识，与上游透传的 `x-request-id` 分属两条排障链路。
            response.headers_mut().insert("x-refract-request-id", value);
        }
        // 限流（本地或上游）时告诉客户端何时重试 —— 标准客户端会自动遵守。
        if let Some(wait) = self.error.retry_after
            && let Ok(value) = HeaderValue::from_str(&wait.as_secs().max(1).to_string())
        {
            response.headers_mut().insert("retry-after", value);
        }
        response
    }
}

/// handler 的统一错误类型。
#[derive(Debug, Clone)]
pub enum AppError {
    /// 管理面信封。
    Admin(GatewayError),
    /// 协议信封。
    Protocol(ProtocolRejection),
    /// 未匹配路由。
    NotFound {
        /// 原始请求路径，用来选信封。
        path: String,
    },
    /// 方法不允许。
    MethodNotAllowed {
        /// 该路径允许的方法，写入错误信息。
        allowed: String,
    },
    /// 请求体超过上限。
    PayloadTooLarge,
    /// 畸形请求。
    BadRequest(String),
}

impl AppError {
    /// 按路径把本错误渲染成 HTTP 响应。
    pub fn to_response(&self, request_path: &str) -> WebResponse {
        match self {
            Self::Admin(err) => admin_error_reply(err),
            Self::Protocol(rejection) => rejection.into_response(),
            Self::NotFound { path } => match envelope_protocol(path) {
                Some(protocol) => protocol_fallback(
                    StatusCode::NOT_FOUND,
                    "endpoint not found".to_owned(),
                    protocol,
                ),
                None => admin_fallback(StatusCode::NOT_FOUND, "endpoint not found".to_owned()),
            },
            Self::MethodNotAllowed { allowed } => {
                let message = format!("HTTP method not allowed; this endpoint accepts {allowed}");
                let mut response = match envelope_protocol(request_path) {
                    Some(protocol) => {
                        protocol_fallback(StatusCode::METHOD_NOT_ALLOWED, message, protocol)
                    }
                    None => admin_fallback(StatusCode::METHOD_NOT_ALLOWED, message),
                };
                if let Ok(value) = HeaderValue::from_str(allowed) {
                    response.headers_mut().insert(header::ALLOW, value);
                }
                response
            }
            Self::PayloadTooLarge => match envelope_protocol(request_path) {
                Some(protocol) => protocol_fallback(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "request body too large".to_owned(),
                    protocol,
                ),
                None => admin_fallback(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "request body too large".to_owned(),
                ),
            },
            Self::BadRequest(message) => match envelope_protocol(request_path) {
                Some(protocol) => {
                    protocol_fallback(StatusCode::BAD_REQUEST, message.clone(), protocol)
                }
                None => admin_fallback(StatusCode::BAD_REQUEST, message.clone()),
            },
        }
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Admin(err) => write!(f, "{err}"),
            Self::Protocol(rejection) => write!(f, "{}", rejection.error),
            Self::NotFound { path } => write!(f, "endpoint not found: {path}"),
            Self::MethodNotAllowed { allowed } => {
                write!(
                    f,
                    "HTTP method not allowed; this endpoint accepts {allowed}"
                )
            }
            Self::PayloadTooLarge => f.write_str("request body too large"),
            Self::BadRequest(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for AppError {}

impl From<ProtocolRejection> for AppError {
    fn from(rejection: ProtocolRejection) -> Self {
        Self::Protocol(rejection)
    }
}

impl From<ApiError> for AppError {
    fn from(err: ApiError) -> Self {
        Self::Admin(err.0)
    }
}

impl From<GatewayError> for AppError {
    fn from(err: GatewayError) -> Self {
        Self::Admin(err)
    }
}

impl From<refract_store::StoreError> for AppError {
    fn from(err: refract_store::StoreError) -> Self {
        Self::Admin(store_to_gateway(err))
    }
}

impl From<AppError> for WebError {
    fn from(err: AppError) -> Self {
        WebError::from_service(err)
    }
}

impl<'r> Service<WebContext<'r, Request<'r>>> for AppError {
    type Response = WebResponse;
    type Error = Infallible;

    async fn call(&self, ctx: WebContext<'r, Request<'r>>) -> Result<Self::Response, Self::Error> {
        Ok(self.to_response(ctx.req().uri().path()))
    }
}

/// 带上限地读完请求体。超过 `limit` 返回 [`AppError::PayloadTooLarge`]。
pub async fn collect_limited(body: &mut RequestBody, limit: usize) -> Result<Bytes, AppError> {
    let mut body = pin!(body);
    let mut collected = Vec::new();
    while let Some(chunk) = body.as_mut().data().await {
        let chunk = chunk.map_err(|error| {
            AppError::BadRequest(format!("failed to read request body: {error}"))
        })?;
        collected.extend_from_slice(chunk.as_ref());
        if collected.len() > limit {
            return Err(AppError::PayloadTooLarge);
        }
    }
    Ok(Bytes::from(collected))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn openai_error_shape_matches_the_sdk_contract() {
        let err = GatewayError::not_found("model `x` not found");
        let body = error_body(&err, Protocol::Chat);
        // OpenAI SDK 读的是 error.message 与 error.type。
        assert_eq!(body["error"]["message"], "model `x` not found");
        assert_eq!(body["error"]["type"], "invalid_request_error");
        // param/code 必须存在（哪怕是 null），某些旧版 SDK 会直接索引。
        assert!(body["error"].get("param").is_some());
        assert!(body["error"].get("code").is_some());
    }

    #[test]
    fn responses_shares_the_openai_shape() {
        let err = GatewayError::invalid_request("bad");
        assert_eq!(
            error_body(&err, Protocol::Chat),
            error_body(&err, Protocol::Responses)
        );
    }

    #[test]
    fn anthropic_error_shape_has_top_level_type() {
        let err = GatewayError::new(ErrorKind::RateLimited, "slow down");
        let body = error_body(&err, Protocol::Messages);
        // Anthropic SDK 先看顶层 type == "error"。
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["type"], "rate_limit_error");
        assert_eq!(body["error"]["message"], "slow down");
    }

    #[test]
    fn insufficient_balance_details_merge_into_openai_error() {
        let err = GatewayError::new(ErrorKind::PermissionDenied, "insufficient balance: 0.0000")
            .with_details(serde_json::json!({"type": "insufficient_balance", "balance": 0.0}));
        let body = error_body(&err, Protocol::Chat);
        assert_eq!(body["error"]["type"], "insufficient_balance");
        assert_eq!(body["error"]["balance"], 0.0);
        // message 仍然保留人类可读描述。
        assert_eq!(body["error"]["message"], "insufficient balance: 0.0000");
    }

    #[test]
    fn gemini_error_shape_carries_code_and_status() {
        let err = GatewayError::new(ErrorKind::RateLimited, "quota");
        let body = error_body(&err, Protocol::Gemini);
        assert_eq!(body["error"]["code"], 429);
        assert_eq!(body["error"]["status"], "RESOURCE_EXHAUSTED");
        assert_eq!(body["error"]["message"], "quota");
    }

    #[test]
    fn transcode_rejection_is_a_client_error() {
        // 需求 4：未勾选的协议要明确报错，且必须是 4xx —— 这是配置问题，
        // 不是服务器故障，报 5xx 会让客户端无谓重试。
        let err = GatewayError::transcode_not_permitted(Protocol::Chat, Protocol::Messages);
        assert_eq!(err.status(), 400);
        let body = error_body(&err, Protocol::Chat);
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("does not accept transcoding")
        );
    }

    #[test]
    fn store_not_found_becomes_404_not_500() {
        let err = store_to_gateway(refract_store::StoreError::not_found("channel", 42));
        assert_eq!(err.status(), 404);
    }

    #[test]
    fn store_invalid_becomes_400() {
        let err = store_to_gateway(refract_store::StoreError::Invalid("bad name".into()));
        assert_eq!(err.status(), 400);
    }

    #[test]
    fn admin_codes_are_stable_strings() {
        // 前端会对这些码做分支，改动它们等于破坏 API 契约。
        assert_eq!(admin_code(ErrorKind::NotFound), "not_found");
        assert_eq!(
            admin_code(ErrorKind::NoAvailableChannel),
            "no_available_channel"
        );
        assert_eq!(
            admin_code(ErrorKind::TranscodeNotPermitted),
            "transcode_not_permitted"
        );
    }

    #[test]
    fn upstream_body_is_surfaced_as_detail() {
        let err = GatewayError::new(ErrorKind::UpstreamError, "boom")
            .with_upstream(502, "<html>nginx</html>");
        let envelope = ErrorEnvelope {
            code: admin_code(err.kind),
            message: err.message.clone(),
            detail: err.upstream_body.clone(),
        };
        assert_eq!(envelope.detail.as_deref(), Some("<html>nginx</html>"));
    }
}
