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
//!    `warp::sse` 直接桥接上游流，转码在流经过时逐帧发生。
//! 2. **日志在响应结束后才落库**。流式请求的 token 用量只有在最后一帧才知道，
//!    提前写日志会得到一堆 `output_tokens: 0`。

use std::convert::Infallible;

use bytes::{Buf, Bytes, BytesMut};
use futures_util::{Stream, StreamExt as _};
use refract_core::{ErrorKind, GatewayError, Protocol};
use refract_protocol::StreamAggregator;
use refract_router::{Diagnosis, InboundPayload, RoutedResponse, RoutedStream};
use refract_store::NewRequestLog;
use serde_json::Value;
use warp::{Filter, Rejection, Reply};

use crate::auth::{Principal, authenticate};
use crate::error::ProtocolRejection;
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
fn forwardable_headers(headers: &warp::http::HeaderMap) -> Vec<(String, String)> {
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
fn tag_request_id(mut response: warp::reply::Response, request_id: &str) -> warp::reply::Response {
    if let Ok(value) = warp::http::HeaderValue::from_str(request_id) {
        response.headers_mut().insert("x-refract-request-id", value);
    }
    response
}

/// 装配网关路由。
///
/// 四个协议各有自己的路径形状 —— 这是外部契约，不能统一。
pub fn routes(state: AppState) -> impl Filter<Extract = (impl Reply,), Error = Rejection> + Clone {
    chat(state.clone())
        .or(messages(state.clone()))
        .unify()
        .or(responses(state.clone()))
        .unify()
        .or(gemini(state.clone()))
        .unify()
        .or(embeddings(state.clone()))
        .unify()
        .or(list_models(state))
        .unify()
}

/// `POST /v1/chat/completions` —— OpenAI Chat Completions。
fn chat(
    state: AppState,
) -> impl Filter<Extract = (warp::reply::Response,), Error = Rejection> + Clone {
    warp::path!("v1" / "chat" / "completions")
        .and(protocol_method(warp::http::Method::POST, Protocol::Chat))
        .and(authenticate(state.authenticator(), Protocol::Chat))
        .and(warp::header::headers_cloned())
        .and(protocol_json_body(Protocol::Chat))
        .and(with(state))
        .and_then(|caller, headers, body, state| {
            dispatch(state, caller, Protocol::Chat, headers, body, None)
        })
}

/// `POST /v1/messages` —— Anthropic Messages。
fn messages(
    state: AppState,
) -> impl Filter<Extract = (warp::reply::Response,), Error = Rejection> + Clone {
    warp::path!("v1" / "messages")
        .and(protocol_method(
            warp::http::Method::POST,
            Protocol::Messages,
        ))
        .and(authenticate(state.authenticator(), Protocol::Messages))
        .and(warp::header::headers_cloned())
        .and(protocol_json_body(Protocol::Messages))
        .and(with(state))
        .and_then(|caller, headers, body, state| {
            dispatch(state, caller, Protocol::Messages, headers, body, None)
        })
}

/// `POST /v1/responses` —— OpenAI Responses。
fn responses(
    state: AppState,
) -> impl Filter<Extract = (warp::reply::Response,), Error = Rejection> + Clone {
    warp::path!("v1" / "responses")
        .and(protocol_method(
            warp::http::Method::POST,
            Protocol::Responses,
        ))
        .and(authenticate(state.authenticator(), Protocol::Responses))
        .and(warp::header::headers_cloned())
        .and(protocol_json_body(Protocol::Responses))
        .and(with(state))
        .and_then(|caller, headers, body, state| {
            dispatch(state, caller, Protocol::Responses, headers, body, None)
        })
}

/// `POST /v1beta/models/{model}:generateContent`（及 `:streamGenerateContent`）。
///
/// Gemini 把模型名和动作编码在**路径**里，而不是请求体 —— 所以这里要把它们
/// 抽出来注入 IR，否则路由器不知道要路由到哪个模型。
fn gemini(
    state: AppState,
) -> impl Filter<Extract = (warp::reply::Response,), Error = Rejection> + Clone {
    warp::path!("v1beta" / "models" / String)
        .and(protocol_method(warp::http::Method::POST, Protocol::Gemini))
        .and(authenticate(state.authenticator(), Protocol::Gemini))
        .and(warp::header::headers_cloned())
        .and(protocol_json_body(Protocol::Gemini))
        .and(with(state))
        .and_then(
            |spec: String,
             principal: Principal,
             headers: warp::http::HeaderMap,
             body: JsonBody,
             state: AppState| async move {
                // `gemini-2.5-pro:streamGenerateContent` → (模型, 是否流式)
                let (model, verb) = spec
                    .split_once(':')
                    .unwrap_or((spec.as_str(), "generateContent"));
                let stream = verb.starts_with("stream");
                dispatch(
                    state,
                    principal,
                    Protocol::Gemini,
                    headers,
                    body,
                    Some(GeminiPath {
                        model: model.to_owned(),
                        stream,
                    }),
                )
                .await
            },
        )
}

/// Gemini 从 URL 里带进来的信息。
struct GeminiPath {
    model: String,
    stream: bool,
}

/// `POST /v1/embeddings` —— OpenAI Embeddings 透传。
///
/// 嵌入没有跨协议转换语义（Anthropic 无此 API，Gemini 形状完全不同），
/// 因此只透传到 **Chat 协议端点**：把嵌入模型加进渠道的模型列表即可路由。
/// 别名、参数覆盖、熔断、重试与对话请求一致；请求与响应字节原样往返。
fn embeddings(
    state: AppState,
) -> impl Filter<Extract = (warp::reply::Response,), Error = Rejection> + Clone {
    warp::path!("v1" / "embeddings")
        .and(protocol_method(warp::http::Method::POST, Protocol::Chat))
        .and(authenticate(state.authenticator(), Protocol::Chat))
        .and(warp::header::headers_cloned())
        .and(protocol_json_body(Protocol::Chat))
        .and(with(state))
        .and_then(embeddings_response)
}

/// 嵌入请求的分发与执行。
///
/// 与 [`dispatch`] 的差别：候选被过滤为 Chat 原生端点（透传无转码路径），
/// 无流式分支，用量从响应体尽力提取。
async fn embeddings_response(
    principal: Principal,
    headers: warp::http::HeaderMap,
    body: JsonBody,
    state: AppState,
) -> Result<warp::reply::Response, Rejection> {
    let inbound = Protocol::Chat;
    let started = std::time::Instant::now();
    let request_id = uuid::Uuid::new_v4().to_string();
    let model = body
        .model
        .as_deref()
        .filter(|model| !model.is_empty())
        .ok_or_else(|| {
            ProtocolRejection::reject_with_id(
                GatewayError::invalid_request("missing required field `model`"),
                inbound,
                request_id.clone(),
            )
        })?
        .to_owned();

    if !principal.allows_model(&model) {
        return Err(ProtocolRejection::reject_with_id(
            GatewayError::new(
                refract_core::ErrorKind::PermissionDenied,
                format!("this API key is not allowed to use model `{model}`"),
            ),
            inbound,
            request_id,
        ));
    }

    let channels = state.channels();
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
    // 透传没有转码路径：只保留 Chat 原生端点。
    route.attempts.retain(|c| c.protocol() == inbound);

    let context = DispatchContext {
        state: state.clone(),
        principal,
        inbound,
        request_id,
        started,
        model: model.clone(),
        stream: false,
        forward_headers: forwardable_headers(&headers),
    };

    if route.is_empty() {
        let err = GatewayError::not_found(format!(
            "no enabled chat-protocol endpoint provides model `{model}`; \
             embeddings pass through chat endpoints only — add the embedding model \
             to a chat endpoint's model list"
        ));
        log_failure(&context, &err);
        return Err(context.reject(err));
    }

    let outcome = match context
        .state
        .executor()
        .execute_passthrough(
            &route,
            refract_core::Action::Embeddings,
            &body.raw,
            &context.forward_headers,
        )
        .await
    {
        Ok(outcome) => outcome,
        Err(error) => {
            log_failure(&context, &error);
            return Err(context.reject(error));
        }
    };

    let usage = embeddings_usage(&outcome.payload.body);
    let response = tag_request_id(native_unary_response(&outcome.payload), &context.request_id);
    let key_id = context.principal.key_id();
    let entry = NewRequestLog {
        owner_id: context.principal.owner_id,
        request_id: context.request_id,
        api_key_id: key_id,
        channel_id: Some(outcome.channel_id),
        channel_name: Some(outcome.channel_name),
        inbound_protocol: inbound,
        upstream_protocol: outcome.upstream_protocol,
        model,
        upstream_model: outcome.upstream_model,
        stream: false,
        status: response.status().as_u16(),
        ttfb_ms: Some(outcome.latency_ms),
        duration_ms: started.elapsed().as_millis() as u64,
        input_tokens: usage.input_tokens,
        output_tokens: 0,
        cached_tokens: 0,
        reasoning_tokens: 0,
        retries: u32::from(outcome.attempts.saturating_sub(1)),
        error_kind: None,
        error_message: None,
    };
    record(&context.state, entry, key_id, usage.input_tokens);

    Ok(response)
}

/// 尽力从嵌入响应提取用量；解析失败不影响透传。
fn embeddings_usage(body: &Bytes) -> EmbeddingsUsage {
    #[derive(serde::Deserialize)]
    struct Envelope {
        #[serde(default)]
        usage: Option<Fields>,
    }
    #[derive(serde::Deserialize, Default)]
    struct Fields {
        #[serde(default)]
        prompt_tokens: u64,
    }
    let usage = serde_json::from_slice::<Envelope>(body)
        .ok()
        .and_then(|e| e.usage)
        .unwrap_or_default();
    EmbeddingsUsage {
        input_tokens: usage.prompt_tokens,
    }
}

/// 嵌入请求的用量：只有输入侧。
struct EmbeddingsUsage {
    input_tokens: u64,
}

/// `GET /v1/models` —— 模型清单。
///
/// 由渠道快照派生，形状照抄 OpenAI（Anthropic/Gemini 的 SDK 也认这个形状的变体，
/// 但真正会调这个端点的几乎都是 OpenAI 系工具）。
fn list_models(
    state: AppState,
) -> impl Filter<Extract = (warp::reply::Response,), Error = Rejection> + Clone {
    warp::path!("v1" / "models")
        .and(protocol_method(warp::http::Method::GET, Protocol::Chat))
        .and(authenticate(state.authenticator(), Protocol::Chat))
        .and(with(state))
        .and_then(|principal: Principal, state: AppState| async move {
            let channels = state.channels();
            let allowed_channels: Vec<_> = channels
                .iter()
                .filter(|channel| principal.allows_channel(channel))
                .collect();
            let mut names: Vec<String> = state
                .planner()
                .visible_models(allowed_channels)
                .into_iter()
                .filter(|m| principal.allows_model(m))
                .collect();
            names.sort_unstable();
            names.dedup();

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

            Ok::<_, Rejection>(
                warp::reply::json(&serde_json::json!({ "object": "list", "data": data }))
                    .into_response(),
            )
        })
}

/// 协议感知的方法检查。
///
/// 使用 `warp::post()` 会在进入 handler 前产生一个没有协议信息的 rejection，
/// 全局恢复器只能回管理 API 形状。这里把 405 包装成协议错误，官方 SDK 才能
/// 正常读到 `error.message`。
fn protocol_method(
    expected: warp::http::Method,
    protocol: Protocol,
) -> impl Filter<Extract = (), Error = Rejection> + Clone {
    warp::method()
        .and_then(move |actual: warp::http::Method| {
            let expected = expected.clone();
            async move {
                if actual == expected {
                    Ok(())
                } else {
                    Err(ProtocolRejection::with_status(
                        GatewayError::invalid_request(format!(
                            "method `{actual}` is not allowed; use `{expected}`"
                        )),
                        protocol,
                        warp::http::StatusCode::METHOD_NOT_ALLOWED,
                    ))
                }
            }
        })
        .untuple_one()
}

/// 有硬上限、且把解析失败包装成对应协议错误的 JSON body。
fn protocol_json_body(
    protocol: Protocol,
) -> impl Filter<Extract = (JsonBody,), Error = Rejection> + Clone {
    warp::body::stream().and_then(move |stream| read_json_body(stream, protocol))
}

/// 原始 JSON 与路由所需的轻量字段；完整 IR 由 executor 按需构造。
struct JsonBody {
    raw: Bytes,
    model: Option<String>,
    stream: bool,
}

#[derive(serde::Deserialize)]
struct RoutingFields {
    model: Option<String>,
    #[serde(default)]
    stream: bool,
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
}

impl DispatchContext {
    /// 构造带请求标识的协议错误，失败响应也能对上日志。
    fn reject(&self, error: GatewayError) -> Rejection {
        ProtocolRejection::reject_with_id(error, self.inbound, self.request_id.clone())
    }
}

async fn read_json_body<S, B>(stream: S, protocol: Protocol) -> Result<JsonBody, Rejection>
where
    S: Stream<Item = Result<B, warp::Error>>,
    B: Buf,
{
    futures_util::pin_mut!(stream);
    let mut body = BytesMut::new();

    while let Some(chunk) = stream.next().await {
        let mut chunk = chunk.map_err(|_| {
            ProtocolRejection::reject(
                GatewayError::invalid_request("failed to read request body"),
                protocol,
            )
        })?;

        if body.len().saturating_add(chunk.remaining()) > GATEWAY_BODY_LIMIT {
            return Err(ProtocolRejection::reject(
                GatewayError::new(
                    ErrorKind::PayloadTooLarge,
                    format!("request body exceeds {GATEWAY_BODY_LIMIT} bytes"),
                ),
                protocol,
            ));
        }

        while chunk.has_remaining() {
            let part = chunk.chunk();
            if part.is_empty() {
                break;
            }
            body.extend_from_slice(part);
            chunk.advance(part.len());
        }
    }

    let raw = body.freeze();
    let routing: RoutingFields = serde_json::from_slice(&raw).map_err(|error| {
        ProtocolRejection::reject(
            GatewayError::invalid_request(format!("malformed request body: {error}")),
            protocol,
        )
    })?;
    Ok(JsonBody {
        raw,
        model: routing.model,
        stream: routing.stream,
    })
}

/// 请求分发的核心。
///
/// 所有四个协议汇聚到这里 —— 差异已经在 codec 里被吸收掉了。
async fn dispatch(
    state: AppState,
    principal: Principal,
    inbound: Protocol,
    headers: warp::http::HeaderMap,
    body: JsonBody,
    gemini_path: Option<GeminiPath>,
) -> Result<warp::reply::Response, Rejection> {
    let started = std::time::Instant::now();
    let request_id = uuid::Uuid::new_v4().to_string();
    // Gemini 从路径取路由字段，另外三种协议只轻量读取顶层 model/stream。
    let (model, stream) = match gemini_path {
        Some(path) => (path.model, path.stream),
        None => {
            let model = body
                .model
                .as_deref()
                .filter(|model| !model.is_empty())
                .ok_or_else(|| {
                    ProtocolRejection::reject_with_id(
                        GatewayError::invalid_request("missing required field `model`"),
                        inbound,
                        request_id.clone(),
                    )
                })?
                .to_owned();
            (model, body.stream)
        }
    };

    // 密钥的模型白名单 —— 在路由之前拦截，避免「路由成功但无权访问」的混乱语义。
    if !principal.allows_model(&model) {
        return Err(ProtocolRejection::reject_with_id(
            GatewayError::new(
                refract_core::ErrorKind::PermissionDenied,
                format!("this API key is not allowed to use model `{model}`"),
            ),
            inbound,
            request_id,
        ));
    }

    let channels = state.channels();
    let allowed_channels: Vec<_> = channels
        .iter()
        .filter(|channel| principal.allows_channel(channel))
        .collect();
    let route = {
        let mut rng = rand::rng();
        state
            .planner()
            .plan(allowed_channels.iter().copied(), &model, inbound, &mut rng)
    };
    let context = DispatchContext {
        state: state.clone(),
        principal,
        inbound,
        request_id,
        started,
        model: model.clone(),
        stream,
        forward_headers: forwardable_headers(&headers),
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

    if stream {
        stream_response(context, body, route).await
    } else {
        unary_response(context, body, route).await
    }
}

/// 非流式响应。
async fn unary_response(
    context: DispatchContext,
    raw: JsonBody,
    route: refract_router::Route<'_>,
) -> Result<warp::reply::Response, Rejection> {
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
        ..
    } = context;
    let owner_id = principal.owner_id;
    let key_id = principal.key_id();

    let usage = outcome.payload.usage();
    let response = match &outcome.payload {
        RoutedResponse::Native { response, .. } => native_unary_response(response),
        RoutedResponse::Transcoded(payload) => {
            let body = state
                .codecs()
                .for_protocol(inbound)
                .encode_response(payload)
                .map_err(|e| ProtocolRejection::reject_with_id(e, inbound, request_id.clone()))?;
            warp::reply::json(&body).into_response()
        }
    };
    let response = tag_request_id(response, &request_id);
    let response_status = response.status().as_u16();
    let entry = NewRequestLog {
        owner_id,
        request_id,
        api_key_id: key_id,
        channel_id: Some(outcome.channel_id),
        channel_name: Some(outcome.channel_name.clone()),
        inbound_protocol: inbound,
        upstream_protocol: outcome.upstream_protocol,
        model,
        upstream_model: outcome.upstream_model.clone(),
        stream: false,
        status: response_status,
        ttfb_ms: Some(outcome.latency_ms),
        duration_ms: started.elapsed().as_millis() as u64,
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cached_tokens: usage.cached_input_tokens,
        reasoning_tokens: usage.reasoning_tokens,
        retries: u32::from(outcome.attempts.saturating_sub(1)),
        error_kind: None,
        error_message: None,
    };
    record(&state, entry, key_id, usage.total());

    Ok(response)
}

/// 构造同协议非流式响应，并只保留可跨连接转发的 headers。
fn native_unary_response(
    upstream: &refract_upstream::UpstreamRawResponse,
) -> warp::reply::Response {
    let mut response = warp::reply::Response::new(upstream.body.clone().into());
    if let Ok(status) = warp::http::StatusCode::from_u16(upstream.status) {
        *response.status_mut() = status;
    }
    copy_end_to_end_headers(&upstream.headers, response.headers_mut(), false);
    if !response
        .headers()
        .contains_key(warp::http::header::CONTENT_TYPE)
    {
        response.headers_mut().insert(
            warp::http::header::CONTENT_TYPE,
            warp::http::HeaderValue::from_static("application/json"),
        );
    }
    response
}

/// 流式响应。
///
/// 关键在于**不等流结束**：`warp::sse::reply` 接受一个 `Stream`，我们把上游流
/// 包装后直接交出去。转码逐帧发生，日志在流的末尾用 `finally` 语义补写。
async fn stream_response(
    dispatch: DispatchContext,
    raw: JsonBody,
    route: refract_router::Route<'_>,
) -> Result<warp::reply::Response, Rejection> {
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
            log_failure(&dispatch, &e);
            return Err(dispatch.reject(e));
        }
    };
    let DispatchContext {
        state,
        principal,
        inbound,
        request_id,
        started,
        model,
        ..
    } = dispatch;

    let context = StreamContext {
        state,
        owner_id: principal.owner_id,
        key_id: principal.key_id(),
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
}

async fn record_stream_endpoint_health(
    context: &StreamContext,
    error: Option<&GatewayError>,
    client_closed: bool,
) {
    if client_closed {
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
            context
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
                .map(|_| ())
        }
        Some(_) => return,
        None => {
            context
                .state
                .executor()
                .health()
                .record_success(
                    context.channel_id,
                    context.upstream_protocol,
                    context.ttfb_ms,
                )
                .await
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
) -> warp::reply::Response {
    let codecs = context.state.codecs();
    let decoder = codecs
        .for_protocol(context.upstream_protocol)
        .stream_decoder();
    let encoder = codecs.for_protocol(context.inbound).stream_encoder();

    // warp 0.4 的 SSE reply 要求返回流 `Sync`，而上游流与 codec 状态机只保证
    // `Send`。把生产者放到独立 Tokio task，响应侧只持有线程安全的 mpsc Receiver：
    // 非 Sync 的网络/codec/数据库状态不会穿过 warp 的 Reply 边界。
    let (tx, rx) = tokio::sync::mpsc::channel::<warp::sse::Event>(32);
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

        let usage = aggregator.usage;
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
        let entry = NewRequestLog {
            owner_id: context.owner_id,
            request_id: context.request_id,
            api_key_id: context.key_id,
            channel_id: Some(context.channel_id),
            channel_name: Some(context.channel_name),
            inbound_protocol: context.inbound,
            upstream_protocol: context.upstream_protocol,
            model: context.model,
            upstream_model: context.upstream_model,
            stream: true,
            status,
            ttfb_ms: Some(context.ttfb_ms),
            duration_ms: context.started.elapsed().as_millis() as u64,
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cached_tokens: usage.cached_input_tokens,
            reasoning_tokens: usage.reasoning_tokens,
            retries: u32::from(context.attempts.saturating_sub(1)),
            error_kind,
            error_message,
        };
        record(&context.state, entry, context.key_id, usage.total());
    });

    let stream =
        tokio_stream::wrappers::ReceiverStream::new(rx).map(Ok::<warp::sse::Event, Infallible>);
    warp::sse::reply(warp::sse::keep_alive().stream(stream)).into_response()
}

/// 原生协议流：不解析、不重编码，逐字节转发上游 SSE。
fn native_stream_response(
    context: StreamContext,
    upstream: refract_upstream::UpstreamRawStream,
) -> warp::reply::Response {
    let refract_upstream::UpstreamRawStream {
        status,
        headers,
        stream: upstream,
    } = upstream;
    let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(32);
    tokio::spawn(relay_native_stream(context, upstream, tx));

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx).map(Ok::<Bytes, std::io::Error>);
    let mut response = warp::reply::stream(stream).into_response();
    if let Ok(status) = warp::http::StatusCode::from_u16(status) {
        *response.status_mut() = status;
    }
    copy_end_to_end_headers(&headers, response.headers_mut(), true);
    // Content-Type 最后强制为 SSE：这个路径的响应体已通过预检确认是合法
    // SSE 流，上游把它错标成 text/plain（部分反代会这样）不该传染给客户端
    // —— 客户端 SDK 靠这个头决定按流解析还是整体读取。
    response.headers_mut().insert(
        warp::http::header::CONTENT_TYPE,
        warp::http::HeaderValue::from_static("text/event-stream"),
    );
    response.headers_mut().insert(
        warp::http::header::CACHE_CONTROL,
        warp::http::HeaderValue::from_static("no-cache"),
    );
    response.headers_mut().insert(
        warp::http::HeaderName::from_static("x-accel-buffering"),
        warp::http::HeaderValue::from_static("no"),
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

    let usage = aggregator.usage;
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
    let entry = NewRequestLog {
        owner_id: context.owner_id,
        request_id: context.request_id,
        api_key_id: context.key_id,
        channel_id: Some(context.channel_id),
        channel_name: Some(context.channel_name),
        inbound_protocol: context.inbound,
        upstream_protocol: context.upstream_protocol,
        model: context.model,
        upstream_model: context.upstream_model,
        stream: true,
        status,
        ttfb_ms: Some(context.ttfb_ms),
        duration_ms: context.started.elapsed().as_millis() as u64,
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cached_tokens: usage.cached_input_tokens,
        reasoning_tokens: usage.reasoning_tokens,
        retries: u32::from(context.attempts.saturating_sub(1)),
        error_kind,
        error_message,
    };
    record(&context.state, entry, context.key_id, usage.total());
}

/// 复制端到端响应头；逐连接 headers 不能跨代理边界继续传播。
fn copy_end_to_end_headers(
    source: &warp::http::HeaderMap,
    target: &mut warp::http::HeaderMap,
    streaming: bool,
) {
    let connection_tokens: Vec<String> = source
        .get_all(warp::http::header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
        .collect();

    // 同名头第一次出现用 insert 覆盖 target 里的预设值（如流式路径预置的
    // Content-Type），之后的重复出现才 append —— 否则预设值 + 上游值会在
    // 响应里产生两个 Content-Type。多值头（如 Set-Cookie）仍然保留全部。
    let mut seen: std::collections::HashSet<warp::http::HeaderName> =
        std::collections::HashSet::new();
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
        if hop_by_hop || (streaming && name == warp::http::header::CONTENT_LENGTH) {
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
    tx: &tokio::sync::mpsc::Sender<warp::sse::Event>,
    frames: Vec<refract_protocol::SseFrame>,
) -> bool {
    for frame in frames {
        if tx.send(to_sse(&frame)).await.is_err() {
            return false;
        }
    }
    true
}

/// 把协议帧转成 warp 的 SSE 事件。
fn to_sse(frame: &refract_protocol::SseFrame) -> warp::sse::Event {
    let event = warp::sse::Event::default().data(&frame.data);
    match &frame.event {
        Some(name) => event.event(name),
        None => event,
    }
}

/// 写一条失败日志。
///
/// 失败也要记 —— 「为什么这个模型总是 503」只能从失败日志里看出来。
fn log_failure(context: &DispatchContext, err: &GatewayError) {
    let key_id = context.principal.key_id();
    let entry = NewRequestLog {
        owner_id: context.principal.owner_id,
        request_id: context.request_id.clone(),
        api_key_id: key_id,
        channel_id: err.channel_id,
        channel_name: err.channel_name.clone(),
        inbound_protocol: context.inbound,
        // 没走到上游时，上游协议就记成入口协议 —— 记 None 会让「转换率」统计失真。
        upstream_protocol: err.protocol.unwrap_or(context.inbound),
        model: context.model.clone(),
        upstream_model: err.upstream_model.clone().unwrap_or_default(),
        stream: context.stream,
        status: err.status(),
        ttfb_ms: None,
        duration_ms: context.started.elapsed().as_millis() as u64,
        input_tokens: 0,
        output_tokens: 0,
        cached_tokens: 0,
        reasoning_tokens: 0,
        retries: u32::from(err.attempts.saturating_sub(1)),
        error_kind: Some(format!("{:?}", err.kind)),
        error_message: Some(err.message.clone()),
    };
    record(&context.state, entry, key_id, 0);
}

/// 落一条日志并累计密钥用量。
///
/// 日志写失败**不能**影响响应 —— 请求已经成功了，因为记账问题给客户端报错
/// 是本末倒置。落库在后台任务里完成，不占用响应路径：SQLite 写入通常在
/// 亚毫秒级，但 checkpoint 或磁盘抖动时会到几十毫秒，没理由让客户端等它。
fn record(state: &AppState, entry: NewRequestLog, key_id: Option<i64>, tokens: u64) {
    // 指标与日志同源采集：/metrics 的数字永远能和日志对上。
    // observe 是纯内存操作，留在请求路径里保证响应返回时指标已可见。
    state.metrics().observe(&entry);
    let state = state.clone();
    tokio::spawn(async move {
        if let Err(e) = state.log_repo().append(&entry).await {
            tracing::warn!(error = %e, "failed to persist request log");
        }
        if let Some(id) = key_id
            && tokens > 0
            && let Err(e) = state.key_repo().record_usage(id, tokens as i64).await
        {
            tracing::warn!(error = %e, "failed to record api key usage");
        }
    });
}

/// 把状态注入过滤器链。
fn with(state: AppState) -> impl Filter<Extract = (AppState,), Error = Infallible> + Clone {
    warp::any().map(move || state.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use refract_core::{
        Channel, ChannelEndpoint, ChannelKind, Credential, ModelEntry, ProtocolSet,
        TranscodePolicy, UpstreamAddress,
    };
    use refract_store::Database;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn state_with(channels: Vec<Channel>) -> AppState {
        let db = Database::open_in_memory().await.unwrap();
        let repo = refract_store::ChannelRepo::new(db.clone());
        for channel in &channels {
            repo.create(channel).await.unwrap();
        }
        let client = refract_upstream::UpstreamClient::new(Default::default()).unwrap();
        AppState::bootstrap(db, client, false).await.unwrap()
    }

    fn channel_at(base: &str, protocol: Protocol, models: &[&str]) -> Channel {
        Channel {
            id: 0,
            owner_id: refract_core::DEFAULT_OWNER_ID,
            name: format!("{protocol}-upstream"),
            kind: ChannelKind::Single(protocol),
            enabled: true,
            priority: 0,
            weight: 1,
            credential: Credential::new("test-key"),
            address: UpstreamAddress {
                unofficial: true,
                full_address: false,
                base_url: Some(base.to_owned()),
                version_prefix: None,
                path: None,
            },
            endpoints: vec![ChannelEndpoint {
                models: models.iter().map(|m| ModelEntry::plain(*m)).collect(),
                ..ChannelEndpoint::new(protocol)
            }],
            tags: Vec::new(),
            timeout_secs: 0,
            proxy: None,
            param_override: None,
            note: None,
        }
    }

    async fn relay_stream_for_test(
        protocol: Protocol,
        items: Vec<Result<Bytes, GatewayError>>,
    ) -> (AppState, refract_core::ChannelId, Vec<Bytes>) {
        let state = state_with(vec![channel_at(
            "https://upstream.invalid",
            protocol,
            &["test-model"],
        )])
        .await;
        let channel = state.channels()[0].clone();
        let context = StreamContext {
            state: state.clone(),
            owner_id: refract_core::DEFAULT_OWNER_ID,
            key_id: None,
            inbound: protocol,
            request_id: format!("stream-test-{protocol}"),
            started: std::time::Instant::now(),
            channel_id: channel.id,
            channel_name: channel.name,
            upstream_protocol: protocol,
            upstream_model: "test-model".to_owned(),
            attempts: 1,
            ttfb_ms: 1,
            model: "test-model".to_owned(),
        };
        let upstream: refract_upstream::ByteStream = Box::pin(futures::stream::iter(items));
        let (tx, mut rx) = tokio::sync::mpsc::channel(32);

        relay_native_stream(context, upstream, tx).await;

        let mut forwarded = Vec::new();
        while let Some(chunk) = rx.recv().await {
            forwarded.push(chunk);
        }
        (state, channel.id, forwarded)
    }

    /// 等待恰好 N 条日志出现。日志是 fire-and-forget 的后台写入，
    /// 响应返回时可能还没落库，直接断言会飘。
    async fn wait_for_logs(state: &AppState, expected: usize) -> Vec<refract_store::RequestLog> {
        for _ in 0..100 {
            let logs = state
                .log_repo()
                .query(refract_core::DEFAULT_OWNER_ID, &Default::default())
                .await
                .unwrap();
            if logs.len() >= expected {
                assert_eq!(logs.len(), expected);
                return logs;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("expected {expected} request logs, but they never arrived");
    }

    async fn only_log(state: &AppState) -> refract_store::RequestLog {
        wait_for_logs(state, 1).await.into_iter().next().unwrap()
    }

    #[tokio::test]
    async fn chat_request_reaches_the_upstream_and_returns_its_answer() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "chatcmpl-1",
                "model": "gpt-4o",
                "choices": [{
                    "index": 0,
                    "message": { "role": "assistant", "content": "pong" },
                    "finish_reason": "stop"
                }],
                "usage": { "prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4 }
            })))
            .mount(&server)
            .await;

        let state = state_with(vec![channel_at(&server.uri(), Protocol::Chat, &["gpt-4o"])]).await;

        let response = warp::test::request()
            .method("POST")
            .path("/v1/chat/completions")
            .json(&serde_json::json!({
                "model": "gpt-4o",
                "messages": [{ "role": "user", "content": "ping" }]
            }))
            .reply(&crate::routes(state))
            .await;

        assert_eq!(response.status(), 200);
        let body: Value = serde_json::from_slice(response.body()).unwrap();
        assert_eq!(body["choices"][0]["message"]["content"], "pong");
    }

    #[tokio::test]
    async fn embeddings_pass_through_a_chat_endpoint_with_alias_rewrite() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list",
                "data": [{ "object": "embedding", "index": 0, "embedding": [0.1, 0.2] }],
                "model": "text-embedding-3-small",
                "usage": { "prompt_tokens": 7, "total_tokens": 7 }
            })))
            .mount(&server)
            .await;

        // 对外名 my-embed → 上游名 text-embedding-3-small。
        let mut channel = channel_at(&server.uri(), Protocol::Chat, &[]);
        channel.endpoints[0].models =
            vec![ModelEntry::mapped("my-embed", "text-embedding-3-small")];
        let state = state_with(vec![channel]).await;

        let response = warp::test::request()
            .method("POST")
            .path("/v1/embeddings")
            .json(&serde_json::json!({ "model": "my-embed", "input": "hello" }))
            .reply(&crate::routes(state.clone()))
            .await;

        assert_eq!(response.status(), 200);
        let body: Value = serde_json::from_slice(response.body()).unwrap();
        assert_eq!(body["data"][0]["object"], "embedding");

        // 上游收到的必须是重写后的模型名（wiremock 记录的请求）。
        let requests = server.received_requests().await.unwrap();
        let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(sent["model"], "text-embedding-3-small");

        // 日志落库：输入 token 从响应 usage 提取。
        let log = only_log(&state).await;
        assert_eq!(log.model, "my-embed");
        assert_eq!(log.input_tokens, 7);
        assert!(!log.stream);
    }

    #[tokio::test]
    async fn embeddings_do_not_route_to_non_chat_endpoints() {
        // Messages 渠道即使勾了 chat 转换，也不能服务 embeddings —— 透传无转码。
        let mut channel = channel_at(
            "https://upstream.invalid",
            Protocol::Messages,
            &["embed-model"],
        );
        channel.endpoints[0].transcode = TranscodePolicy {
            enabled: true,
            accepted: ProtocolSet::from_iter_protocols([Protocol::Chat]),
        };
        let state = state_with(vec![channel]).await;

        let response = warp::test::request()
            .method("POST")
            .path("/v1/embeddings")
            .json(&serde_json::json!({ "model": "embed-model", "input": "x" }))
            .reply(&crate::routes(state))
            .await;

        assert_eq!(response.status(), 404);
        let body: Value = serde_json::from_slice(response.body()).unwrap();
        let message = body["error"]["message"].as_str().unwrap_or_default();
        assert!(message.contains("chat"), "{message}");
    }

    #[tokio::test]
    async fn native_unary_gateway_preserves_unknown_fields_and_json_bytes() {
        let server = MockServer::start().await;
        let upstream_body = br#"{ "id" : "raw", "future_response" : {"kept":true} }"#;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(upstream_body.as_slice(), "application/json"),
            )
            .mount(&server)
            .await;
        let state = state_with(vec![channel_at(&server.uri(), Protocol::Chat, &["gpt-4o"])]).await;
        let request_body =
            br#"{ "model" : "gpt-4o", "messages" : [], "future_request" : [1,2,3] }"#;

        let response = warp::test::request()
            .method("POST")
            .path("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(request_body)
            .reply(&crate::routes(state))
            .await;

        assert_eq!(response.status(), 200);
        assert_eq!(response.body(), upstream_body.as_slice());
        let received = server.received_requests().await.unwrap();
        assert_eq!(received[0].body.as_slice(), request_body);
    }

    #[tokio::test]
    async fn native_unary_gateway_forwards_status_headers_and_logs_actual_status() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(201)
                    .insert_header("x-request-id", "upstream-request-1")
                    .insert_header("connection", "x-upstream-hop")
                    .insert_header("x-upstream-hop", "must-not-leak")
                    .set_body_json(serde_json::json!({
                        "id": "chatcmpl-created",
                        "model": "gpt-4o",
                        "choices": [{
                            "index": 0,
                            "message": {"role": "assistant", "content": "created"},
                            "finish_reason": "stop"
                        }]
                    })),
            )
            .mount(&server)
            .await;
        let state = state_with(vec![channel_at(&server.uri(), Protocol::Chat, &["gpt-4o"])]).await;

        let response = warp::test::request()
            .method("POST")
            .path("/v1/chat/completions")
            .json(&serde_json::json!({"model": "gpt-4o", "messages": []}))
            .reply(&crate::routes(state.clone()))
            .await;

        assert_eq!(response.status(), 201);
        assert_eq!(
            response
                .headers()
                .get("x-request-id")
                .and_then(|value| value.to_str().ok()),
            Some("upstream-request-1")
        );
        assert!(!response.headers().contains_key("connection"));
        assert!(!response.headers().contains_key("x-upstream-hop"));

        let logs = wait_for_logs(&state, 1).await;
        assert_eq!(logs[0].status, 201);
    }

    #[tokio::test]
    async fn non_gemini_clean_eof_without_terminal_event_is_a_failure() {
        let cases = [
            (
                Protocol::Chat,
                "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
            ),
            (
                Protocol::Messages,
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
            ),
            (
                Protocol::Responses,
                "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"sequence_number\":1,\"item_id\":\"m\",\"output_index\":0,\"content_index\":0,\"delta\":\"hi\"}\n\n",
            ),
        ];

        for (protocol, frame) in cases {
            let (state, channel_id, forwarded) =
                relay_stream_for_test(protocol, vec![Ok(Bytes::copy_from_slice(frame.as_bytes()))])
                    .await;
            assert_eq!(forwarded.concat(), frame.as_bytes());

            let log = only_log(&state).await;
            assert_eq!(log.status, 502, "{protocol}");
            assert_eq!(log.error_kind.as_deref(), Some("UpstreamError"));
            assert!(
                log.error_message
                    .as_deref()
                    .is_some_and(|message| message.contains("completion event")),
                "{protocol}: {:?}",
                log.error_message
            );
            let health = state
                .health_repo()
                .get(channel_id, protocol)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(health.total_failures, 1, "{protocol}");
        }
    }

    #[tokio::test]
    async fn gemini_valid_frame_then_clean_eof_is_success_and_records_usage() {
        let frame = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hi\"}],\"role\":\"model\"},\"finishReason\":\"STOP\",\"index\":0}],\"usageMetadata\":{\"promptTokenCount\":3,\"candidatesTokenCount\":2}}\n\n";
        let (state, channel_id, forwarded) = relay_stream_for_test(
            Protocol::Gemini,
            vec![Ok(Bytes::copy_from_slice(frame.as_bytes()))],
        )
        .await;
        assert_eq!(forwarded.concat(), frame.as_bytes());

        let log = only_log(&state).await;
        assert_eq!(log.status, 200);
        assert_eq!(log.input_tokens, 3);
        assert_eq!(log.output_tokens, 2);
        assert!(log.error_kind.is_none());
        let health = state
            .health_repo()
            .get(channel_id, Protocol::Gemini)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(health.total_requests, 1);
        assert_eq!(health.total_failures, 0);
    }

    #[tokio::test]
    async fn native_chat_stream_usage_is_written_to_the_log() {
        let stream = "data: {\"id\":\"x\",\"model\":\"test-model\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\ndata: {\"id\":\"x\",\"model\":\"test-model\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":5,\"total_tokens\":12}}\n\ndata: [DONE]\n\n";
        let (state, _, _) = relay_stream_for_test(
            Protocol::Chat,
            vec![Ok(Bytes::copy_from_slice(stream.as_bytes()))],
        )
        .await;

        let log = only_log(&state).await;
        assert_eq!(log.status, 200);
        assert_eq!(log.input_tokens, 7);
        assert_eq!(log.output_tokens, 5);
    }

    #[tokio::test]
    async fn native_stream_midflight_timeout_is_failure() {
        let frame = "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n";
        let timeout = GatewayError::new(ErrorKind::Timeout, "upstream stalled");
        let (state, channel_id, _) = relay_stream_for_test(
            Protocol::Chat,
            vec![Ok(Bytes::copy_from_slice(frame.as_bytes())), Err(timeout)],
        )
        .await;

        let log = only_log(&state).await;
        assert_eq!(log.status, 504);
        assert_eq!(log.error_kind.as_deref(), Some("Timeout"));
        let health = state
            .health_repo()
            .get(channel_id, Protocol::Chat)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(health.total_failures, 1);
    }

    #[test]
    fn response_header_filter_removes_hop_by_hop_and_stream_length() {
        let mut source = warp::http::HeaderMap::new();
        source.insert("connection", "x-hop".parse().unwrap());
        source.insert("x-hop", "private".parse().unwrap());
        source.insert("transfer-encoding", "chunked".parse().unwrap());
        source.insert("content-length", "42".parse().unwrap());
        source.insert("x-request-id", "req-1".parse().unwrap());

        let mut unary = warp::http::HeaderMap::new();
        copy_end_to_end_headers(&source, &mut unary, false);
        assert_eq!(unary.get("content-length").unwrap(), "42");
        assert_eq!(unary.get("x-request-id").unwrap(), "req-1");
        assert!(!unary.contains_key("connection"));
        assert!(!unary.contains_key("x-hop"));
        assert!(!unary.contains_key("transfer-encoding"));

        let mut streaming = warp::http::HeaderMap::new();
        copy_end_to_end_headers(&source, &mut streaming, true);
        assert!(!streaming.contains_key("content-length"));
        assert_eq!(streaming.get("x-request-id").unwrap(), "req-1");
    }

    #[tokio::test]
    async fn unknown_model_reports_which_models_exist_not_a_bare_failure() {
        let state = state_with(vec![channel_at(
            "https://x.test",
            Protocol::Chat,
            &["gpt-4o"],
        )])
        .await;

        let response = warp::test::request()
            .method("POST")
            .path("/v1/chat/completions")
            .json(&serde_json::json!({
                "model": "does-not-exist",
                "messages": [{ "role": "user", "content": "hi" }]
            }))
            .reply(&crate::routes(state))
            .await;

        assert_eq!(response.status(), 404);
        let text = String::from_utf8_lossy(response.body());
        assert!(text.contains("does-not-exist"), "got: {text}");
    }

    #[tokio::test]
    async fn protocol_not_permitted_is_rejected_with_an_actionable_message() {
        // 需求 4：未勾选转换的协议打过来必须报错，而不是硬转。
        let mut channel = channel_at("https://x.test", Protocol::Chat, &["gpt-4o"]);
        channel.endpoints[0].transcode = TranscodePolicy::DISABLED;
        let state = state_with(vec![channel]).await;

        let response = warp::test::request()
            .method("POST")
            .path("/v1/messages")
            .json(&serde_json::json!({
                "model": "gpt-4o",
                "max_tokens": 16,
                "messages": [{ "role": "user", "content": "hi" }]
            }))
            .reply(&crate::routes(state))
            .await;

        assert_eq!(response.status(), 400);
        let text = String::from_utf8_lossy(response.body());
        assert!(
            text.contains("chat"),
            "error should name the usable protocol: {text}"
        );
    }

    #[tokio::test]
    async fn anthropic_errors_use_the_anthropic_envelope() {
        // 错误体的形状也是外部契约 —— Anthropic SDK 读 error.type。
        let state = state_with(vec![]).await;

        let response = warp::test::request()
            .method("POST")
            .path("/v1/messages")
            .json(&serde_json::json!({
                "model": "nope",
                "max_tokens": 16,
                "messages": [{ "role": "user", "content": "hi" }]
            }))
            .reply(&crate::routes(state))
            .await;

        let body: Value = serde_json::from_slice(response.body()).unwrap();
        assert_eq!(body["type"], "error");
        assert!(body["error"]["type"].is_string());
    }

    #[tokio::test]
    async fn malformed_json_uses_the_inbound_protocol_error_envelope() {
        let state = state_with(vec![]).await;

        let chat = warp::test::request()
            .method("POST")
            .path("/v1/chat/completions")
            .header("content-type", "application/json")
            .body("{")
            .reply(&crate::routes(state.clone()))
            .await;
        assert_eq!(chat.status(), 400);
        let chat_body: Value = serde_json::from_slice(chat.body()).unwrap();
        assert!(chat_body["error"]["message"].is_string());
        assert!(chat_body["error"]["type"].is_string());

        let messages = warp::test::request()
            .method("POST")
            .path("/v1/messages")
            .header("content-type", "application/json")
            .body("{")
            .reply(&crate::routes(state))
            .await;
        assert_eq!(messages.status(), 400);
        let messages_body: Value = serde_json::from_slice(messages.body()).unwrap();
        assert_eq!(messages_body["type"], "error");
        assert!(messages_body["error"]["type"].is_string());
    }

    #[tokio::test]
    async fn wrong_method_uses_the_protocol_envelope_and_405() {
        let state = state_with(vec![]).await;
        let response = warp::test::request()
            .method("GET")
            .path("/v1/responses")
            .reply(&crate::routes(state))
            .await;

        assert_eq!(response.status(), 405);
        let body: Value = serde_json::from_slice(response.body()).unwrap();
        assert!(
            body["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("POST"))
        );
        assert!(body["error"]["type"].is_string());
    }

    #[tokio::test]
    async fn cross_protocol_transcode_actually_reaches_a_chat_upstream() {
        // Anthropic 客户端 → Chat 上游，这是这个网关最核心的能力。
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "chatcmpl-2",
                "model": "gpt-4o",
                "choices": [{
                    "index": 0,
                    "message": { "role": "assistant", "content": "transcoded" },
                    "finish_reason": "stop"
                }],
                "usage": { "prompt_tokens": 2, "completion_tokens": 2, "total_tokens": 4 }
            })))
            .mount(&server)
            .await;

        let mut channel = channel_at(&server.uri(), Protocol::Chat, &["gpt-4o"]);
        channel.endpoints[0].transcode = TranscodePolicy {
            enabled: true,
            accepted: ProtocolSet::from_iter_protocols([Protocol::Messages]),
        };
        let state = state_with(vec![channel]).await;

        let response = warp::test::request()
            .method("POST")
            .path("/v1/messages")
            .json(&serde_json::json!({
                "model": "gpt-4o",
                "max_tokens": 32,
                "messages": [{ "role": "user", "content": "hi" }]
            }))
            .reply(&crate::routes(state))
            .await;

        assert_eq!(response.status(), 200);
        let body: Value = serde_json::from_slice(response.body()).unwrap();
        // 回给客户端的必须是 Anthropic 形状，不是上游的 Chat 形状。
        assert_eq!(body["type"], "message");
        assert_eq!(body["content"][0]["text"], "transcoded");
    }

    #[tokio::test]
    async fn gemini_takes_model_and_stream_flag_from_the_url() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "candidates": [{
                    "content": { "role": "model", "parts": [{ "text": "hello" }] },
                    "finishReason": "STOP"
                }],
                "usageMetadata": { "promptTokenCount": 1, "candidatesTokenCount": 1 }
            })))
            .mount(&server)
            .await;

        let state = state_with(vec![channel_at(
            &server.uri(),
            Protocol::Gemini,
            &["gemini-2.5-pro"],
        )])
        .await;

        let response = warp::test::request()
            .method("POST")
            .path("/v1beta/models/gemini-2.5-pro:generateContent")
            .json(&serde_json::json!({
                "contents": [{ "role": "user", "parts": [{ "text": "hi" }] }]
            }))
            .reply(&crate::routes(state))
            .await;

        assert_eq!(response.status(), 200);
        let body: Value = serde_json::from_slice(response.body()).unwrap();
        assert_eq!(
            body["candidates"][0]["content"]["parts"][0]["text"],
            "hello"
        );
    }

    #[tokio::test]
    async fn a_successful_request_is_written_to_the_log() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "chatcmpl-3",
                "model": "gpt-4o",
                "choices": [{
                    "index": 0,
                    "message": { "role": "assistant", "content": "ok" },
                    "finish_reason": "stop"
                }],
                "usage": { "prompt_tokens": 7, "completion_tokens": 5, "total_tokens": 12 }
            })))
            .mount(&server)
            .await;

        let state = state_with(vec![channel_at(&server.uri(), Protocol::Chat, &["gpt-4o"])]).await;

        warp::test::request()
            .method("POST")
            .path("/v1/chat/completions")
            .json(&serde_json::json!({
                "model": "gpt-4o",
                "messages": [{ "role": "user", "content": "hi" }]
            }))
            .reply(&crate::routes(state.clone()))
            .await;

        let logs = wait_for_logs(&state, 1).await;
        assert_eq!(logs[0].status, 200);
        assert_eq!(logs[0].input_tokens, 7);
        assert_eq!(logs[0].output_tokens, 5);
    }

    #[tokio::test]
    async fn every_response_carries_the_gateway_request_id() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "chatcmpl-1",
                "model": "gpt-4o",
                "choices": [{
                    "index": 0,
                    "message": { "role": "assistant", "content": "ok" },
                    "finish_reason": "stop"
                }]
            })))
            .mount(&server)
            .await;
        let state = state_with(vec![channel_at(&server.uri(), Protocol::Chat, &["gpt-4o"])]).await;

        // 成功响应：头里的 id 必须能对上落库的那条日志。
        let response = warp::test::request()
            .method("POST")
            .path("/v1/chat/completions")
            .json(&serde_json::json!({"model": "gpt-4o", "messages": []}))
            .reply(&crate::routes(state.clone()))
            .await;
        assert_eq!(response.status(), 200);
        let request_id = response
            .headers()
            .get("x-refract-request-id")
            .and_then(|value| value.to_str().ok())
            .expect("success responses must carry x-refract-request-id")
            .to_owned();
        let log = only_log(&state).await;
        assert_eq!(log.request_id, request_id);

        // 失败响应（未知模型）同样要带 id —— 报障的往往正是失败的请求。
        let failure = warp::test::request()
            .method("POST")
            .path("/v1/chat/completions")
            .json(&serde_json::json!({"model": "ghost", "messages": []}))
            .reply(&crate::routes(state.clone()))
            .await;
        assert_eq!(failure.status(), 404);
        let failure_id = failure
            .headers()
            .get("x-refract-request-id")
            .and_then(|value| value.to_str().ok())
            .expect("error responses must carry x-refract-request-id")
            .to_owned();
        let logs = wait_for_logs(&state, 2).await;
        assert!(
            logs.iter().any(|entry| entry.request_id == failure_id),
            "the failure log must share the id returned to the client"
        );
    }

    #[tokio::test]
    async fn a_failed_request_is_also_logged() {
        // 失败日志是排查「这个模型为什么总是不通」的唯一线索。
        let state = state_with(vec![]).await;

        warp::test::request()
            .method("POST")
            .path("/v1/chat/completions")
            .json(&serde_json::json!({
                "model": "ghost",
                "messages": [{ "role": "user", "content": "hi" }]
            }))
            .reply(&crate::routes(state.clone()))
            .await;

        let logs = wait_for_logs(&state, 1).await;
        assert_eq!(logs[0].status, 404);
        assert!(logs[0].error_message.is_some());
    }

    #[tokio::test]
    async fn model_list_is_derived_from_channels() {
        let state = state_with(vec![
            channel_at("https://a.test", Protocol::Chat, &["gpt-4o"]),
            channel_at("https://b.test", Protocol::Messages, &["claude-4"]),
        ])
        .await;

        let response = warp::test::request()
            .method("GET")
            .path("/v1/models")
            .reply(&crate::routes(state))
            .await;

        assert_eq!(response.status(), 200);
        let body: Value = serde_json::from_slice(response.body()).unwrap();
        let ids: Vec<&str> = body["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["claude-4", "gpt-4o"]);
    }

    #[tokio::test]
    async fn streaming_response_is_sent_as_sse() {
        let server = MockServer::start().await;
        let sse = "data: {\"id\":\"1\",\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n\
                   data: [DONE]\n\n";
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse),
            )
            .mount(&server)
            .await;

        let state = state_with(vec![channel_at(&server.uri(), Protocol::Chat, &["gpt-4o"])]).await;

        let response = warp::test::request()
            .method("POST")
            .path("/v1/chat/completions")
            .json(&serde_json::json!({
                "model": "gpt-4o",
                "stream": true,
                "messages": [{ "role": "user", "content": "hi" }]
            }))
            .reply(&crate::routes(state))
            .await;

        assert_eq!(response.status(), 200);
        let ctypes: Vec<_> = response.headers().get_all("content-type").iter().collect();
        assert_eq!(ctypes.len(), 1, "exactly one content-type: {ctypes:?}");
        assert!(
            ctypes[0].to_str().unwrap().starts_with("text/event-stream"),
            "streaming must not degrade to a JSON response, got {ctypes:?}"
        );
    }
}
