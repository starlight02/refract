//! 上游客户端的真实 HTTP 集成测试。
//!
//! 单元测试只能验证请求头装配与错误映射的纯逻辑。这些测试起一个真实的 HTTP
//! 服务器，验证**真的发出了正确的请求、真的能逐帧读回流式响应**。没有这一层，
//! 「客户端能工作」只是推断而非事实。

use std::time::Duration;

use futures::StreamExt as _;
use refract_core::{Action, Credential, ErrorKind, Protocol, UpstreamAddress};
use refract_upstream::{UpstreamClient, UpstreamClientConfig, UpstreamRequest, probe_models};
use serde_json::json;
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// 指向 mock server 的非官方地址。
fn address(server: &MockServer) -> UpstreamAddress {
    UpstreamAddress {
        unofficial: true,
        full_address: false,
        base_url: Some(server.uri()),
        version_prefix: Some("/v1".into()),
        path: None,
    }
}

/// 完整地址模式，绕过所有拼接与校验。
fn full_address(url: String) -> UpstreamAddress {
    UpstreamAddress {
        unofficial: true,
        full_address: true,
        base_url: Some(url),
        version_prefix: None,
        path: None,
    }
}

fn client() -> UpstreamClient {
    UpstreamClient::new(UpstreamClientConfig {
        timeout: Duration::from_secs(5),
        connect_timeout: Duration::from_secs(2),
        stream_idle_timeout: Duration::from_millis(500),
        ..Default::default()
    })
    .unwrap()
}

#[tokio::test]
async fn chat_request_hits_the_joined_url_with_bearer_auth() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer sk-test"))
        .and(body_json(json!({"model": "gpt-4o", "messages": []})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-1",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"}}]
        })))
        .mount(&server)
        .await;

    let addr = address(&server);
    let cred = Credential::new("sk-test");
    let body = json!({"model": "gpt-4o", "messages": []});
    let response = client()
        .send(UpstreamRequest::post(
            Protocol::Chat,
            &addr,
            &cred,
            "gpt-4o",
            Action::Generate,
            &body,
        ))
        .await
        .unwrap();

    assert_eq!(response.status, 200);
    assert_eq!(response.body["id"], "chatcmpl-1");
}

#[tokio::test]
async fn messages_request_uses_anthropic_headers() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "sk-ant"))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "msg_1"})))
        .mount(&server)
        .await;

    let addr = address(&server);
    let cred = Credential::new("sk-ant");
    let body = json!({"model": "claude-sonnet-4-6", "messages": []});
    let response = client()
        .send(UpstreamRequest::post(
            Protocol::Messages,
            &addr,
            &cred,
            "claude-sonnet-4-6",
            Action::Generate,
            &body,
        ))
        .await
        .unwrap();
    assert_eq!(response.body["id"], "msg_1");
}

#[tokio::test]
async fn gemini_url_substitutes_model_and_action() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1beta/models/gemini-2.5-pro:streamGenerateContent"))
        .and(header("x-goog-api-key", "AIza-test"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"candidates": []})))
        .mount(&server)
        .await;

    let addr = UpstreamAddress {
        unofficial: true,
        full_address: false,
        base_url: Some(server.uri()),
        version_prefix: Some("/v1beta".into()),
        path: None,
    };
    let cred = Credential::new("AIza-test");
    let body = json!({"contents": []});
    let response = client()
        .send(UpstreamRequest::post(
            Protocol::Gemini,
            &addr,
            &cred,
            "gemini-2.5-pro",
            Action::Stream,
            &body,
        ))
        .await
        .unwrap();
    assert_eq!(response.status, 200);
}

#[tokio::test]
async fn upstream_error_body_is_surfaced_with_status() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429).set_body_json(json!({
            "error": {"message": "rate limit reached", "type": "rate_limit_error"}
        })))
        .mount(&server)
        .await;

    let addr = full_address(format!("{}/anything", server.uri()));
    let cred = Credential::new("k");
    let body = json!({});
    let err = client()
        .send(UpstreamRequest::post(
            Protocol::Chat,
            &addr,
            &cred,
            "m",
            Action::Generate,
            &body,
        ))
        .await
        .unwrap_err();

    assert_eq!(err.kind, ErrorKind::RateLimited);
    assert_eq!(err.message, "rate limit reached");
    assert_eq!(err.upstream_status, Some(429));
    assert!(err.is_retryable());
}

#[tokio::test]
async fn malformed_json_response_is_reported_not_panicked() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>oops</html>"))
        .mount(&server)
        .await;

    let addr = full_address(format!("{}/x", server.uri()));
    let cred = Credential::new("k");
    let body = json!({});
    let err = client()
        .send(UpstreamRequest::post(
            Protocol::Chat,
            &addr,
            &cred,
            "m",
            Action::Generate,
            &body,
        ))
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::UpstreamError);
    assert!(err.message.contains("malformed JSON"));
    assert_eq!(err.upstream_body.as_deref(), Some("<html>oops</html>"));
}

#[tokio::test]
async fn raw_unary_request_and_response_preserve_bytes() {
    let server = MockServer::start().await;
    let upstream_body = br#"{ "id" : "raw", "future_field" : {"x":1} }"#;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(upstream_body.as_slice(), "application/json"),
        )
        .mount(&server)
        .await;

    let addr = full_address(format!("{}/v1/chat/completions", server.uri()));
    let cred = Credential::new("k");
    let request_body = br#"{ "model" : "gpt-5", "future_request_field" : [1,2,3] }"#;
    let response = client()
        .send_raw(UpstreamRequest::post_raw(
            Protocol::Chat,
            &addr,
            &cred,
            "gpt-5",
            Action::Generate,
            request_body,
        ))
        .await
        .unwrap();

    assert_eq!(response.body.as_ref(), upstream_body);
    let received = server.received_requests().await.unwrap();
    assert_eq!(received[0].body.as_slice(), request_body);
}

#[tokio::test]
async fn redirects_are_not_followed() {
    // 跟随重定向会把 Authorization 头发给第三方主机，且会掩盖 base_url 配错。
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(302).insert_header("location", "https://evil.example.com/v1"),
        )
        .mount(&server)
        .await;

    let addr = full_address(format!("{}/x", server.uri()));
    let cred = Credential::new("k");
    let body = json!({});
    let err = client()
        .send(UpstreamRequest::post(
            Protocol::Chat,
            &addr,
            &cred,
            "m",
            Action::Generate,
            &body,
        ))
        .await
        .unwrap_err();
    assert_eq!(err.upstream_status, Some(302));
}

#[tokio::test]
async fn sse_stream_yields_frames_in_order() {
    let server = MockServer::start().await;
    let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"He\"}}]}\n\n\
               data: {\"choices\":[{\"delta\":{\"content\":\"llo\"}}]}\n\n\
               data: [DONE]\n\n";
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse),
        )
        .mount(&server)
        .await;

    let addr = full_address(format!("{}/v1/chat/completions", server.uri()));
    let cred = Credential::new("k");
    let body = json!({"stream": true});
    let stream = client()
        .stream(UpstreamRequest::post(
            Protocol::Chat,
            &addr,
            &cred,
            "m",
            Action::Stream,
            &body,
        ))
        .await
        .unwrap();

    let frames: Vec<_> = stream.map(|f| f.unwrap()).collect().await;
    assert_eq!(frames.len(), 3);
    assert!(frames[0].data.contains("He"));
    assert!(frames[1].data.contains("llo"));
    assert!(frames[2].is_done_sentinel());
}

#[tokio::test]
async fn stream_error_response_is_an_error_not_a_frame() {
    // 上游用 4xx + JSON 拒绝流式请求时，绝不能把错误文本当成 SSE 帧下发。
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "error": {"message": "invalid api key"}
        })))
        .mount(&server)
        .await;

    let addr = full_address(format!("{}/v1/chat/completions", server.uri()));
    let cred = Credential::new("bad");
    let body = json!({"stream": true});
    let err = match client()
        .stream(UpstreamRequest::post(
            Protocol::Chat,
            &addr,
            &cred,
            "m",
            Action::Stream,
            &body,
        ))
        .await
    {
        Ok(_) => panic!("expected error"),
        Err(e) => e,
    };

    assert_eq!(err.kind, ErrorKind::Unauthenticated);
    assert_eq!(err.message, "invalid api key");
}

#[tokio::test]
async fn raw_stream_passes_bytes_through_unchanged() {
    let server = MockServer::start().await;
    let sse = "event: message_start\ndata: {\"type\":\"message_start\"}\n\n";
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse),
        )
        .mount(&server)
        .await;

    let addr = full_address(format!("{}/v1/messages", server.uri()));
    let cred = Credential::new("k");
    let body = json!({"stream": true});
    let response = client()
        .stream_raw(UpstreamRequest::post(
            Protocol::Messages,
            &addr,
            &cred,
            "m",
            Action::Stream,
            &body,
        ))
        .await
        .unwrap();

    assert_eq!(response.status, 200);
    // wiremock/reqwest 在流式响应时可能把 text/event-stream 改成 text/plain。
    // 这不影响生产代码：gateway 层会强制设置正确的 Content-Type。
    let ctype = response
        .headers
        .get("content-type")
        .and_then(|v| v.to_str().ok());
    assert!(
        ctype == Some("text/event-stream") || ctype == Some("text/plain"),
        "unexpected content-type: {:?}",
        ctype
    );
    let bytes: Vec<u8> = response
        .stream
        .map(|c| c.unwrap())
        .collect::<Vec<_>>()
        .await
        .concat();
    assert_eq!(String::from_utf8(bytes).unwrap(), sse);
}

/// 起一个**真的会挂住**的 SSE 服务器：发完头和一帧后不再写任何数据，
/// 但保持连接开着。
///
/// 不能用 wiremock 做这件事 —— 它的 `set_delay` 延迟的是整个响应，
/// 客户端拿到的仍是一个完整的 body，根本不构成「流中途卡住」。
/// 而「上游 TCP 还连着但不再发数据」恰恰是真实世界最常见的挂起方式。
async fn spawn_stalling_sse_server() -> String {
    use tokio::io::AsyncWriteExt as _;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                // 读掉请求头，否则客户端可能因写阻塞而先失败。
                let mut buf = [0_u8; 4096];
                let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut buf).await;

                let head = "HTTP/1.1 200 OK\r\n\
                            content-type: text/event-stream\r\n\
                            transfer-encoding: chunked\r\n\r\n";
                if socket.write_all(head.as_bytes()).await.is_err() {
                    return;
                }
                // 一个 chunk：先写长度，再写内容。
                let frame = "data: {\"a\":1}\n\n";
                let chunk = format!("{:x}\r\n{frame}\r\n", frame.len());
                let _ = socket.write_all(chunk.as_bytes()).await;
                let _ = socket.flush().await;

                // 然后什么都不做，连接一直开着 —— 这就是「上游挂住了」。
                tokio::time::sleep(Duration::from_secs(30)).await;
            });
        }
    });

    format!("http://{addr}")
}

/// 持续产出数据、但总时长超过普通请求 deadline 的 SSE 服务。
async fn spawn_slow_healthy_sse_server() -> String {
    use tokio::io::AsyncWriteExt as _;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = [0_u8; 4096];
        let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut buf).await;
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n",
            )
            .await
            .unwrap();

        for data in ["data: 1\n\n", "data: 2\n\n", "data: [DONE]\n\n"] {
            tokio::time::sleep(Duration::from_millis(80)).await;
            let chunk = format!("{:x}\r\n{data}\r\n", data.len());
            socket.write_all(chunk.as_bytes()).await.unwrap();
            socket.flush().await.unwrap();
        }
        socket.write_all(b"0\r\n\r\n").await.unwrap();
        socket.flush().await.unwrap();
        let _ = socket.shutdown().await;
    });

    format!("http://{addr}")
}

#[tokio::test]
async fn stalled_stream_hits_the_idle_timeout() {
    let base = spawn_stalling_sse_server().await;
    let addr = full_address(format!("{base}/v1/chat/completions"));
    let cred = Credential::new("k");
    let body = json!({"stream": true});

    let mut stream = client()
        .stream(UpstreamRequest::post(
            Protocol::Chat,
            &addr,
            &cred,
            "m",
            Action::Stream,
            &body,
        ))
        .await
        .unwrap_or_else(|e| panic!("stream should open: {e}"));

    // 第一帧正常到达。
    let first = stream.next().await.expect("first frame");
    assert_eq!(first.unwrap().data, "{\"a\":1}");

    // 之后上游不再发数据，空闲超时必须把它掐掉，而不是永远等下去。
    let second = stream
        .next()
        .await
        .expect("idle timeout must yield an error");
    let err = match second {
        Ok(frame) => panic!("expected an idle timeout, got another frame: {frame:?}"),
        Err(e) => e,
    };
    assert_eq!(err.kind, ErrorKind::Timeout);
    assert!(err.message.contains("stalled"), "got: {}", err.message);
}

#[tokio::test]
async fn healthy_stream_is_not_cut_off_by_the_unary_timeout() {
    let base = spawn_slow_healthy_sse_server().await;
    let addr = full_address(format!("{base}/v1/chat/completions"));
    let cred = Credential::new("k");
    let body = json!({"stream": true});
    let client = UpstreamClient::new(UpstreamClientConfig {
        timeout: Duration::from_millis(100),
        stream_idle_timeout: Duration::from_millis(2000),
        ..Default::default()
    })
    .unwrap();

    let events: Vec<_> = client
        .stream(UpstreamRequest::post(
            Protocol::Chat,
            &addr,
            &cred,
            "m",
            Action::Stream,
            &body,
        ))
        .await
        .unwrap()
        .collect()
        .await;
    assert_eq!(events.len(), 3, "events were: {events:#?}");
    assert!(events.into_iter().all(|event| event.is_ok()));
}

#[tokio::test]
async fn stream_waiting_for_headers_hits_the_idle_timeout() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("data: [DONE]\n\n")
                .set_delay(Duration::from_millis(300)),
        )
        .mount(&server)
        .await;
    let addr = full_address(format!("{}/v1/chat/completions", server.uri()));
    let cred = Credential::new("k");
    let body = json!({"stream": true});
    let client = UpstreamClient::new(UpstreamClientConfig {
        stream_idle_timeout: Duration::from_millis(50),
        ..Default::default()
    })
    .unwrap();

    let err = match client
        .stream(UpstreamRequest::post(
            Protocol::Chat,
            &addr,
            &cred,
            "m",
            Action::Stream,
            &body,
        ))
        .await
    {
        Ok(_) => panic!("expected the initial idle timeout"),
        Err(error) => error,
    };
    assert_eq!(err.kind, ErrorKind::Timeout);
    assert!(err.message.contains("before response headers"));
}

#[tokio::test]
async fn request_timeout_is_enforced() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({}))
                .set_delay(Duration::from_secs(3)),
        )
        .mount(&server)
        .await;

    let addr = full_address(format!("{}/x", server.uri()));
    let cred = Credential::new("k");
    let body = json!({});
    let mut req = UpstreamRequest::post(Protocol::Chat, &addr, &cred, "m", Action::Generate, &body);
    req.timeout = Some(Duration::from_millis(200));

    let err = client().send(req).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Timeout);
}

#[tokio::test]
async fn extra_headers_are_forwarded() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(header("x-request-source", "refract-test"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .mount(&server)
        .await;

    let addr = full_address(format!("{}/x", server.uri()));
    let cred = Credential::new("k");
    let body = json!({});
    let extra = [("x-request-source".to_owned(), "refract-test".to_owned())];
    let mut req = UpstreamRequest::post(Protocol::Chat, &addr, &cred, "m", Action::Generate, &body);
    req.extra_headers = &extra;

    assert_eq!(client().send(req).await.unwrap().status, 200);
}

#[tokio::test]
async fn request_proxy_overrides_the_global_proxy() {
    let global_proxy = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(502).set_body_string("wrong proxy"))
        .mount(&global_proxy)
        .await;

    let channel_proxy = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer sk-proxy"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "via-channel-proxy"
        })))
        .mount(&channel_proxy)
        .await;

    let client = UpstreamClient::new(UpstreamClientConfig {
        proxy: Some(global_proxy.uri()),
        timeout: Duration::from_secs(5),
        ..Default::default()
    })
    .unwrap();
    let addr = full_address("http://upstream.invalid/v1/chat/completions".into());
    let cred = Credential::new("sk-proxy");
    let body = json!({"model": "gpt-5", "messages": []});
    let channel_proxy_url = channel_proxy.uri();
    let mut request = UpstreamRequest::post(
        Protocol::Chat,
        &addr,
        &cred,
        "gpt-5",
        Action::Generate,
        &body,
    );
    request.proxy = Some(&channel_proxy_url);

    let response = client.send(request).await.unwrap();
    assert_eq!(response.body["id"], "via-channel-proxy");
    assert_eq!(global_proxy.received_requests().await.unwrap().len(), 0);
    assert_eq!(channel_proxy.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn invalid_request_proxy_is_a_configuration_error() {
    let addr = full_address("http://upstream.invalid/v1/chat/completions".into());
    let cred = Credential::new("k");
    let body = json!({});
    let mut request =
        UpstreamRequest::post(Protocol::Chat, &addr, &cred, "m", Action::Generate, &body);
    request.proxy = Some("not a url");

    let err = client().send(request).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Configuration);
    assert!(err.message.contains("invalid upstream proxy"));
}

#[tokio::test]
async fn model_probe_fetches_and_normalizes() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list",
            "data": [{"id": "gpt-5"}, {"id": "gpt-4o"}, {"id": "gpt-5"}]
        })))
        .mount(&server)
        .await;

    let addr = address(&server);
    let models = probe_models(
        &client(),
        Protocol::Chat,
        &addr,
        &Credential::new("k"),
        None,
    )
    .await
    .unwrap();
    let ids: Vec<_> = models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, vec!["gpt-5", "gpt-4o"]);
}

#[tokio::test]
async fn model_probe_uses_gemini_shape() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1beta/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "models": [{"name": "models/gemini-2.5-pro", "displayName": "Gemini 2.5 Pro"}]
        })))
        .mount(&server)
        .await;

    let addr = UpstreamAddress {
        unofficial: true,
        full_address: false,
        base_url: Some(server.uri()),
        version_prefix: Some("/v1beta".into()),
        path: None,
    };
    let models = probe_models(
        &client(),
        Protocol::Gemini,
        &addr,
        &Credential::new("k"),
        None,
    )
    .await
    .unwrap();
    assert_eq!(models[0].id, "gemini-2.5-pro");
}

#[tokio::test]
async fn connection_refused_is_an_upstream_error() {
    // 绑定随机端口后立刻释放 —— 该端口无监听。Unix 通常立刻 RST 成 UpstreamError；
    // Windows 常对刚关闭的端口 SYN 重试，直到 connect_timeout，reqwest 报 Timeout。
    // 合同是结构化传输错误，不是 panic，也没有上游 HTTP 状态。
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = full_address(format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().unwrap()
    ));
    drop(listener);
    let cred = Credential::new("k");
    let body = json!({});
    let err = client()
        .send(UpstreamRequest::post(
            Protocol::Chat,
            &addr,
            &cred,
            "m",
            Action::Generate,
            &body,
        ))
        .await
        .unwrap_err();
    assert!(
        matches!(err.kind, ErrorKind::UpstreamError | ErrorKind::Timeout),
        "err was: {err:#?}"
    );
    assert!(err.upstream_status.is_none(), "err was: {err:#?}");
}

#[tokio::test]
async fn protocol_mismatch_in_path_is_rejected_before_sending() {
    // 非完整地址模式下，把 chat 协议指到 /messages 路径应当在本地就报错，
    // 而不是把请求打出去收一个看不懂的 4xx。
    let addr = UpstreamAddress {
        unofficial: true,
        full_address: false,
        base_url: Some("https://example.com".into()),
        version_prefix: Some("/v1".into()),
        path: Some("/messages".into()),
    };
    let cred = Credential::new("k");
    let body = json!({});
    let err = client()
        .send(UpstreamRequest::post(
            Protocol::Chat,
            &addr,
            &cred,
            "m",
            Action::Generate,
            &body,
        ))
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Configuration);
}
