//! OpenAI Realtime API 的 WebSocket 直通桥接。
//!
//! `GET /v1/realtime?model=...` 升级为 WebSocket 后，网关在客户端与上游
//! 之间做纯字节级双向转发 —— 不解析事件、不转码协议。Realtime 是会话式
//! 协议（一个连接里多轮 response.create），转码它是另一个量级的工程；
//! 直通已经解决「密钥集中管理 + 渠道路由」这两个网关的核心价值。
//!
//! 鉴权：支持 `Authorization: Bearer` 头、`?key=` 查询参数，以及浏览器
//! Realtime 客户端使用的 `openai-insecure-api-key.*` WebSocket 子协议。

use std::mem;

use futures_util::{SinkExt, StreamExt};
use http_ws::{CloseReason, Message, ws};
use refract_core::{GatewayError, Protocol};
use tokio_tungstenite::tungstenite;
use xitca_web::WebContext;
use xitca_web::body::{ResponseBody, StreamDataBody};
use xitca_web::error::Error as WebError;
use xitca_web::http::{HeaderValue, WebResponse};

use crate::auth::{Principal, require_gateway};
use crate::error::{AppError, ProtocolRejection};
use crate::state::AppState;

/// `GET /v1/realtime`
pub async fn realtime(mut ctx: WebContext<'_, AppState>) -> Result<WebResponse, WebError> {
    let state = ctx.state().clone();
    let query = ctx.req().uri().query().unwrap_or("");
    let model = url::form_urlencoded::parse(query.as_bytes())
        .find(|(name, _)| name == "model")
        .map(|(_, value)| value.into_owned())
        .filter(|value| !value.is_empty());
    let protocols = ctx
        .req()
        .headers()
        .get("sec-websocket-protocol")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let headers = ctx.req().headers().clone();
    let request_id = uuid::Uuid::new_v4().to_string();

    let model = model.ok_or_else(|| {
        AppError::Protocol(ProtocolRejection::with_id(
            GatewayError::invalid_request("query parameter `model` is required"),
            Protocol::Chat,
            request_id.clone(),
        ))
    })?;
    let principal =
        require_gateway(&state, &headers, ctx.req().uri().query(), Protocol::Chat).await?;
    if !principal.allows_model(&model) {
        return Err(AppError::Protocol(ProtocolRejection::with_id(
            GatewayError::new(
                refract_core::ErrorKind::PermissionDenied,
                format!("this API key is not allowed to use model `{model}`"),
            ),
            Protocol::Chat,
            request_id,
        ))
        .into());
    }
    crate::gateway::enforce_rate_limit(&state, &principal, Protocol::Chat, &request_id)?;
    let concurrency_permit =
        crate::gateway::enforce_global_limits(&state, Protocol::Chat, &request_id)?;
    let target = resolve_target(&state, &principal, &model)?;

    let body = mem::take(ctx.body_get_mut());
    let (decode, mut response, tx) =
        ws(ctx.req(), StreamDataBody::new(body)).map_err(WebError::from_service)?;
    if let Ok(value) = HeaderValue::from_str(&request_id) {
        response.headers_mut().insert("x-refract-request-id", value);
    }
    if protocols
        .as_deref()
        .is_some_and(|offered| offered.split(',').any(|item| item.trim() == "realtime"))
    {
        response.headers_mut().insert(
            "sec-websocket-protocol",
            HeaderValue::from_static("realtime"),
        );
    }

    tokio::task::spawn_local(bridge(
        state,
        decode,
        tx,
        target,
        model,
        request_id,
        concurrency_permit,
    ));

    Ok(response.map(|body| ResponseBody::boxed(StreamDataBody::new(body))))
}

struct BridgeTarget {
    url: String,
    credential: String,
    channel_id: refract_core::ChannelId,
    channel_name: String,
    api_key_id: Option<i64>,
    user_id: Option<i64>,
    upstream_model: String,
    extra_headers: Vec<(String, String)>,
}

/// 从渠道快照解析 Realtime 的上游地址与凭据。
///
/// `resolve_target` 在鉴权之后调用，能拿到 principal，因此渠道池走
/// [`AppState::channels_for`]（共享 + 本用户私有）。
fn resolve_target(
    state: &AppState,
    principal: &Principal,
    model: &str,
) -> Result<BridgeTarget, AppError> {
    let channels = state.channels_for(principal.gateway_user_id());
    let allowed: Vec<_> = channels
        .iter()
        .filter(|channel| principal.allows_channel(channel))
        .collect();
    let mut route = {
        let mut rng = rand::rng();
        state
            .planner()
            .plan(allowed.iter().copied(), model, Protocol::Chat, &mut rng)
    };
    // Realtime 只有 OpenAI 形状，转码无从谈起：只保留原生 chat 端点。
    route
        .attempts
        .retain(|candidate| candidate.protocol() == Protocol::Chat);
    let prioritized = state.executor().prioritize(&route);
    let Some(candidate) = prioritized.first().map(|&index| route.attempts[index]) else {
        return Err(AppError::Protocol(ProtocolRejection::new(
            GatewayError::not_found(format!(
                "no chat-protocol channel provides model `{model}` for realtime"
            )),
            Protocol::Chat,
        )));
    };

    let upstream_model = candidate.upstream_model();
    let url = realtime_url(candidate.address(), upstream_model)
        .map_err(|error| AppError::Protocol(ProtocolRejection::new(error, Protocol::Chat)))?;

    Ok(BridgeTarget {
        url,
        credential: candidate.credential().expose().to_owned(),
        channel_id: candidate.channel_id(),
        channel_name: candidate.channel.name.clone(),
        api_key_id: principal.key_id(),
        user_id: principal.user_id,
        upstream_model: upstream_model.to_owned(),
        extra_headers: candidate.channel.extra_headers.clone(),
    })
}

/// 把 Chat HTTP 地址可靠地转换成 Realtime WebSocket 地址。
///
/// 普通地址模式从 `{base}{prefix}/models` 推导同级 `/realtime`，因此保留
/// 中转站路径前缀。完整地址模式则只接受明确的 `/chat/completions` 或
/// `/realtime` 结尾，避免把未知完整路径静默拼成错误地址。
fn realtime_url(
    address: &refract_core::UpstreamAddress,
    upstream_model: &str,
) -> Result<String, GatewayError> {
    let mut url = address
        .resolve(Protocol::Chat, refract_core::Action::ListModels, "")
        .map_err(|error| {
            GatewayError::new(refract_core::ErrorKind::Configuration, error.to_string())
        })?;

    let path = url.path().trim_end_matches('/');
    let base = if path.ends_with("/realtime") {
        path.to_owned()
    } else if let Some(base) = path.strip_suffix("/chat/completions") {
        format!("{base}/realtime")
    } else if !address.full_address
        && let Some(base) = path.strip_suffix("/models")
    {
        format!("{base}/realtime")
    } else {
        return Err(GatewayError::new(
            refract_core::ErrorKind::Configuration,
            "a full Chat address used for Realtime must end in `/chat/completions` or `/realtime`",
        ));
    };
    url.set_path(&base);

    let websocket_scheme = match url.scheme() {
        "https" => "wss",
        "http" => "ws",
        "wss" => "wss",
        "ws" => "ws",
        scheme => {
            return Err(GatewayError::new(
                refract_core::ErrorKind::Configuration,
                format!("Realtime address uses unsupported URL scheme `{scheme}`"),
            ));
        }
    };
    url.set_scheme(websocket_scheme).map_err(|()| {
        GatewayError::new(
            refract_core::ErrorKind::Configuration,
            "failed to convert the Realtime address to WebSocket",
        )
    })?;

    // 保留中转站自带的查询参数，但由网关掌管 model；Url 负责百分号编码，
    // 模型别名里即使有 `/`、`+` 或空格也不能篡改查询串。
    let existing: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(key, _)| key != "model")
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    url.set_query(None);
    {
        let mut query = url.query_pairs_mut();
        query.extend_pairs(existing.iter().map(|(key, value)| (key, value)));
        query.append_pair("model", upstream_model);
    }
    Ok(url.into())
}

/// 双向桥接主体。任何一侧断开都会把关闭传导到另一侧。
async fn bridge<S>(
    state: AppState,
    mut client_rx: S,
    mut client_tx: http_ws::ResponseSender,
    target: BridgeTarget,
    model: String,
    request_id: String,
    _concurrency_permit: Option<tokio::sync::OwnedSemaphorePermit>,
) where
    S: StreamExt<Item = Result<Message, http_ws::WsError<xitca_web::error::BodyError>>> + Unpin,
{
    let started = std::time::Instant::now();

    let upstream = connect_upstream(&target).await;
    let mut upstream = match upstream {
        Ok(socket) => socket,
        Err(error) => {
            tracing::warn!(%error, channel = %target.channel_name, "realtime upstream connect failed");
            let _ = client_tx
                .close(Some(CloseReason {
                    code: http_ws::CloseCode::Error,
                    description: Some(format!("upstream connect failed: {error}")),
                }))
                .await;
            log_session(&state, &target, &request_id, &model, started, 502).await;
            return;
        }
    };

    let mut client_closed_clean = false;
    loop {
        tokio::select! {
            inbound = client_rx.next() => match inbound {
                Some(Ok(Message::Close(_))) => {
                    client_closed_clean = true;
                    let _ = upstream.send(tungstenite::Message::Close(None)).await;
                    break;
                }
                Some(Ok(message)) => {
                    let Some(converted) = to_upstream(message) else { continue };
                    if upstream.send(converted).await.is_err() {
                        break;
                    }
                }
                Some(Err(_)) | None => {
                    let _ = upstream.send(tungstenite::Message::Close(None)).await;
                    break;
                }
            },
            outbound = upstream.next() => match outbound {
                Some(Ok(tungstenite::Message::Close(frame))) => {
                    let code = frame.as_ref().map_or(1000, |item| u16::from(item.code));
                    client_closed_clean = matches!(code, 1000 | 1001);
                    if !client_closed_clean {
                        tracing::debug!(
                            code,
                            reason = %frame.as_ref().map_or("", |item| item.reason.as_str()),
                            "realtime upstream closed session abnormally"
                        );
                    }
                    let reason = frame.map(|item| CloseReason {
                        code: close_code(u16::from(item.code)),
                        description: Some(item.reason.to_string()),
                    });
                    let _ = client_tx.close(reason).await;
                    break;
                }
                Some(Ok(message)) => {
                    let Some(converted) = to_client(message) else { continue };
                    if send_client(&client_tx, converted).await.is_err() {
                        break;
                    }
                }
                Some(Err(error)) => {
                    tracing::debug!(%error, "realtime upstream stream error");
                    let _ = client_tx
                        .close(Some(CloseReason {
                            code: http_ws::CloseCode::Error,
                            description: Some("upstream error".into()),
                        }))
                        .await;
                    break;
                }
                None => {
                    let _ = client_tx.close(None::<CloseReason>).await;
                    break;
                }
            },
        }
    }

    let status = if client_closed_clean { 200 } else { 499 };
    log_session(&state, &target, &request_id, &model, started, status).await;
}

type UpstreamSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect_upstream(target: &BridgeTarget) -> Result<UpstreamSocket, String> {
    use tungstenite::client::IntoClientRequest;
    let mut request = target
        .url
        .clone()
        .into_client_request()
        .map_err(|error| error.to_string())?;
    let headers = request.headers_mut();
    headers.insert(
        "authorization",
        format!("Bearer {}", target.credential)
            .parse()
            .map_err(|_| "credential contains invalid header characters".to_owned())?,
    );
    for (name, value) in &target.extra_headers {
        let name = tungstenite::http::HeaderName::try_from(name.as_str())
            .map_err(|error| format!("invalid configured header name: {error}"))?;
        let value = tungstenite::http::HeaderValue::try_from(value.as_str())
            .map_err(|error| format!("invalid configured header value: {error}"))?;
        headers.insert(name, value);
    }

    let (socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(|error| error.to_string())?;
    Ok(socket)
}

/// 客户端帧 → tungstenite 帧。
fn to_upstream(message: Message) -> Option<tungstenite::Message> {
    match message {
        Message::Text(text) => Some(tungstenite::Message::text(
            String::from_utf8_lossy(&text).into_owned(),
        )),
        Message::Binary(data) => Some(tungstenite::Message::binary(data.to_vec())),
        Message::Ping(data) => Some(tungstenite::Message::Ping(data.to_vec().into())),
        Message::Pong(data) => Some(tungstenite::Message::Pong(data.to_vec().into())),
        Message::Close(_) | Message::Continuation(_) | Message::Nop => None,
    }
}

/// tungstenite 帧 → 客户端帧。
fn to_client(message: tungstenite::Message) -> Option<Message> {
    match message {
        tungstenite::Message::Text(text) => Some(Message::Text(text.as_bytes().to_vec().into())),
        tungstenite::Message::Binary(data) => Some(Message::Binary(data)),
        tungstenite::Message::Ping(data) => Some(Message::Ping(data)),
        tungstenite::Message::Pong(data) => Some(Message::Pong(data)),
        tungstenite::Message::Close(_) | tungstenite::Message::Frame(_) => None,
    }
}

async fn send_client(
    tx: &http_ws::ResponseSender,
    message: Message,
) -> Result<(), http_ws::ProtocolError> {
    match message {
        Message::Text(text) => tx.text(text).await,
        Message::Binary(data) => tx.binary(data).await,
        Message::Ping(data) => tx.ping(data).await,
        Message::Pong(data) => tx.pong(data).await,
        Message::Close(reason) => {
            let mut tx = tx
                .downgrade()
                .upgrade()
                .ok_or(http_ws::ProtocolError::SendClosed)?;
            tx.close(reason).await
        }
        Message::Continuation(item) => tx.continuation(item).await,
        Message::Nop => Ok(()),
    }
}

fn close_code(code: u16) -> http_ws::CloseCode {
    http_ws::CloseCode::from(code)
}

/// 把一次 Realtime 会话记进请求日志。token 计量不解析（usage 藏在事件流
/// 里且形态未稳定），先保证会话本身可见、可查、可算时长。
async fn log_session(
    state: &AppState,
    target: &BridgeTarget,
    request_id: &str,
    model: &str,
    started: std::time::Instant,
    status: u16,
) {
    let entry = refract_store::NewRequestLog {
        owner_id: refract_core::DEFAULT_OWNER_ID,
        user_id: target.user_id,
        request_id: request_id.to_owned(),
        api_key_id: target.api_key_id,
        channel_id: Some(target.channel_id),
        channel_name: Some(target.channel_name.clone()),
        inbound_protocol: Protocol::Chat,
        upstream_protocol: Protocol::Chat,
        model: model.to_owned(),
        upstream_model: target.upstream_model.clone(),
        stream: true,
        status,
        ttfb_ms: None,
        duration_ms: started.elapsed().as_millis() as u64,
        input_tokens: 0,
        output_tokens: 0,
        cached_tokens: 0,
        cache_write_tokens: 0,
        reasoning_tokens: 0,
        retries: 0,
        cost: 0.0,
        error_kind: (status >= 400).then(|| "upstream_error".to_owned()),
        error_message: (status >= 400).then(|| "realtime session ended abnormally".to_owned()),
        request_body: None,
        response_body: None,
        credential_hint: None,
        affinity_rule: None,
    };
    state.metrics().observe(&entry);
    if let Err(error) = state.log_repo().append(&entry).await {
        tracing::warn!(%error, "failed to log realtime session");
    }
}

#[cfg(test)]
mod tests {
    use futures_util::{SinkExt, StreamExt};
    use refract_core::{
        Channel, ChannelEndpoint, ChannelKind, Credential, ModelEntry, UpstreamAddress,
    };
    use refract_store::Database;
    use tokio_tungstenite::tungstenite;

    use super::*;

    /// 一个只会 echo 文本帧的假 Realtime 上游。
    async fn spawn_ws_echo_upstream() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
                    while let Some(Ok(message)) = socket.next().await {
                        match message {
                            tungstenite::Message::Text(text) => {
                                let reply = format!("echo:{text}");
                                if socket
                                    .send(tungstenite::Message::text(reply))
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                            tungstenite::Message::Close(_) => break,
                            _ => {}
                        }
                    }
                });
            }
        });
        format!("http://{addr}")
    }

    async fn state_with_channel(base_url: &str) -> AppState {
        let db = Database::open_in_memory().await.unwrap();
        let client = refract_upstream::UpstreamClient::new(
            refract_upstream::UpstreamClientConfig::default(),
        )
        .unwrap();
        let state = AppState::bootstrap(db, client, false).await.unwrap();
        let channel = Channel {
            id: 0,
            owner_id: refract_core::DEFAULT_OWNER_ID,
            visibility: refract_core::ChannelVisibility::Shared,
            user_id: None,
            name: "realtime".into(),
            kind: ChannelKind::Single(Protocol::Chat),
            enabled: true,
            priority: 0,
            weight: 1,
            credential: Credential::new("sk-realtime"),
            credentials: Vec::new(),
            key_strategy: Default::default(),
            address: UpstreamAddress {
                unofficial: true,
                full_address: false,
                base_url: Some(base_url.to_owned()),
                version_prefix: None,
                path: None,
            },
            endpoints: vec![ChannelEndpoint {
                models: vec![ModelEntry::plain("gpt-realtime")],
                ..ChannelEndpoint::new(Protocol::Chat)
            }],
            tags: vec![],
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
        };
        state.channel_repo().create(&channel).await.unwrap();
        state.reload_channels().await.unwrap();
        state
    }

    async fn serve_app(state: AppState) -> (std::net::SocketAddr, impl FnOnce()) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let std_listener = listener.into_std().unwrap();
        std_listener.set_nonblocking(true).unwrap();
        let (handle, wait) = crate::start_server(state, std_listener, None).expect("listen");
        let join = std::thread::spawn(move || {
            let _ = wait();
        });
        let stop = move || {
            handle.stop(true);
            let _ = join.join();
        };
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        (addr, stop)
    }

    #[tokio::test]
    async fn realtime_bridges_text_frames_both_ways() {
        let upstream = spawn_ws_echo_upstream().await;
        let state = state_with_channel(&upstream).await;
        let (addr, stop) = serve_app(state.clone()).await;

        let url = format!("ws://{addr}/v1/realtime?model=gpt-realtime");
        let (mut client, _) = tokio_tungstenite::connect_async(url)
            .await
            .expect("websocket handshake succeeds");

        client
            .send(tungstenite::Message::text(r#"{"type":"session.update"}"#))
            .await
            .unwrap();
        let reply = client.next().await.expect("bridged reply").unwrap();
        assert_eq!(
            reply.to_text().unwrap(),
            r#"echo:{"type":"session.update"}"#
        );

        drop(client);
        let mut logged = false;
        for _ in 0..100 {
            let items = state
                .log_repo()
                .query(refract_core::DEFAULT_OWNER_ID, &Default::default())
                .await
                .unwrap();
            if let Some(entry) = items.first() {
                assert_eq!(entry.model, "gpt-realtime");
                assert!(entry.stream);
                logged = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        stop();
        assert!(logged, "realtime session should be logged");
    }

    #[tokio::test]
    async fn realtime_requires_a_routable_model() {
        let state = state_with_channel("http://127.0.0.1:1").await;
        let (addr, stop) = serve_app(state).await;
        let url = format!("ws://{addr}/v1/realtime?model=ghost");
        let result = tokio_tungstenite::connect_async(url).await;
        stop();
        assert!(result.is_err(), "unknown model must fail before upgrade");
    }

    #[test]
    fn realtime_url_preserves_prefix_and_encodes_model_alias() {
        let address = UpstreamAddress {
            unofficial: true,
            full_address: false,
            base_url: Some("https://relay.example/proxy".into()),
            version_prefix: Some("v1".into()),
            path: Some("chat/completions".into()),
        };
        let raw = realtime_url(&address, "gpt/realtime preview+1").unwrap();
        let parsed = url::Url::parse(&raw).unwrap();
        assert_eq!(parsed.scheme(), "wss");
        assert_eq!(parsed.path(), "/proxy/v1/realtime");
        assert_eq!(
            parsed
                .query_pairs()
                .find(|(key, _)| key == "model")
                .map(|(_, value)| value.into_owned()),
            Some("gpt/realtime preview+1".into())
        );
    }

    #[test]
    fn realtime_url_derives_from_a_full_chat_address() {
        let address = UpstreamAddress {
            unofficial: true,
            full_address: true,
            base_url: Some("https://relay.example/openai/v1/chat/completions?region=hk".into()),
            version_prefix: None,
            path: None,
        };
        let raw = realtime_url(&address, "gpt-realtime").unwrap();
        let parsed = url::Url::parse(&raw).unwrap();
        assert_eq!(
            parsed.as_str(),
            "wss://relay.example/openai/v1/realtime?region=hk&model=gpt-realtime"
        );
    }
}
