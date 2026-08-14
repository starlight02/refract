//! OpenAI Realtime API 的 WebSocket 直通桥接。
//!
//! `GET /v1/realtime?model=...` 升级为 WebSocket 后，网关在客户端与上游
//! 之间做纯字节级双向转发 —— 不解析事件、不转码协议。Realtime 是会话式
//! 协议（一个连接里多轮 response.create），转码它是另一个量级的工程；
//! 直通已经解决「密钥集中管理 + 渠道路由」这两个网关的核心价值。
//!
//! 鉴权：支持 `Authorization: Bearer` 头、`?key=` 查询参数，以及浏览器
//! Realtime 客户端使用的 `openai-insecure-api-key.*` WebSocket 子协议。

use futures_util::{SinkExt, StreamExt};
use refract_core::{GatewayError, Protocol};
use tokio_tungstenite::tungstenite;
use warp::{Filter, Rejection, Reply, filters::BoxedFilter};

use crate::auth::{Principal, authenticate};
use crate::error::ProtocolRejection;
use crate::state::{AppState, with_state};

/// `GET /v1/realtime` 路由。
pub fn routes(state: AppState) -> BoxedFilter<(warp::reply::Response,)> {
    warp::path!("v1" / "realtime")
        .and(warp::query::<RealtimeQuery>())
        .and(authenticate(state.authenticator(), Protocol::Chat))
        .and(warp::header::optional::<String>("sec-websocket-protocol"))
        .and(warp::ws())
        .and(with_state(state))
        .and_then(
            |query: RealtimeQuery,
             principal: Principal,
             protocols: Option<String>,
             ws: warp::ws::Ws,
             state: AppState| async move {
                let request_id = uuid::Uuid::new_v4().to_string();
                // 升级前完成路由决策：模型不可路由时给出正常的 HTTP 错误，
                // 而不是让客户端拿到一个立刻断开的 101。
                let model = query.model.clone().ok_or_else(|| {
                    ProtocolRejection::reject_with_id(
                        GatewayError::invalid_request("query parameter `model` is required"),
                        Protocol::Chat,
                        request_id.clone(),
                    )
                })?;
                if !principal.allows_model(&model) {
                    return Err(ProtocolRejection::reject_with_id(
                        GatewayError::new(
                            refract_core::ErrorKind::PermissionDenied,
                            format!("this API key is not allowed to use model `{model}`"),
                        ),
                        Protocol::Chat,
                        request_id,
                    ));
                }
                crate::gateway::enforce_rate_limit(
                    &state,
                    &principal,
                    Protocol::Chat,
                    &request_id,
                )?;
                let concurrency_permit =
                    crate::gateway::enforce_global_limits(&state, Protocol::Chat, &request_id)?;
                let target = resolve_target(&state, &principal, &model)?;
                let response_id = request_id.clone();
                let upgrade = ws.on_upgrade(move |socket| {
                    bridge(state, socket, target, model, request_id, concurrency_permit)
                });
                let mut response = upgrade.into_response();
                response.headers_mut().insert(
                    "x-refract-request-id",
                    response_id.parse().expect("UUID is a valid header value"),
                );
                if protocols
                    .as_deref()
                    .is_some_and(|offered| offered.split(',').any(|item| item.trim() == "realtime"))
                {
                    response.headers_mut().insert(
                        "sec-websocket-protocol",
                        "realtime".parse().expect("static header value"),
                    );
                }
                Ok::<_, Rejection>(response)
            },
        )
        .boxed()
}

#[derive(Debug, serde::Deserialize)]
struct RealtimeQuery {
    model: Option<String>,
}

/// 一次桥接的目标参数。在升级前解析完毕，回调里不再需要路由知识。
struct BridgeTarget {
    /// 上游 WS 地址（含 `?model=` 上游名）。
    url: String,
    /// 上游凭据。
    credential: String,
    /// 渠道快照，记日志用。
    channel_id: i64,
    channel_name: String,
    api_key_id: Option<i64>,
    upstream_model: String,
    extra_headers: Vec<(String, String)>,
}

/// 从渠道快照解析 Realtime 的上游地址与凭据。
fn resolve_target(
    state: &AppState,
    principal: &Principal,
    model: &str,
) -> Result<BridgeTarget, Rejection> {
    let channels = state.channels();
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
    route.attempts.retain(|c| c.protocol() == Protocol::Chat);
    let prioritized = state.executor().prioritize(&route);
    let Some(candidate) = prioritized.first().map(|&index| route.attempts[index]) else {
        return Err(ProtocolRejection::reject(
            GatewayError::not_found(format!(
                "no chat-protocol channel provides model `{model}` for realtime"
            )),
            Protocol::Chat,
        ));
    };

    let upstream_model = candidate.upstream_model();
    let url = realtime_url(candidate.address(), upstream_model)
        .map_err(|error| ProtocolRejection::reject(error, Protocol::Chat))?;

    Ok(BridgeTarget {
        url,
        credential: candidate.credential().expose().to_owned(),
        channel_id: candidate.channel_id(),
        channel_name: candidate.channel.name.clone(),
        api_key_id: principal.key_id(),
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
async fn bridge(
    state: AppState,
    client: warp::ws::WebSocket,
    target: BridgeTarget,
    model: String,
    request_id: String,
    _concurrency_permit: Option<tokio::sync::OwnedSemaphorePermit>,
) {
    let started = std::time::Instant::now();

    let upstream = connect_upstream(&target).await;
    let (mut client_tx, mut client_rx) = client.split();

    let mut upstream = match upstream {
        Ok(socket) => socket,
        Err(error) => {
            tracing::warn!(%error, channel = %target.channel_name, "realtime upstream connect failed");
            // 已经 101 了，只能用 close frame 告知失败原因。
            let _ = client_tx
                .send(warp::ws::Message::close_with(
                    1011_u16,
                    format!("upstream connect failed: {error}"),
                ))
                .await;
            log_session(&state, &target, &request_id, &model, started, 502).await;
            return;
        }
    };

    let mut client_closed_clean = false;
    loop {
        tokio::select! {
            inbound = client_rx.next() => match inbound {
                Some(Ok(message)) => {
                    if message.is_close() {
                        client_closed_clean = true;
                        let _ = upstream.send(tungstenite::Message::Close(None)).await;
                        break;
                    }
                    let Some(converted) = to_tungstenite(message) else { continue };
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
                    client_closed_clean = true;
                    let message = match frame {
                        Some(frame) => warp::ws::Message::close_with(
                            u16::from(frame.code),
                            frame.reason.to_string(),
                        ),
                        None => warp::ws::Message::close(),
                    };
                    let _ = client_tx.send(message).await;
                    break;
                }
                Some(Ok(message)) => {
                    let Some(converted) = to_warp(message) else { continue };
                    if client_tx.send(converted).await.is_err() {
                        break;
                    }
                }
                Some(Err(error)) => {
                    tracing::debug!(%error, "realtime upstream stream error");
                    let _ = client_tx
                        .send(warp::ws::Message::close_with(1011_u16, "upstream error"))
                        .await;
                    break;
                }
                None => {
                    let _ = client_tx.send(warp::ws::Message::close()).await;
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
        .as_str()
        .into_client_request()
        .map_err(|e| e.to_string())?;
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
        .map_err(|e| e.to_string())?;
    Ok(socket)
}

/// warp 帧 → tungstenite 帧。返回 `None` 的帧类型无需转发。
fn to_tungstenite(message: warp::ws::Message) -> Option<tungstenite::Message> {
    if message.is_text() {
        let text = message.to_str().ok()?.to_owned();
        Some(tungstenite::Message::text(text))
    } else if message.is_binary() {
        Some(tungstenite::Message::binary(message.into_bytes()))
    } else if message.is_ping() {
        Some(tungstenite::Message::Ping(message.into_bytes()))
    } else if message.is_pong() {
        Some(tungstenite::Message::Pong(message.into_bytes()))
    } else {
        None
    }
}

/// tungstenite 帧 → warp 帧。
fn to_warp(message: tungstenite::Message) -> Option<warp::ws::Message> {
    match message {
        tungstenite::Message::Text(text) => Some(warp::ws::Message::text(text.as_str())),
        tungstenite::Message::Binary(data) => Some(warp::ws::Message::binary(data.to_vec())),
        tungstenite::Message::Ping(data) => Some(warp::ws::Message::ping(data.to_vec())),
        tungstenite::Message::Pong(data) => Some(warp::ws::Message::pong(data.to_vec())),
        // Close 在主循环里单独处理；Frame 是底层变体，connect_async 不会给。
        tungstenite::Message::Close(_) | tungstenite::Message::Frame(_) => None,
    }
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
    };
    state.metrics().observe(&entry);
    if let Err(error) = state.log_repo().append(&entry).await {
        tracing::warn!(%error, "failed to log realtime session");
    }
}

#[cfg(test)]
mod tests {
    use refract_core::{
        Channel, ChannelEndpoint, ChannelKind, Credential, ModelEntry, UpstreamAddress,
    };
    use refract_store::Database;

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
            name: "realtime".into(),
            kind: ChannelKind::Single(Protocol::Chat),
            enabled: true,
            priority: 0,
            weight: 1,
            credential: Credential::new("sk-realtime"),
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

    #[tokio::test]
    async fn realtime_bridges_text_frames_both_ways() {
        let upstream = spawn_ws_echo_upstream().await;
        let state = state_with_channel(&upstream).await;

        let mut client = warp::test::ws()
            .path("/v1/realtime?model=gpt-realtime")
            .handshake(routes(state.clone()))
            .await
            .expect("websocket handshake succeeds");

        client
            .send(warp::ws::Message::text(r#"{"type":"session.update"}"#))
            .await;
        let reply = client.recv().await.expect("bridged reply");
        assert_eq!(reply.to_str().unwrap(), r#"echo:{"type":"session.update"}"#);

        // 主动关闭后会话被记入日志。
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
        assert!(logged, "realtime session should be logged");
    }

    #[tokio::test]
    async fn realtime_requires_a_routable_model() {
        let state = state_with_channel("http://127.0.0.1:1").await;
        let result = warp::test::ws()
            .path("/v1/realtime?model=ghost")
            .handshake(routes(state))
            .await;
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
