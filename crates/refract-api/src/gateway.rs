//! 网关路由（`/v1/...`、`/v1beta/...`）。
//!
//! 这一层的形状**不由我们决定** —— 它必须逐字节地像 OpenAI / Anthropic /
//! Google 的官方 API，否则现成的 SDK 用不了，而「能用现成 SDK」正是这个网关
//! 存在的理由。
//!
//! 一次请求的完整生命周期：
//!
//! ```text
//! 客户端 JSON ──decode_request──▶ UnifiedRequest(IR)
//!                                      │
//!                                  planner.plan  ← 渠道快照 + 路由策略
//!                                      │
//!                                 executor.execute ← 重试/熔断/协议转换
//!                                      │
//! 客户端 JSON ◀──encode_response── UnifiedResponse(IR)
//! ```
//!
//! 两个容易做错的地方，这里都做对了：
//!
//! 1. **流式响应必须边收边发**。把整个流收完再返回等于把流式变成非流式，
//!    客户端的「打字机效果」会退化成「卡十秒然后全部出现」。所以用
//!    转码流自己拼 SSE 文本，经 mpsc 边收边发。
//! 2. **日志在响应结束后才落库**。流式请求的 token 用量只有在最后一帧才知道，
//!    提前写日志会得到一堆 `output_tokens: 0`。

use std::convert::Infallible;

use bytes::Bytes;
use futures_util::StreamExt as _;
use refract_core::{ErrorKind, GatewayError, PassKind, Protocol};
use refract_protocol::StreamAggregator;
use refract_router::{Diagnosis, InboundPayload, RoutedResponse, RoutedStream};
use refract_store::NewRequestLog;
use serde_json::Value;
use xitca_web::body::{Frame, RequestBody, ResponseBody, StreamBody};
use xitca_web::handler::body::Body;
use xitca_web::handler::params::Params;
use xitca_web::handler::state::StateRef;
use xitca_web::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, WebResponse, header};

use crate::auth::{Principal, require_gateway};
use crate::error::{AppError, ProtocolRejection, collect_limited, json_response};
use crate::state::AppState;

/// 单个推理请求体的硬上限。
///
/// 32 MiB 足以容纳常见的 base64 图片/音频请求，同时阻止无限聚合 body 把个人
/// 服务拖垮。流式读取时逐 chunk 检查，因此没有 `Content-Length` 的请求也受限。
const GATEWAY_BODY_LIMIT: usize = 32 * 1024 * 1024;

/// 允许透传到上游的入站请求头。
///
/// 白名单而非黑名单：请求头里混着鉴权、代理与传输层字段，漏掉任何一个都是
/// 事故。这四个是客户端合理需要传给上游的全部 ——
/// `anthropic-beta`/`openai-beta` 开启厂商预览功能，`http-referer`/`x-title`
/// 是 OpenRouter 系上游的应用归属约定。它们只在**同协议直通**时随请求发出。
const FORWARDED_HEADER_NAMES: [&str; 4] =
    ["anthropic-beta", "openai-beta", "http-referer", "x-title"];

/// 从入站请求头里筛出可透传的部分。
fn forwardable_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    FORWARDED_HEADER_NAMES
        .iter()
        .flat_map(|name| headers.get_all(*name).iter().map(move |v| (name, v)))
        .filter_map(|(name, value)| {
            let value = value.to_str().ok()?;
            Some(((*name).to_owned(), value.to_owned()))
        })
        .collect()
}

/// 给响应打上网关的请求标识，客户端报障时能直接对上网关日志里的那一行。
///
/// 用自有头而不是 `x-request-id`：后者是上游的标识，端到端透传给客户端
/// 拿去向上游报障用，覆盖它会把两个排障链路搅在一起。
fn tag_request_id(mut response: WebResponse, request_id: &str) -> WebResponse {
    if let Ok(value) = HeaderValue::from_str(request_id) {
        response.headers_mut().insert("x-refract-request-id", value);
    }
    response
}

fn protocol_body_error(err: AppError, protocol: Protocol) -> AppError {
    match err {
        AppError::PayloadTooLarge => AppError::Protocol(ProtocolRejection::new(
            GatewayError::new(ErrorKind::PayloadTooLarge, "request body too large"),
            protocol,
        )),
        AppError::BadRequest(message) => AppError::Protocol(ProtocolRejection::new(
            GatewayError::invalid_request(message),
            protocol,
        )),
        other => other,
    }
}

fn parse_json_body(raw: Bytes, protocol: Protocol) -> Result<JsonBody, AppError> {
    let routing: RoutingFields = serde_json::from_slice(&raw).map_err(|error| {
        AppError::Protocol(ProtocolRejection::new(
            GatewayError::invalid_request(format!("malformed request body: {error}")),
            protocol,
        ))
    })?;
    Ok(JsonBody {
        raw,
        model: routing.model,
        stream: routing.stream,
    })
}

async fn pass_json(
    state: &AppState,
    headers: &HeaderMap,
    addr: std::net::SocketAddr,
    body: &mut RequestBody,
    kind: PassKind,
) -> Result<WebResponse, AppError> {
    let protocol = kind.protocol();
    let principal = require_gateway(state, headers, None, protocol).await?;
    let raw = collect_limited(body, GATEWAY_BODY_LIMIT)
        .await
        .map_err(|err| protocol_body_error(err, protocol))?;
    let parsed = parse_json_body(raw, protocol)?;
    let request_id = uuid::Uuid::new_v4().to_string();
    let model = parsed
        .model
        .as_deref()
        .filter(|model| !model.is_empty())
        .ok_or_else(|| {
            AppError::Protocol(ProtocolRejection::with_id(
                GatewayError::invalid_request(
                    "missing required field `model` — the gateway needs it to route",
                ),
                protocol,
                request_id.clone(),
            ))
        })?
        .to_owned();
    passthrough_response(
        state.clone(),
        principal,
        kind,
        request_id,
        headers.clone(),
        model,
        parsed.raw,
        None,
        crate::peer_addr(addr),
    )
    .await
}

async fn pass_multipart(
    state: &AppState,
    headers: &HeaderMap,
    addr: std::net::SocketAddr,
    body: &mut RequestBody,
    kind: PassKind,
) -> Result<WebResponse, AppError> {
    let protocol = kind.protocol();
    let principal = require_gateway(state, headers, None, protocol).await?;
    let raw = collect_limited(body, MULTIPART_BODY_LIMIT)
        .await
        .map_err(|err| protocol_body_error(err, protocol))?;
    let request_id = uuid::Uuid::new_v4().to_string();
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let model = multipart_model(&raw).ok_or_else(|| {
        AppError::Protocol(ProtocolRejection::with_id(
            GatewayError::invalid_request(
                "multipart form must include a `model` field — \
                 the gateway needs it to route",
            ),
            protocol,
            request_id.clone(),
        ))
    })?;
    passthrough_response(
        state.clone(),
        principal,
        kind,
        request_id,
        headers.clone(),
        model,
        raw,
        content_type,
        crate::peer_addr(addr),
    )
    .await
}

macro_rules! json_handler {
    ($name:ident, $protocol:expr) => {
        /// 网关 JSON 入口。
        pub async fn $name(
            StateRef(state): StateRef<'_, AppState>,
            headers: &HeaderMap,
            addr: std::net::SocketAddr,
            Body(mut body): Body<RequestBody>,
            req: &xitca_web::http::WebRequest<()>,
        ) -> Result<WebResponse, AppError> {
            let principal = require_gateway(state, headers, req.uri().query(), $protocol).await?;
            let raw = collect_limited(&mut body, GATEWAY_BODY_LIMIT)
                .await
                .map_err(|err| protocol_body_error(err, $protocol))?;
            let parsed = parse_json_body(raw, $protocol)?;
            dispatch(
                state.clone(),
                principal,
                $protocol,
                headers.clone(),
                parsed,
                None,
                crate::peer_addr(addr),
            )
            .await
        }
    };
}

json_handler!(chat_completions, Protocol::Chat);
json_handler!(messages, Protocol::Messages);
json_handler!(responses, Protocol::Responses);

macro_rules! pass_json_handler {
    ($name:ident, $kind:expr) => {
        /// 网关 JSON 直通入口。
        pub async fn $name(
            StateRef(state): StateRef<'_, AppState>,
            headers: &HeaderMap,
            addr: std::net::SocketAddr,
            Body(mut body): Body<RequestBody>,
            req: &xitca_web::http::WebRequest<()>,
        ) -> Result<WebResponse, AppError> {
            let _ = req;
            pass_json(state, headers, addr, &mut body, $kind).await
        }
    };
}

pass_json_handler!(embeddings, PassKind::Embeddings);
pass_json_handler!(completions, PassKind::Completions);
pass_json_handler!(images_generations, PassKind::Images);
pass_json_handler!(audio_speech, PassKind::AudioSpeech);
pass_json_handler!(moderations, PassKind::Moderations);
pass_json_handler!(rerank, PassKind::Rerank);
pass_json_handler!(count_tokens, PassKind::CountTokens);

macro_rules! pass_multipart_handler {
    ($name:ident, $kind:expr) => {
        /// 网关 multipart 直通入口。
        pub async fn $name(
            StateRef(state): StateRef<'_, AppState>,
            headers: &HeaderMap,
            addr: std::net::SocketAddr,
            Body(mut body): Body<RequestBody>,
        ) -> Result<WebResponse, AppError> {
            pass_multipart(state, headers, addr, &mut body, $kind).await
        }
    };
}

pass_multipart_handler!(audio_transcriptions, PassKind::AudioTranscriptions);
pass_multipart_handler!(audio_translations, PassKind::AudioTranslations);
pass_multipart_handler!(image_edits, PassKind::ImageEdits);

/// `GET /v1/models`
pub async fn list_models(
    StateRef(state): StateRef<'_, AppState>,
    headers: &HeaderMap,
    req: &xitca_web::http::WebRequest<()>,
) -> Result<WebResponse, AppError> {
    let principal = require_gateway(state, headers, req.uri().query(), Protocol::Chat).await?;
    let names = visible_model_names(state, &principal);
    let now = chrono::Utc::now().timestamp();
    let data: Vec<Value> = names
        .iter()
        .map(|id| {
            serde_json::json!({
                "id": id,
                "object": "model",
                "created": now,
                "owned_by": "refract",
            })
        })
        .collect();
    Ok(json_response(
        StatusCode::OK,
        &serde_json::json!({ "object": "list", "data": data }),
    ))
}

/// `GET /v1/models/{*id}`
pub async fn get_model(
    StateRef(state): StateRef<'_, AppState>,
    headers: &HeaderMap,
    Params(model): Params<String>,
    req: &xitca_web::http::WebRequest<()>,
) -> Result<WebResponse, AppError> {
    let principal = require_gateway(state, headers, req.uri().query(), Protocol::Chat).await?;
    if model.is_empty() || !visible_model_names(state, &principal).contains(&model) {
        return Err(AppError::Protocol(ProtocolRejection::new(
            GatewayError::not_found(format!("model `{model}` does not exist")),
            Protocol::Chat,
        )));
    }
    Ok(json_response(
        StatusCode::OK,
        &serde_json::json!({
            "id": model,
            "object": "model",
            "created": chrono::Utc::now().timestamp(),
            "owned_by": "refract",
        }),
    ))
}

/// `GET /v1beta/models`
pub async fn list_models_gemini(
    StateRef(state): StateRef<'_, AppState>,
    headers: &HeaderMap,
    req: &xitca_web::http::WebRequest<()>,
) -> Result<WebResponse, AppError> {
    let principal = require_gateway(state, headers, req.uri().query(), Protocol::Gemini).await?;
    let models: Vec<Value> = visible_model_names(state, &principal)
        .into_iter()
        .map(|name| {
            serde_json::json!({
                "name": format!("models/{name}"),
                "displayName": name,
                "supportedGenerationMethods": ["generateContent", "streamGenerateContent"],
            })
        })
        .collect();
    Ok(json_response(
        StatusCode::OK,
        &serde_json::json!({ "models": models }),
    ))
}

/// `GET /v1beta/models/{*rest}`
pub async fn get_model_gemini(
    StateRef(state): StateRef<'_, AppState>,
    headers: &HeaderMap,
    Params(rest): Params<String>,
    req: &xitca_web::http::WebRequest<()>,
) -> Result<WebResponse, AppError> {
    let principal = require_gateway(state, headers, req.uri().query(), Protocol::Gemini).await?;
    let raw = rest.trim_matches('/');
    let model = raw.strip_prefix("models/").unwrap_or(raw).to_owned();
    if model.is_empty() || !visible_model_names(state, &principal).contains(&model) {
        return Err(AppError::Protocol(ProtocolRejection::new(
            GatewayError::not_found(format!("model `{model}` does not exist")),
            Protocol::Gemini,
        )));
    }
    Ok(json_response(
        StatusCode::OK,
        &serde_json::json!({
            "name": format!("models/{model}"),
            "displayName": model,
            "supportedGenerationMethods": ["generateContent", "streamGenerateContent"],
        }),
    ))
}

/// `POST /v1beta/models/{*rest}`
pub async fn gemini_action(
    StateRef(state): StateRef<'_, AppState>,
    headers: &HeaderMap,
    addr: std::net::SocketAddr,
    Params(rest): Params<String>,
    Body(mut body): Body<RequestBody>,
    req: &xitca_web::http::WebRequest<()>,
) -> Result<WebResponse, AppError> {
    let principal = require_gateway(state, headers, req.uri().query(), Protocol::Gemini).await?;
    let spec = rest.trim_matches('/');
    let spec = spec.strip_prefix("models/").unwrap_or(spec);
    let (model, verb) = spec.split_once(':').unwrap_or((spec, "generateContent"));
    let pass_kind = match verb {
        "countTokens" => Some(PassKind::GeminiCountTokens),
        "embedContent" => Some(PassKind::GeminiEmbed),
        "batchEmbedContents" => Some(PassKind::GeminiBatchEmbed),
        _ => None,
    };
    let raw = collect_limited(&mut body, GATEWAY_BODY_LIMIT)
        .await
        .map_err(|err| protocol_body_error(err, Protocol::Gemini))?;
    if let Some(kind) = pass_kind {
        let request_id = uuid::Uuid::new_v4().to_string();
        return passthrough_response(
            state.clone(),
            principal,
            kind,
            request_id,
            headers.clone(),
            model.to_owned(),
            raw,
            None,
            crate::peer_addr(addr),
        )
        .await;
    }
    if !matches!(verb, "generateContent" | "streamGenerateContent") {
        return Err(AppError::Protocol(ProtocolRejection::new(
            GatewayError::invalid_request(format!(
                "unsupported Gemini action `:{verb}`; supported: generateContent, \
                 streamGenerateContent, countTokens, embedContent, batchEmbedContents"
            )),
            Protocol::Gemini,
        )));
    }
    let parsed = parse_json_body(raw, Protocol::Gemini)?;
    dispatch(
        state.clone(),
        principal,
        Protocol::Gemini,
        headers.clone(),
        parsed,
        Some(GeminiPath {
            model: model.to_owned(),
            stream: verb.starts_with("stream"),
        }),
        crate::peer_addr(addr),
    )
    .await
}

/// 原始 JSON 与路由所需的轻量字段；完整 IR 由 executor 按需构造。
struct JsonBody {
    raw: Bytes,
    model: Option<String>,
    stream: bool,
}

struct GeminiPath {
    model: String,
    stream: bool,
}

/// multipart 请求体的大小上限：音频文件（OpenAI 限 25MB）加表单开销。
const MULTIPART_BODY_LIMIT: usize = 64 * 1024 * 1024;

/// 直通请求的分发与执行。
///
/// 与 [`dispatch`] 的差别：候选被过滤为入口协议的原生端点（透传无转码
/// 路径），无流式分支，用量从响应体尽力提取。
#[allow(clippy::too_many_arguments)]
async fn passthrough_response(
    state: AppState,
    principal: Principal,
    kind: PassKind,
    request_id: String,
    headers: HeaderMap,
    model: String,
    raw: Bytes,
    content_type: Option<String>,
    remote_addr: Option<std::net::SocketAddr>,
) -> Result<WebResponse, AppError> {
    let inbound = kind.protocol();
    let started = std::time::Instant::now();

    enforce_ip_limit(&state, remote_addr.map(|a| a.ip()), inbound, &request_id)?;
    if !principal.allows_model(&model) {
        return Err(AppError::Protocol(ProtocolRejection::with_id(
            GatewayError::new(
                refract_core::ErrorKind::PermissionDenied,
                format!("this API key is not allowed to use model `{model}`"),
            ),
            inbound,
            request_id,
        )));
    }
    enforce_rate_limit(&state, &principal, inbound, &request_id)?;
    let _concurrency_permit = enforce_global_limits(&state, inbound, &request_id)?;

    // 可见性由 channels_for 保证（共享 + 本用户私有）；allows_channel 只做
    // tenant owner_id 与密钥标签过滤，owner_id 不是 user_id。
    let channels = state.channels_for(principal.gateway_user_id());
    let allowed_channels: Vec<_> = channels
        .iter()
        .filter(|channel| principal.allows_channel(channel))
        .collect();
    let mut route = {
        let mut rng = rand::rng();
        state
            .planner()
            .plan(allowed_channels.iter().copied(), &model, inbound, &mut rng)
    };
    route
        .attempts
        .retain(|candidate| candidate.protocol() == inbound);

    let affinity = resolve_affinity(&state, &principal, inbound, &headers, &raw, &model);
    route.identity = principal.key_id().map(|id| id as u64);
    if let Some(decision) = &affinity
        && let Some(bound) = decision.binding
        && !route.pin_channel(bound)
    {
        drop_affinity_binding(&state, decision, bound);
    }
    let capture_bodies = state.capture_bodies();
    let context = DispatchContext {
        state: state.clone(),
        principal,
        inbound,
        request_id,
        started,
        model: model.clone(),
        stream: false,
        forward_headers: forwardable_headers(&headers),
        capture_bodies,
        request_snapshot: (capture_bodies
            && !content_type
                .as_deref()
                .is_some_and(|ct| ct.starts_with("multipart/")))
        .then(|| body_snapshot(&raw)),
        affinity,
    };

    if route.is_empty() {
        let err = GatewayError::not_found(format!(
            "no enabled {proto}-protocol endpoint provides model `{model}`; \
             this endpoint passes bytes through {proto} endpoints only — \
             add the model to a {proto} endpoint's model list",
            proto = inbound.as_str(),
        ));
        log_failure(&context, &err);
        return Err(context.reject(err));
    }

    let pinned = context.affinity.as_ref().and_then(|d| d.binding);
    let pinned_only = context
        .affinity
        .as_ref()
        .is_some_and(|d| d.skip_retry_on_failure);
    let route = match pinned {
        Some(_) if pinned_only => route.pinned_only(),
        _ => route,
    };
    let outcome = match context
        .state
        .executor()
        .execute_passthrough(
            &route,
            refract_core::Action::Passthrough(kind),
            &raw,
            &context.forward_headers,
            content_type.as_deref(),
        )
        .await
    {
        Ok(outcome) => outcome,
        Err(error) => {
            if let Some(decision) = &context.affinity {
                settle_affinity_on_failure(&context.state, decision);
            }
            log_failure(&context, &error);
            return Err(context.reject(error));
        }
    };
    if let Some(decision) = &context.affinity {
        context
            .state
            .affinity()
            .record(decision, outcome.channel_id);
    }

    let usage = passthrough_usage(kind, &outcome.payload.body);
    let response_snapshot = (context.capture_bodies && snapshot_worthy(&outcome.payload.headers))
        .then(|| body_snapshot(&outcome.payload.body));
    let response = tag_request_id(native_unary_response(&outcome.payload), &context.request_id);
    let key_id = context.principal.key_id();
    let mut entry = NewRequestLog::new(
        context.principal.owner_id,
        context.request_id,
        key_id,
        inbound,
        model,
        false,
    )
    .with_channel(
        outcome.channel_id,
        outcome.channel_name,
        outcome.upstream_protocol,
        outcome.upstream_model,
    )
    .with_timing(
        Some(outcome.latency_ms),
        started.elapsed().as_millis() as u64,
    )
    .with_snapshots(context.request_snapshot, response_snapshot)
    .with_routing_context(
        outcome.credential_hint,
        context.affinity.as_ref().map(|d| d.rule_name.clone()),
    );
    entry.input_tokens = usage;
    entry.status = response.status().as_u16();
    entry.retries = u32::from(outcome.attempts.saturating_sub(1));
    entry.user_id = context.principal.user_id;
    record(
        &context.state,
        entry,
        key_id,
        usage,
        context.principal.user_id,
    );

    Ok(response)
}

/// 尽力从直通响应提取输入侧用量；解析失败不影响透传。
fn passthrough_usage(kind: PassKind, body: &Bytes) -> u64 {
    #[derive(serde::Deserialize, Default)]
    struct Envelope {
        #[serde(default)]
        usage: Option<UsageFields>,
        #[serde(default)]
        input_tokens: Option<u64>,
        #[serde(default, rename = "totalTokens")]
        total_tokens: Option<u64>,
    }
    #[derive(serde::Deserialize, Default)]
    struct UsageFields {
        #[serde(default)]
        prompt_tokens: u64,
    }
    let envelope = serde_json::from_slice::<Envelope>(body).unwrap_or_default();
    match kind {
        PassKind::Embeddings | PassKind::Completions => {
            envelope.usage.unwrap_or_default().prompt_tokens
        }
        PassKind::CountTokens => envelope.input_tokens.unwrap_or_default(),
        PassKind::GeminiCountTokens => envelope.total_tokens.unwrap_or_default(),
        _ => 0,
    }
}

/// 从 multipart 字节里提取 `model` 字段值。
fn multipart_model(raw: &[u8]) -> Option<String> {
    const MARKER: &[u8] = b"name=\"model\"";
    let mut search_from = 0usize;
    loop {
        let rest = &raw[search_from..];
        let marker_at = rest
            .windows(MARKER.len())
            .position(|window| window == MARKER)?;
        let abs = search_from + marker_at;
        let attr_boundary = abs == 0 || {
            let prev = raw[abs - 1];
            prev == b';' || prev == b' ' || prev == b'\t'
        };
        if attr_boundary {
            let after = &raw[abs..];
            let start = after.windows(4).position(|w| w == b"\r\n\r\n")? + 4;
            let len = after[start..].windows(2).position(|w| w == b"\r\n")?;
            let value = std::str::from_utf8(&after[start..start + len]).ok()?;
            let value = value.trim();
            return (!value.is_empty()).then(|| value.to_owned());
        }
        search_from = abs + MARKER.len();
    }
}

fn visible_model_names(state: &AppState, principal: &Principal) -> Vec<String> {
    let channels = state.channels_for(principal.gateway_user_id());
    let allowed_channels: Vec<_> = channels
        .iter()
        .filter(|channel| principal.allows_channel(channel))
        .collect();
    let mut names: Vec<String> = state
        .planner()
        .visible_models(allowed_channels)
        .into_iter()
        .filter(|name| principal.allows_model(name))
        .collect();
    names.sort_unstable();
    names.dedup();
    names
}

/// 管理面 Playground 的入口：用本地身份直接走完整的分发管线。
///
/// 不发放临时网关密钥、不绕过任何路由/熔断/日志逻辑 —— Playground 请求
/// 与真实客户端请求唯一的区别是鉴权面（管理令牌而非网关密钥）。
pub(crate) async fn playground_chat(state: AppState, raw: Bytes) -> Result<WebResponse, AppError> {
    let routing: RoutingFields = serde_json::from_slice(&raw).map_err(|error| {
        AppError::Protocol(ProtocolRejection::new(
            GatewayError::invalid_request(format!("malformed request body: {error}")),
            Protocol::Chat,
        ))
    })?;
    let body = JsonBody {
        raw,
        model: routing.model,
        stream: routing.stream,
    };
    dispatch(
        state,
        Principal::local(refract_core::DEFAULT_OWNER_ID),
        Protocol::Chat,
        HeaderMap::new(),
        body,
        None,
        None,
    )
    .await
}

#[derive(serde::Deserialize)]
struct RoutingFields {
    model: Option<String>,
    #[serde(default)]
    stream: bool,
}

/// 正文快照的大小上限。超过就截断 —— 日志是排障凭据不是归档，
/// 一张 base64 图片能把 SQLite 行撑到没法在界面里打开。
const BODY_SNAPSHOT_LIMIT: usize = 64 * 1024;

/// 把请求/响应字节渲染成可入库的文本快照（UTF-8 宽容 + 截断标记）。
fn body_snapshot(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    if text.len() <= BODY_SNAPSHOT_LIMIT {
        return text.into_owned();
    }
    let mut cut = BODY_SNAPSHOT_LIMIT;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!(
        "{}\n…[truncated, {} bytes total]",
        &text[..cut],
        bytes.len()
    )
}

/// 响应是否值得存文本快照 —— 音频等二进制存进 TEXT 列只会产生乱码。
fn snapshot_worthy(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_none_or(|ct| {
            ct.starts_with("application/json") || ct.starts_with("text/") || ct.contains("+json")
        })
}

/// 单次网关执行在普通响应和流式响应间共享的元数据。
struct DispatchContext {
    state: AppState,
    principal: Principal,
    inbound: Protocol,
    request_id: String,
    started: std::time::Instant,
    model: String,
    stream: bool,
    /// 按白名单过滤后的入站请求头，同协议直通时透传给上游。
    forward_headers: Vec<(String, String)>,
    /// 是否记录正文快照（跟随全局设置，在入口处取一次保持一致）。
    capture_bodies: bool,
    /// 请求正文快照。multipart 等二进制请求为 `None`。
    request_snapshot: Option<String>,
    /// 本次请求命中的亲和规则决策；`None` = 无规则匹配或功能关闭。
    affinity: Option<refract_router::AffinityDecision>,
}

impl DispatchContext {
    /// 构造带请求标识的协议错误，失败响应也能对上日志。
    fn reject(&self, error: GatewayError) -> AppError {
        AppError::Protocol(ProtocolRejection::with_id(
            error,
            self.inbound,
            self.request_id.clone(),
        ))
    }
}

/// 按入口协议推导亲和规则匹配用的请求路径。
///
/// Gemini 的实际路径随模型与动作变化，这里给出前缀锚点 —— 规则的
/// path 正则按前缀匹配即可覆盖。
fn inbound_path(inbound: Protocol) -> &'static str {
    match inbound {
        Protocol::Chat => "/v1/chat/completions",
        Protocol::Responses => "/v1/responses",
        Protocol::Messages => "/v1/messages",
        Protocol::Gemini => "/v1beta/models",
    }
}

/// 路由前解析渠道亲和性：命中规则返回决策，功能关闭或无匹配返回 `None`。
fn resolve_affinity(
    state: &AppState,
    principal: &Principal,
    inbound: Protocol,
    headers: &HeaderMap,
    body: &Bytes,
    model: &str,
) -> Option<refract_router::AffinityDecision> {
    let engine = state.affinity();
    if !engine.is_active() {
        return None;
    }
    // 懒解析：仅当确有规则需要 Body 来源时才解析请求体 JSON。
    let parsed_body = engine
        .needs_body()
        .then(|| serde_json::from_slice::<Value>(body).ok())
        .flatten();
    let ctx = refract_router::AffinityContext {
        model,
        path: inbound_path(inbound),
        api_key_id: principal.key_id().map(|id| id as u64),
        headers,
        body: parsed_body.as_ref(),
    };
    engine.resolve(&ctx)
}

/// 绑定的渠道不在本次候选里时，按原因决定是否遗忘绑定。
///
/// - 渠道已删除 → 永远遗忘；
/// - 渠道停用/自动禁用 → 跟随 `keep_on_channel_disabled`；
/// - 渠道存在且启用（本次因模型/协议/API key 过滤而缺席）→ 保留绑定，
///   会话换个模型再来时还能回到它。
fn drop_affinity_binding(
    state: &AppState,
    decision: &refract_router::AffinityDecision,
    bound: refract_core::ChannelId,
) {
    let channels = state.channels();
    match channels.iter().find(|c| c.id == bound) {
        None => state.affinity().forget(&decision.cache_key),
        Some(channel) if !channel.enabled || channel.auto_disabled => {
            if !state.affinity().keep_on_channel_disabled() {
                state.affinity().forget(&decision.cache_key);
            }
        }
        Some(_) => {}
    }
}

/// 亲和绑定存在且钉住渠道失败时的收尾。
///
/// `skip_retry_on_failure` = true 表示「绑定即承诺」：保留绑定，错误原样
/// 返回，不偷偷换渠道（换渠道会让会话漂移，违背规则意图）。否则遗忘
/// 绑定，让下一次请求重新竞争。
fn settle_affinity_on_failure(state: &AppState, decision: &refract_router::AffinityDecision) {
    if !decision.skip_retry_on_failure {
        state.affinity().forget(&decision.cache_key);
    }
}

/// 请求分发的核心。
///
/// 所有四个协议汇聚到这里 —— 差异已经在 codec 里被吸收掉了。
async fn dispatch(
    state: AppState,
    principal: Principal,
    inbound: Protocol,
    headers: HeaderMap,
    body: JsonBody,
    gemini_path: Option<GeminiPath>,
    remote_addr: Option<std::net::SocketAddr>,
) -> Result<WebResponse, AppError> {
    let started = std::time::Instant::now();
    let request_id = uuid::Uuid::new_v4().to_string();

    enforce_ip_limit(&state, remote_addr.map(|a| a.ip()), inbound, &request_id)?;
    // Gemini 从路径取路由字段，另外三种协议只轻量读取顶层 model/stream。
    let (model, stream) = match gemini_path {
        Some(path) => (path.model, path.stream),
        None => {
            let model = body
                .model
                .as_deref()
                .filter(|model| !model.is_empty())
                .ok_or_else(|| {
                    AppError::Protocol(ProtocolRejection::with_id(
                        GatewayError::invalid_request("missing required field `model`"),
                        inbound,
                        request_id.clone(),
                    ))
                })?
                .to_owned();
            (model, body.stream)
        }
    };

    // 密钥的模型白名单 —— 在路由之前拦截，避免「路由成功但无权访问」的混乱语义。
    if !principal.allows_model(&model) {
        return Err(AppError::Protocol(ProtocolRejection::with_id(
            GatewayError::new(
                refract_core::ErrorKind::PermissionDenied,
                format!("this API key is not allowed to use model `{model}`"),
            ),
            inbound,
            request_id,
        )));
    }
    enforce_rate_limit(&state, &principal, inbound, &request_id)?;
    let concurrency_permit = enforce_global_limits(&state, inbound, &request_id)?;

    let channels = state.channels_for(principal.gateway_user_id());
    let allowed_channels: Vec<_> = channels
        .iter()
        .filter(|channel| principal.allows_channel(channel))
        .collect();
    // 渠道亲和性：路由前解析身份值，把已绑定的渠道钉到候选最前。
    let affinity = resolve_affinity(&state, &principal, inbound, &headers, &body.raw, &model);
    let mut route = {
        let mut rng = rand::rng();
        state
            .planner()
            .plan(allowed_channels.iter().copied(), &model, inbound, &mut rng)
    };
    // 黏性密钥策略的锚点：同一调用者在同一渠道固定同一把 key。
    route.identity = principal.key_id().map(|id| id as u64);
    // 绑定移到最前；绑定的渠道没进候选时区分原因再决定是否遗忘绑定。
    if let Some(decision) = &affinity
        && let Some(bound) = decision.binding
        && !route.pin_channel(bound)
    {
        drop_affinity_binding(&state, decision, bound);
    }
    let capture_bodies = state.capture_bodies();
    let context = DispatchContext {
        state: state.clone(),
        principal,
        inbound,
        request_id,
        started,
        model: model.clone(),
        stream,
        forward_headers: forwardable_headers(&headers),
        capture_bodies,
        request_snapshot: capture_bodies.then(|| body_snapshot(&body.raw)),
        affinity: affinity.clone(),
    };

    if route.is_empty() {
        // 空路由的原因有三种，给出的错误必须能指导用户改配置 ——
        // 「没有可用渠道」是最没用的错误信息。
        let err = match state
            .planner()
            .diagnose(allowed_channels.iter().copied(), &model, inbound)
        {
            Diagnosis::UnknownModel => {
                GatewayError::not_found(format!("no enabled channel provides model `{}`", model))
            }
            Diagnosis::ProtocolNotPermitted { available } => {
                GatewayError::invalid_request(format!(
                    "model `{}` is not reachable over the {inbound} protocol; \
                 enable protocol conversion on the channel, or use one of: {}",
                    model,
                    available
                        .iter()
                        .map(|p| p.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            }
            // 诊断说可路由但计划为空 —— 只可能是所有候选都在熔断中。
            Diagnosis::Routable => GatewayError::no_channel(format!(
                "every endpoint serving `{}` is currently suspended by the circuit breaker",
                model
            )),
        };
        log_failure(&context, &err);
        return Err(context.reject(err));
    }

    let pinned = affinity.as_ref().and_then(|d| d.binding);
    if stream {
        stream_response(context, body, route, pinned, concurrency_permit).await
    } else {
        // unary 在本函数栈上持有 permit：响应体已完整构造才返回。
        let response = unary_response(context, body, route, pinned).await;
        drop(concurrency_permit);
        response
    }
}

/// 非流式响应。
async fn unary_response(
    context: DispatchContext,
    raw: JsonBody,
    route: refract_router::Route<'_>,
    pinned: Option<refract_core::ChannelId>,
) -> Result<WebResponse, AppError> {
    // 亲和钉住且 skip_retry_on_failure：绑定即承诺，失败不偷偷换渠道
    // 造成会话漂移 —— 错误原样返回，绑定保留。
    let pinned_only = context
        .affinity
        .as_ref()
        .is_some_and(|d| d.skip_retry_on_failure);
    let route = match pinned {
        Some(_) if pinned_only => route.pinned_only(),
        _ => route,
    };
    let outcome = match context
        .state
        .executor()
        .execute(
            &route,
            InboundPayload::raw(context.inbound, &raw.raw, &context.model, context.stream)
                .with_headers(&context.forward_headers),
        )
        .await
    {
        Ok(o) => o,
        Err(e) => {
            if let Some(decision) = &context.affinity {
                settle_affinity_on_failure(&context.state, decision);
            }
            log_failure(&context, &e);
            return Err(context.reject(e));
        }
    };
    let DispatchContext {
        state,
        principal,
        inbound,
        request_id,
        started,
        model,
        capture_bodies,
        request_snapshot,
        affinity,
        ..
    } = context;
    let owner_id = principal.owner_id;
    let key_id = principal.key_id();
    let user_id = principal.user_id;
    // 成功 → 写入/刷新亲和绑定（switch_on_success 决定失败后兜底是否改绑）。
    if let Some(decision) = &affinity {
        state.affinity().record(decision, outcome.channel_id);
    }
    let affinity_rule = affinity.map(|d| d.rule_name);

    let usage = outcome
        .payload
        .usage()
        .billing_normalized(outcome.upstream_protocol);
    let mut response_snapshot = None;
    let response = match &outcome.payload {
        RoutedResponse::Native { response, .. } => {
            if capture_bodies && snapshot_worthy(&response.headers) {
                response_snapshot = Some(body_snapshot(&response.body));
            }
            native_unary_response(response)
        }
        RoutedResponse::Transcoded(payload) => {
            let body = state
                .codecs()
                .for_protocol(inbound)
                .encode_response(payload)
                .map_err(|e| {
                    AppError::Protocol(ProtocolRejection::with_id(e, inbound, request_id.clone()))
                })?;
            if capture_bodies {
                response_snapshot = Some(body_snapshot(body.to_string().as_bytes()));
            }
            json_response(StatusCode::OK, &body)
        }
    };
    let response = tag_request_id(response, &request_id);
    let response_status = response.status().as_u16();
    let mut entry = NewRequestLog::new(owner_id, request_id, key_id, inbound, model, false)
        .with_channel(
            outcome.channel_id,
            outcome.channel_name.clone(),
            outcome.upstream_protocol,
            outcome.upstream_model.clone(),
        )
        .with_tokens(
            usage.input_tokens,
            usage.output_tokens,
            usage.cached_input_tokens,
            usage.cache_write_tokens,
            usage.reasoning_tokens,
        )
        .with_timing(
            Some(outcome.latency_ms),
            started.elapsed().as_millis() as u64,
        )
        .with_snapshots(request_snapshot, response_snapshot)
        .with_routing_context(outcome.credential_hint.clone(), affinity_rule);
    entry.status = response_status;
    entry.retries = u32::from(outcome.attempts.saturating_sub(1));
    entry.user_id = user_id;
    record(&state, entry, key_id, usage.total(), user_id);

    Ok(response)
}

/// 构造同协议非流式响应，并只保留可跨连接转发的 headers。
fn native_unary_response(upstream: &refract_upstream::UpstreamRawResponse) -> WebResponse {
    let mut response = WebResponse::new(ResponseBody::bytes(upstream.body.clone()));
    if let Ok(status) = StatusCode::from_u16(upstream.status) {
        *response.status_mut() = status;
    }
    copy_end_to_end_headers(&upstream.headers, response.headers_mut(), false);
    if !response.headers().contains_key(header::CONTENT_TYPE) {
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
    }
    response
}

/// 流式响应。
///
/// 关键在于**不等流结束**：响应体是 `StreamBody`，我们把上游流
/// 包装后直接交出去。转码逐帧发生，日志在流的末尾用 `finally` 语义补写。
async fn stream_response(
    dispatch: DispatchContext,
    raw: JsonBody,
    route: refract_router::Route<'_>,
    pinned: Option<refract_core::ChannelId>,
    concurrency_permit: Option<tokio::sync::OwnedSemaphorePermit>,
) -> Result<WebResponse, AppError> {
    let pinned_only = dispatch
        .affinity
        .as_ref()
        .is_some_and(|d| d.skip_retry_on_failure);
    let route = match pinned {
        Some(_) if pinned_only => route.pinned_only(),
        _ => route,
    };
    let outcome = match dispatch
        .state
        .executor()
        .execute_stream(
            &route,
            InboundPayload::raw(dispatch.inbound, &raw.raw, &dispatch.model, dispatch.stream)
                .with_headers(&dispatch.forward_headers),
        )
        .await
    {
        Ok(o) => o,
        Err(e) => {
            if let Some(decision) = &dispatch.affinity {
                settle_affinity_on_failure(&dispatch.state, decision);
            }
            log_failure(&dispatch, &e);
            return Err(dispatch.reject(e));
        }
    };
    // 流式「成功」= 钉住渠道已经开始服务（200 + 流已开）：绑定在这里写入。
    if let Some(decision) = &dispatch.affinity {
        dispatch
            .state
            .affinity()
            .record(decision, outcome.channel_id);
    }
    let DispatchContext {
        state,
        principal,
        inbound,
        request_id,
        started,
        model,
        capture_bodies,
        request_snapshot,
        affinity,
        ..
    } = dispatch;

    let context = StreamContext {
        state,
        owner_id: principal.owner_id,
        key_id: principal.key_id(),
        user_id: principal.user_id,
        inbound,
        request_id: request_id.clone(),
        started,
        channel_id: outcome.channel_id,
        channel_name: outcome.channel_name,
        upstream_protocol: outcome.upstream_protocol,
        upstream_model: outcome.upstream_model,
        attempts: outcome.attempts,
        ttfb_ms: outcome.latency_ms,
        model,
        capture_bodies,
        request_snapshot,
        credential_hint: outcome.credential_hint,
        affinity,
        _concurrency_permit: concurrency_permit,
    };

    let response = match outcome.payload {
        RoutedStream::Native(response) => native_stream_response(context, response),
        RoutedStream::Transcoded(stream) => transcoded_stream_response(context, stream),
    };
    Ok(tag_request_id(response, &request_id))
}

struct StreamContext {
    state: AppState,
    owner_id: i64,
    key_id: Option<i64>,
    user_id: Option<i64>,
    inbound: Protocol,
    request_id: String,
    started: std::time::Instant,
    channel_id: refract_core::ChannelId,
    channel_name: String,
    upstream_protocol: Protocol,
    upstream_model: String,
    attempts: u8,
    ttfb_ms: u64,
    model: String,
    capture_bodies: bool,
    request_snapshot: Option<String>,
    /// 实际使用的上游密钥脱敏提示。
    credential_hint: Option<String>,
    /// 命中的亲和决策：流中途失败时据此决定是否解绑。
    affinity: Option<refract_router::AffinityDecision>,
    /// 全局并发 permit —— 流式响应要占用额度直到流结束（finalize 时 drop）。
    _concurrency_permit: Option<tokio::sync::OwnedSemaphorePermit>,
}

async fn record_stream_endpoint_health(
    context: &StreamContext,
    error: Option<&GatewayError>,
    client_closed: bool,
) {
    if client_closed {
        tracing::warn!(
            channel_id = context.channel_id,
            protocol = %context.upstream_protocol,
            "client dropped stream before completion; health stats not updated"
        );
        return;
    }
    let result = match error {
        Some(error)
            if matches!(
                error.kind,
                ErrorKind::Unauthenticated
                    | ErrorKind::PermissionDenied
                    | ErrorKind::RateLimited
                    | ErrorKind::UpstreamError
                    | ErrorKind::Timeout
                    | ErrorKind::Configuration
                    | ErrorKind::NotFound
            ) =>
        {
            match context
                .state
                .executor()
                .health()
                .record_failure(
                    context.channel_id,
                    context.upstream_protocol,
                    &error.to_string(),
                    error.retry_after,
                )
                .await
            {
                Ok(health) => {
                    // 流式中途失败同样要进事件管道 —— 自动禁用与告警
                    // 不能只看非流式请求。
                    context
                        .state
                        .emit_router_event(refract_router::RouterEvent::Failure {
                            channel_id: context.channel_id,
                            channel_name: context.channel_name.clone(),
                            protocol: context.upstream_protocol,
                            kind: error.kind,
                            message: error.message.clone(),
                            suspended: health.suspended_until.is_some(),
                            consecutive_fails: health.consecutive_fails,
                        });
                    Ok(())
                }
                Err(e) => Err(e),
            }
        }
        Some(_) => return,
        None => {
            match context
                .state
                .executor()
                .health()
                .record_success(
                    context.channel_id,
                    context.upstream_protocol,
                    context.ttfb_ms,
                )
                .await
            {
                Ok(recovered) => {
                    context
                        .state
                        .emit_router_event(refract_router::RouterEvent::Success {
                            channel_id: context.channel_id,
                            channel_name: context.channel_name.clone(),
                            protocol: context.upstream_protocol,
                            recovered,
                        });
                    Ok(())
                }
                Err(e) => Err(e),
            }
        }
    };
    if let Err(error) = result {
        tracing::warn!(error = %error, "failed to persist terminal stream health");
    }
}

/// 跨协议流：解析上游事件并编码成入口协议。
fn transcoded_stream_response(
    context: StreamContext,
    mut upstream: refract_upstream::SseStream,
) -> WebResponse {
    let codecs = context.state.codecs();
    let decoder = codecs
        .for_protocol(context.upstream_protocol)
        .stream_decoder();
    let encoder = codecs.for_protocol(context.inbound).stream_encoder();

    // 转码流与 codec 状态机只保证 `Send`。把生产者放到独立 Tokio task，
    // 响应侧只持有线程安全的 mpsc Receiver。
    let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(32);
    let keep_tx = tx.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(15));
        interval.tick().await;
        loop {
            interval.tick().await;
            if keep_tx
                .send(Bytes::from_static(b": keep-alive\n\n"))
                .await
                .is_err()
            {
                break;
            }
        }
    });
    tokio::spawn(async move {
        let mut decoder = decoder;
        let mut encoder = encoder;
        let mut aggregator = StreamAggregator::new();
        let mut stream_error: Option<GatewayError> = None;
        let mut client_closed = false;
        let mut saw_done = false;

        'upstream: while let Some(event) = upstream.next().await {
            let sse = match event {
                Ok(value) => value,
                Err(error) => {
                    stream_error = Some(error);
                    break 'upstream;
                }
            };
            let frame = refract_protocol::SseFrame {
                event: (!sse.event.is_empty()).then_some(sse.event),
                data: sse.data,
            };
            let events = match decoder.decode(&frame) {
                Ok(value) => value,
                Err(error) => {
                    stream_error = Some(error);
                    break 'upstream;
                }
            };
            for event in events {
                if let refract_protocol::StreamEvent::Error { message, .. } = &event {
                    stream_error =
                        Some(GatewayError::new(ErrorKind::UpstreamError, message.clone()));
                    break 'upstream;
                }
                if matches!(event, refract_protocol::StreamEvent::Done) {
                    saw_done = true;
                }
                aggregator.absorb(&event);
                let frames = match encoder.encode(&event) {
                    Ok(value) => value,
                    Err(error) => {
                        stream_error = Some(error);
                        break 'upstream;
                    }
                };
                if !send_frames(&tx, frames).await {
                    client_closed = true;
                    break 'upstream;
                }
            }
        }

        if stream_error.is_none()
            && !client_closed
            && context.upstream_protocol != Protocol::Gemini
            && !saw_done
        {
            stream_error = Some(GatewayError::new(
                ErrorKind::UpstreamError,
                "upstream stream ended without its protocol completion event",
            ));
        }

        if stream_error.is_none() && !client_closed {
            match decoder.finish() {
                Ok(events) => {
                    for event in events {
                        aggregator.absorb(&event);
                        match encoder.encode(&event) {
                            Ok(frames) => {
                                if !send_frames(&tx, frames).await {
                                    client_closed = true;
                                    break;
                                }
                            }
                            Err(error) => {
                                stream_error = Some(error);
                                break;
                            }
                        }
                    }
                }
                Err(error) => stream_error = Some(error),
            }
            if stream_error.is_none() && !client_closed {
                match encoder.finish() {
                    Ok(frames) => {
                        if !send_frames(&tx, frames).await {
                            client_closed = true;
                        }
                    }
                    Err(error) => stream_error = Some(error),
                }
            }
        }

        // 已经产生错误时，尽力发一个目标协议可识别的错误事件。
        if let Some(error) = &stream_error {
            let event = refract_protocol::StreamEvent::Error {
                message: error.message.clone(),
                kind: format!("{:?}", error.kind).to_lowercase(),
            };
            aggregator.absorb(&event);
            if let Ok(frames) = encoder.encode(&event)
                && !send_frames(&tx, frames).await
            {
                client_closed = true;
            }
        }

        let usage = aggregator
            .usage
            .billing_normalized(context.upstream_protocol);
        record_stream_endpoint_health(&context, stream_error.as_ref(), client_closed).await;
        let (status, error_kind, error_message) = match stream_error.as_ref() {
            Some(error) => (
                error.kind.status(),
                Some(format!("{:?}", error.kind)),
                Some(error.message.clone()),
            ),
            None if client_closed => (
                499,
                Some("client_closed_request".to_owned()),
                Some("client disconnected before the stream completed".to_owned()),
            ),
            None => (200, None, None),
        };
        let response_body = context
            .capture_bodies
            .then(|| body_snapshot(aggregator.text_preview().as_bytes()))
            .filter(|text| !text.is_empty());
        let mut entry = NewRequestLog::new(
            context.owner_id,
            context.request_id,
            context.key_id,
            context.inbound,
            context.model,
            true,
        )
        .with_channel(
            context.channel_id,
            context.channel_name,
            context.upstream_protocol,
            context.upstream_model,
        )
        .with_tokens(
            usage.input_tokens,
            usage.output_tokens,
            usage.cached_input_tokens,
            usage.cache_write_tokens,
            usage.reasoning_tokens,
        )
        .with_timing(
            Some(context.ttfb_ms),
            context.started.elapsed().as_millis() as u64,
        )
        .with_snapshots(context.request_snapshot, response_body)
        .with_routing_context(
            context.credential_hint.clone(),
            context.affinity.as_ref().map(|d| d.rule_name.clone()),
        );
        entry.status = status;
        entry.retries = u32::from(context.attempts.saturating_sub(1));
        entry.error_kind = error_kind;
        entry.error_message = error_message;
        entry.user_id = context.user_id;
        // 上游中途出错（非客户端断开）→ 钉住的渠道失约，按策略解绑；
        // 只解指向本渠道的绑定，避免误伤 switch_on_success=false 下
        // 仍然指向旧渠道的绑定。
        if stream_error.is_some()
            && let Some(decision) = context.affinity.as_ref()
            && !decision.skip_retry_on_failure
        {
            context
                .state
                .affinity()
                .forget_if_bound_to(&decision.cache_key, context.channel_id);
        }
        record(
            &context.state,
            entry,
            context.key_id,
            usage.total(),
            context.user_id,
        );
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx)
        .map(|bytes: Bytes| Ok::<_, Infallible>(Frame::Data(bytes)));
    let mut response = WebResponse::new(ResponseBody::boxed(StreamBody::new(stream)));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response.headers_mut().insert(
        HeaderName::from_static("x-accel-buffering"),
        HeaderValue::from_static("no"),
    );
    response
}

/// 原生协议流：不解析、不重编码，逐字节转发上游 SSE。
fn native_stream_response(
    context: StreamContext,
    upstream: refract_upstream::UpstreamRawStream,
) -> WebResponse {
    let refract_upstream::UpstreamRawStream {
        status,
        headers,
        stream: upstream,
    } = upstream;
    let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(32);
    tokio::spawn(relay_native_stream(context, upstream, tx));

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx)
        .map(|bytes: Bytes| Ok::<_, Infallible>(Frame::Data(bytes)));
    let mut response = WebResponse::new(ResponseBody::boxed(StreamBody::new(stream)));
    if let Ok(status) = StatusCode::from_u16(status) {
        *response.status_mut() = status;
    }
    copy_end_to_end_headers(&headers, response.headers_mut(), true);
    // Content-Type 最后强制为 SSE：这个路径的响应体已通过预检确认是合法
    // SSE 流，上游把它错标成 text/plain（部分反代会这样）不该传染给客户端
    // —— 客户端 SDK 靠这个头决定按流解析还是整体读取。
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response.headers_mut().insert(
        HeaderName::from_static("x-accel-buffering"),
        HeaderValue::from_static("no"),
    );
    response
}

async fn relay_native_stream(
    context: StreamContext,
    mut upstream: refract_upstream::ByteStream,
    tx: tokio::sync::mpsc::Sender<Bytes>,
) {
    let mut stream_error: Option<GatewayError> = None;
    let mut client_closed = false;
    let mut parser = refract_protocol::SseParser::new();
    let mut decoder = context
        .state
        .codecs()
        .for_protocol(context.upstream_protocol)
        .stream_decoder();
    let mut aggregator = StreamAggregator::new();
    let mut saw_done = false;

    while let Some(chunk) = upstream.next().await {
        match chunk {
            Ok(bytes) => {
                let inspection = parser.feed_bytes(&bytes).and_then(|frames| {
                    inspect_native_frames(frames, decoder.as_mut(), &mut aggregator, &mut saw_done)
                });
                if tx.send(bytes).await.is_err() {
                    client_closed = true;
                    break;
                }
                if let Err(error) = inspection {
                    stream_error = Some(error);
                    break;
                }
            }
            Err(error) => {
                stream_error = Some(error);
                break;
            }
        }
    }

    if stream_error.is_none() && !client_closed {
        match parser.finish_bytes() {
            Ok(Some(frame)) => {
                if let Err(error) = inspect_native_frames(
                    vec![frame],
                    decoder.as_mut(),
                    &mut aggregator,
                    &mut saw_done,
                ) {
                    stream_error = Some(error);
                }
            }
            Ok(None) => {}
            Err(error) => stream_error = Some(error),
        }
    }
    if stream_error.is_none()
        && !client_closed
        && context.upstream_protocol != Protocol::Gemini
        && !saw_done
    {
        stream_error = Some(GatewayError::new(
            ErrorKind::UpstreamError,
            "upstream stream ended without its protocol completion event",
        ));
    }
    if stream_error.is_none() && !client_closed {
        match decoder.finish() {
            Ok(events) => {
                for event in events {
                    aggregator.absorb(&event);
                }
            }
            Err(error) => stream_error = Some(error),
        }
    }

    let usage = aggregator
        .usage
        .billing_normalized(context.upstream_protocol);
    record_stream_endpoint_health(&context, stream_error.as_ref(), client_closed).await;
    let (status, error_kind, error_message) = match stream_error.as_ref() {
        Some(error) => (
            error.kind.status(),
            Some(format!("{:?}", error.kind)),
            Some(error.message.clone()),
        ),
        None if client_closed => (
            499,
            Some("client_closed_request".to_owned()),
            Some("client disconnected before the stream completed".to_owned()),
        ),
        None => (200, None, None),
    };
    let response_body = context
        .capture_bodies
        .then(|| body_snapshot(aggregator.text_preview().as_bytes()))
        .filter(|text| !text.is_empty());
    let mut entry = NewRequestLog::new(
        context.owner_id,
        context.request_id,
        context.key_id,
        context.inbound,
        context.model,
        true,
    )
    .with_channel(
        context.channel_id,
        context.channel_name,
        context.upstream_protocol,
        context.upstream_model,
    )
    .with_tokens(
        usage.input_tokens,
        usage.output_tokens,
        usage.cached_input_tokens,
        usage.cache_write_tokens,
        usage.reasoning_tokens,
    )
    .with_timing(
        Some(context.ttfb_ms),
        context.started.elapsed().as_millis() as u64,
    )
    .with_snapshots(context.request_snapshot, response_body)
    .with_routing_context(
        context.credential_hint.clone(),
        context.affinity.as_ref().map(|d| d.rule_name.clone()),
    );
    entry.status = status;
    entry.retries = u32::from(context.attempts.saturating_sub(1));
    entry.error_kind = error_kind;
    entry.error_message = error_message;
    entry.user_id = context.user_id;
    // 与转码流一致的亲和收尾：只对仍指向本渠道的绑定生效。
    if stream_error.is_some()
        && let Some(decision) = context.affinity.as_ref()
        && !decision.skip_retry_on_failure
    {
        context
            .state
            .affinity()
            .forget_if_bound_to(&decision.cache_key, context.channel_id);
    }
    record(
        &context.state,
        entry,
        context.key_id,
        usage.total(),
        context.user_id,
    );
}

/// 复制端到端响应头；逐连接 headers 不能跨代理边界继续传播。
fn copy_end_to_end_headers(source: &HeaderMap, target: &mut HeaderMap, streaming: bool) {
    let connection_tokens: Vec<String> = source
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
        .collect();

    // 同名头第一次出现用 insert 覆盖 target 里的预设值（如流式路径预置的
    // Content-Type），之后的重复出现才 append —— 否则预设值 + 上游值会在
    // 响应里产生两个 Content-Type。多值头（如 Set-Cookie）仍然保留全部。
    let mut seen: std::collections::HashSet<HeaderName> = std::collections::HashSet::new();
    for (name, value) in source {
        let lower = name.as_str();
        let hop_by_hop = matches!(
            lower,
            "connection"
                | "keep-alive"
                | "proxy-authenticate"
                | "proxy-authorization"
                | "te"
                | "trailer"
                | "transfer-encoding"
                | "upgrade"
        ) || connection_tokens.iter().any(|token| token == lower);
        if hop_by_hop || (streaming && name == header::CONTENT_LENGTH) {
            continue;
        }
        if seen.insert(name.clone()) {
            target.insert(name.clone(), value.clone());
        } else {
            target.append(name.clone(), value.clone());
        }
    }
}

fn inspect_native_frames(
    frames: Vec<refract_protocol::SseFrame>,
    decoder: &mut dyn refract_protocol::StreamDecoder,
    aggregator: &mut StreamAggregator,
    saw_done: &mut bool,
) -> Result<(), GatewayError> {
    for frame in frames {
        let events = decoder.decode(&frame).map_err(|error| {
            GatewayError::new(
                ErrorKind::UpstreamError,
                format!("failed to decode upstream stream: {}", error.message),
            )
        })?;
        for event in events {
            match &event {
                refract_protocol::StreamEvent::Error { message, .. } => {
                    return Err(GatewayError::new(ErrorKind::UpstreamError, message.clone()));
                }
                refract_protocol::StreamEvent::Done => *saw_done = true,
                _ => {}
            }
            aggregator.absorb(&event);
        }
    }
    Ok(())
}

/// 向客户端发送一批已经编码的 SSE 帧；返回 false 表示客户端已断开。
async fn send_frames(
    tx: &tokio::sync::mpsc::Sender<Bytes>,
    frames: Vec<refract_protocol::SseFrame>,
) -> bool {
    for frame in frames {
        if tx.send(sse_bytes(&frame)).await.is_err() {
            return false;
        }
    }
    true
}

/// 把协议帧转成 SSE 文本。
fn sse_bytes(frame: &refract_protocol::SseFrame) -> Bytes {
    let mut out = String::new();
    if let Some(name) = &frame.event {
        out.push_str("event: ");
        out.push_str(name);
        out.push('\n');
    }
    out.push_str("data: ");
    out.push_str(&frame.data);
    out.push_str("\n\n");
    Bytes::from(out)
}

/// 写一条失败日志。
///
/// 失败也要记 —— 「为什么这个模型总是 503」只能从失败日志里看出来。
fn log_failure(context: &DispatchContext, err: &GatewayError) {
    let key_id = context.principal.key_id();
    let mut entry = NewRequestLog::new(
        context.principal.owner_id,
        context.request_id.clone(),
        key_id,
        context.inbound,
        context.model.clone(),
        context.stream,
    )
    .with_timing(None, context.started.elapsed().as_millis() as u64)
    .with_snapshots(context.request_snapshot.clone(), err.upstream_body.clone())
    .with_routing_context(
        err.credential_hint.clone(),
        context.affinity.as_ref().map(|d| d.rule_name.clone()),
    );
    entry.channel_id = err.channel_id;
    entry.channel_name = err.channel_name.clone();
    entry.upstream_protocol = err.protocol.unwrap_or(context.inbound);
    entry.upstream_model = err.upstream_model.clone().unwrap_or_default();
    entry.status = err.status();
    entry.retries = u32::from(err.attempts.saturating_sub(1));
    entry.error_kind = Some(err.kind.openai_type().to_owned());
    entry.error_message = Some(err.message.clone());
    entry.user_id = context.principal.user_id;
    record(&context.state, entry, key_id, 0, context.principal.user_id);
}

/// 网关级全局准入：RPM / TPM 窗口 + 并发 permit。
///
/// 密钥级限速在免鉴权模式下是零防护 —— 这层是跑飞的本地 agent 迴圈
/// 与上游账单之间最后的保险丝。返回的 permit 活多久，并发额度就占多久：
/// unary 在响应构造完释放，流式一直持有到流结束。
///
/// RPM 与 TPM 在同一次 `admit` 里检查：两者共用一个窗口条目，分开调用会把
/// 本次请求重复计入 requests 计数。
pub(crate) fn enforce_global_limits(
    state: &AppState,
    inbound: Protocol,
    request_id: &str,
) -> Result<Option<tokio::sync::OwnedSemaphorePermit>, AppError> {
    let limits = state.global_limits();
    if let Err(exceeded) = state.rate_limiter().admit(
        crate::rate::GLOBAL_WINDOW_KEY,
        i64::from(limits.rpm),
        i64::from(limits.tpm),
    ) {
        // 触发维度决定文案里报哪个上限：RPM 与 TPM 的处置动作不同（降频 vs 缩上下文）。
        let cap = match exceeded.dimension {
            crate::rate::RateDimension::Requests => limits.rpm,
            crate::rate::RateDimension::Tokens => limits.tpm,
        };
        let error = GatewayError::new(
            ErrorKind::RateLimited,
            format!(
                "gateway-wide {} limit ({}) exceeded; retry in {}s",
                exceeded.dimension.describe(),
                cap,
                exceeded.retry_after_secs,
            ),
        )
        .with_retry_after(std::time::Duration::from_secs(exceeded.retry_after_secs));
        return Err(AppError::Protocol(ProtocolRejection::with_id(
            error,
            inbound,
            request_id.to_owned(),
        )));
    }
    match state.concurrency_semaphore() {
        None => Ok(None),
        Some(semaphore) => match semaphore.try_acquire_owned() {
            Ok(permit) => Ok(Some(permit)),
            // 不排队直接拒绝：排队会把过载藏进延迟里，429 让客户端自己退避。
            Err(_) => {
                let error = GatewayError::new(
                    ErrorKind::RateLimited,
                    format!(
                        "gateway concurrency limit ({}) reached; retry shortly",
                        state.global_limits().max_concurrency
                    ),
                )
                .with_retry_after(std::time::Duration::from_secs(1));
                Err(AppError::Protocol(ProtocolRejection::with_id(
                    error,
                    inbound,
                    request_id.to_owned(),
                )))
            }
        },
    }
}

/// 速率限制准入。密钥未配置限制时零开销；未鉴权（本地模式）不限。
pub(crate) fn enforce_rate_limit(
    state: &AppState,
    principal: &Principal,
    inbound: Protocol,
    request_id: &str,
) -> Result<(), AppError> {
    let Some(key) = principal.api_key.as_deref() else {
        return Ok(());
    };
    if let Err(exceeded) = state
        .rate_limiter()
        .admit(key.id, key.rpm_limit, key.tpm_limit)
    {
        let error = GatewayError::new(
            ErrorKind::RateLimited,
            format!(
                "API key `{}` exceeded its {} limit; retry in {}s",
                key.name,
                exceeded.dimension.describe(),
                exceeded.retry_after_secs,
            ),
        )
        .with_retry_after(std::time::Duration::from_secs(exceeded.retry_after_secs));
        return Err(AppError::Protocol(ProtocolRejection::with_id(
            error,
            inbound,
            request_id.to_owned(),
        )));
    }
    Ok(())
}

/// 单 IP 限速准入。未配置限制（rpm == 0）时零开销。
pub(crate) fn enforce_ip_limit(
    state: &AppState,
    remote_ip: Option<std::net::IpAddr>,
    inbound: Protocol,
    request_id: &str,
) -> Result<(), AppError> {
    let limits = state.ip_limits();
    if limits.rpm == 0 {
        return Ok(());
    }
    let ip = remote_ip.unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    if let Err(exceeded) = state.ip_limiter().admit(ip, limits.rpm) {
        let error = GatewayError::new(
            ErrorKind::RateLimited,
            format!(
                "per-IP request limit ({} RPM) exceeded; retry in {}s",
                limits.rpm, exceeded.retry_after_secs,
            ),
        )
        .with_retry_after(std::time::Duration::from_secs(exceeded.retry_after_secs));
        return Err(AppError::Protocol(ProtocolRejection::with_id(
            error,
            inbound,
            request_id.to_owned(),
        )));
    }
    Ok(())
}

/// 落一条日志并累计密钥用量。
///
/// 日志写失败**不能**影响响应 —— 请求已经成功了，因为记账问题给客户端报错
/// 是本末倒置。落库在后台任务里完成，不占用响应路径：SQLite 写入通常在
/// 亚毫秒级，但 checkpoint 或磁盘抖动时会到几十毫秒，没理由让客户端等它。
fn record(
    state: &AppState,
    mut entry: NewRequestLog,
    key_id: Option<i64>,
    tokens: u64,
    user_id: Option<i64>,
) {
    // 成本按落库当时的价表固化进日志：单价会变，历史账单不应跟着变。
    entry.cost = state.cost_for(
        &entry.model,
        entry.input_tokens,
        entry.output_tokens,
        entry.cached_tokens,
        entry.cache_write_tokens,
    );
    // 指标与日志同源采集：/metrics 的数字永远能和日志对上。
    // observe 是纯内存操作，留在请求路径里保证响应返回时指标已可见。
    state.metrics().observe(&entry);
    // TPM 记账在内存里同步完成 —— 下一个请求的准入检查要立刻看到本次用量。
    if tokens > 0 {
        // 全局窗口无条件收账：免鉴权模式下 key_id 为 None，只挂密钥的话
        // 全局 TPM 永远读到 0，保险丝等于没接。
        state
            .rate_limiter()
            .add_tokens(crate::rate::GLOBAL_WINDOW_KEY, tokens);
        if let Some(id) = key_id {
            state.rate_limiter().add_tokens(id, tokens);
        }
    }
    let state = state.clone();
    tokio::spawn(async move {
        if let Err(e) = state.log_repo().append(&entry).await {
            tracing::warn!(error = %e, "failed to persist request log");
        }
        if let Some(id) = key_id
            && (tokens > 0 || entry.cost > 0.0)
            && let Err(e) = state
                .key_repo()
                .record_usage(id, tokens as i64, entry.cost)
                .await
        {
            tracing::warn!(error = %e, "failed to record api key usage");
        }
        // 钱包扣款：幂等键 = request_id，重试不会双扣（store 层唯一索引兜底）。
        // 扣款失败不回滚已完成的请求 —— 余额由下次请求的预检兜底，失败只计数告警。
        if let Some(uid) = user_id
            && entry.cost > 0.0
        {
            match state
                .wallet_repo()
                .apply(
                    uid,
                    -entry.cost,
                    refract_store::LedgerKind::Charge,
                    Some(&entry.request_id),
                    &entry.model,
                )
                .await
            {
                // 扣款成功后同步预检缓存，让余额耗尽的下一请求立刻被挡，
                // 而不是等 60 秒 TTL 自然过期。
                Ok(_) => {
                    if let Ok(balance) = state.wallet_repo().balance(uid).await {
                        state.prime_balance_cache(uid, balance);
                    }
                }
                Err(e) => {
                    state.metrics().note_wallet_charge_failure();
                    tracing::error!(error = %e, user_id = uid, "failed to charge wallet");
                }
            }
        }
    });
}

#[cfg(test)]
#[path = "gateway_tests.rs"]
mod tests;
