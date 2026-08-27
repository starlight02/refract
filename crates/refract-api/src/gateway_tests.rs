use super::*;
use refract_core::{
    Channel, ChannelEndpoint, ChannelKind, Credential, ModelEntry, ProtocolSet, TranscodePolicy,
    UpstreamAddress,
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
        credentials: Vec::new(),
        key_strategy: Default::default(),
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
        auto_disabled: false,
        balance: None,
        balance_updated_at: None,
        extra_headers: Vec::new(),
        test_model: None,
        empty_response_retry: Default::default(),
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
        capture_bodies: false,
        request_snapshot: None,
        credential_hint: None,
        affinity: None,
        _concurrency_permit: None,
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

    let response = crate::http_test::TestRequest::post("/v1/chat/completions")
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "messages": [{ "role": "user", "content": "ping" }]
        }))
        .send(state.clone())
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
    channel.endpoints[0].models = vec![ModelEntry::mapped("my-embed", "text-embedding-3-small")];
    let state = state_with(vec![channel]).await;

    let response = crate::http_test::TestRequest::post("/v1/embeddings")
        .json(&serde_json::json!({ "model": "my-embed", "input": "hello" }))
        .send(state.clone())
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
async fn images_generation_passes_through_with_alias_rewrite() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/images/generations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "created": 1, "data": [{ "b64_json": "aGk=" }]
        })))
        .mount(&server)
        .await;

    let mut channel = channel_at(&server.uri(), Protocol::Chat, &[]);
    channel.endpoints[0].models = vec![ModelEntry::mapped("my-image", "gpt-image-1")];
    let state = state_with(vec![channel]).await;

    let response = crate::http_test::TestRequest::post("/v1/images/generations")
        .json(&serde_json::json!({ "model": "my-image", "prompt": "a cat" }))
        .send(state.clone())
        .await;

    assert_eq!(response.status(), 200);
    let requests = server.received_requests().await.unwrap();
    let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(sent["model"], "gpt-image-1");

    let log = only_log(&state).await;
    assert_eq!(log.model, "my-image");
    // 图像端点没有 token 用量，0 是准确值。
    assert_eq!(log.input_tokens, 0);
}

#[tokio::test]
async fn audio_transcription_multipart_extracts_model_and_rewrites_alias() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/transcriptions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "text": "hello world" })),
        )
        .mount(&server)
        .await;

    let mut channel = channel_at(&server.uri(), Protocol::Chat, &[]);
    channel.endpoints[0].models = vec![ModelEntry::mapped("stt", "whisper-1")];
    let state = state_with(vec![channel]).await;

    let form = concat!(
        "--BOUNDARY\r\n",
        "Content-Disposition: form-data; name=\"model\"\r\n",
        "\r\n",
        "stt\r\n",
        "--BOUNDARY\r\n",
        "Content-Disposition: form-data; name=\"file\"; filename=\"a.mp3\"\r\n",
        "Content-Type: audio/mpeg\r\n",
        "\r\n",
        "FAKEAUDIO\r\n",
        "--BOUNDARY--\r\n",
    );
    let response = crate::http_test::TestRequest::post("/v1/audio/transcriptions")
        .header("content-type", "multipart/form-data; boundary=BOUNDARY")
        .body(form)
        .send(state.clone())
        .await;

    assert_eq!(response.status(), 200);

    // 上游收到：model 字段被改写为上游名，文件部分原样，boundary 保留。
    let requests = server.received_requests().await.unwrap();
    let sent = std::str::from_utf8(&requests[0].body).unwrap();
    assert!(sent.contains("whisper-1"), "alias should be rewritten");
    assert!(!sent.contains("\r\nstt\r\n"), "original alias must be gone");
    assert!(sent.contains("FAKEAUDIO"), "file bytes must be untouched");
    let content_type = requests[0].headers.get("content-type").unwrap();
    assert!(
        content_type.to_str().unwrap().contains("boundary=BOUNDARY"),
        "multipart boundary must ride along"
    );

    let log = only_log(&state).await;
    assert_eq!(log.model, "stt");
}

#[tokio::test]
async fn multipart_without_model_field_is_rejected() {
    let state = state_with(vec![]).await;
    let form = concat!(
        "--B\r\n",
        "Content-Disposition: form-data; name=\"file\"; filename=\"a.mp3\"\r\n",
        "\r\n",
        "DATA\r\n",
        "--B--\r\n",
    );
    let response = crate::http_test::TestRequest::post("/v1/audio/transcriptions")
        .header("content-type", "multipart/form-data; boundary=B")
        .body(form)
        .send(state.clone())
        .await;
    assert_eq!(response.status(), 400);
}

#[tokio::test]
async fn count_tokens_passes_through_messages_endpoints() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages/count_tokens"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "input_tokens": 42 })),
        )
        .mount(&server)
        .await;

    let channel = channel_at(&server.uri(), Protocol::Messages, &["claude-sonnet-4-6"]);
    let state = state_with(vec![channel]).await;

    let response = crate::http_test::TestRequest::post("/v1/messages/count_tokens")
        .json(&serde_json::json!({
            "model": "claude-sonnet-4-6",
            "messages": [{ "role": "user", "content": "hi" }]
        }))
        .send(state.clone())
        .await;

    assert_eq!(response.status(), 200);
    let body: Value = serde_json::from_slice(response.body()).unwrap();
    assert_eq!(body["input_tokens"], 42);

    let log = only_log(&state).await;
    assert_eq!(log.input_tokens, 42);
    assert_eq!(log.inbound_protocol, Protocol::Messages.as_str());
}

#[tokio::test]
async fn gemini_count_tokens_routes_by_path() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1beta/models/gemini-2.5-pro:countTokens"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "totalTokens": 9 })),
        )
        .mount(&server)
        .await;

    let channel = channel_at(&server.uri(), Protocol::Gemini, &["gemini-2.5-pro"]);
    let state = state_with(vec![channel]).await;

    let response = crate::http_test::TestRequest::post("/v1beta/models/gemini-2.5-pro:countTokens")
        .json(&serde_json::json!({
            "contents": [{ "parts": [{ "text": "hi" }] }]
        }))
        .send(state.clone())
        .await;

    assert_eq!(response.status(), 200);
    let body: Value = serde_json::from_slice(response.body()).unwrap();
    assert_eq!(body["totalTokens"], 9);

    let log = only_log(&state).await;
    assert_eq!(log.input_tokens, 9);
}

#[tokio::test]
async fn model_discovery_endpoints_cover_openai_and_gemini_shapes() {
    let state = state_with(vec![channel_at(
        "https://upstream.invalid",
        Protocol::Chat,
        &["gpt-4o"],
    )])
    .await;
    // OpenAI 单模型查询。
    let response = crate::http_test::TestRequest::get("/v1/models/gpt-4o")
        .send(state.clone())
        .await;
    assert_eq!(response.status(), 200);
    let body: Value = serde_json::from_slice(response.body()).unwrap();
    assert_eq!(body["id"], "gpt-4o");
    assert_eq!(body["object"], "model");

    let missing = crate::http_test::TestRequest::get("/v1/models/ghost")
        .send(state.clone())
        .await;
    assert_eq!(missing.status(), 404);

    // 带命名空间的模型 id 含 `/`，也必须能查到（SDK 启动校验依赖）。
    let ns_state = state_with(vec![channel_at(
        "https://upstream.invalid",
        Protocol::Chat,
        &["openai/gpt-4o"],
    )])
    .await;
    let namespaced = crate::http_test::TestRequest::get("/v1/models/openai/gpt-4o")
        .send(ns_state.clone())
        .await;
    assert_eq!(namespaced.status(), 200);
    let body: Value = serde_json::from_slice(namespaced.body()).unwrap();
    assert_eq!(body["id"], "openai/gpt-4o");

    // Gemini 形状清单。
    let gemini = crate::http_test::TestRequest::get("/v1beta/models")
        .send(state.clone())
        .await;
    assert_eq!(gemini.status(), 200);
    let body: Value = serde_json::from_slice(gemini.body()).unwrap();
    assert_eq!(body["models"][0]["name"], "models/gpt-4o");

    let listed = crate::http_test::TestRequest::get("/v1/models")
        .send(state.clone())
        .await;
    assert_eq!(listed.status(), 200);
    let body: Value = serde_json::from_slice(listed.body()).unwrap();
    assert_eq!(body["object"], "list");
    assert_eq!(body["data"][0]["id"], "gpt-4o");

    let gemini_one = crate::http_test::TestRequest::get("/v1beta/models/gpt-4o")
        .send(state.clone())
        .await;
    assert_eq!(gemini_one.status(), 200);
    let body: Value = serde_json::from_slice(gemini_one.body()).unwrap();
    assert_eq!(body["name"], "models/gpt-4o");

    let gemini_prefixed = crate::http_test::TestRequest::get("/v1beta/models/models/gpt-4o")
        .send(state.clone())
        .await;
    assert_eq!(gemini_prefixed.status(), 200);

    let gemini_missing = crate::http_test::TestRequest::get("/v1beta/models/ghost")
        .send(state.clone())
        .await;
    assert_eq!(gemini_missing.status(), 404);
}

#[tokio::test]
async fn legacy_completions_pass_through_chat_endpoints() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "cmpl-1",
            "object": "text_completion",
            "model": "codestral-fim",
            "choices": [{ "index": 0, "text": "println!(\"hi\")", "finish_reason": "stop" }],
            "usage": { "prompt_tokens": 4, "completion_tokens": 3, "total_tokens": 7 }
        })))
        .mount(&server)
        .await;

    let state = state_with(vec![channel_at(
        &server.uri(),
        Protocol::Chat,
        &["codestral-fim"],
    )])
    .await;
    let response = crate::http_test::TestRequest::post("/v1/completions")
        .json(&serde_json::json!({ "model": "codestral-fim", "prompt": "fn main() {" }))
        .send(state.clone())
        .await;

    assert_eq!(response.status(), 200);
    let body: Value = serde_json::from_slice(response.body()).unwrap();
    assert_eq!(body["object"], "text_completion");

    // legacy completions 的输入侧用量也要记进日志，否则计费缺口。
    let log = only_log(&state).await;
    assert_eq!(log.input_tokens, 4);
}

#[tokio::test]
async fn gemini_embed_content_routes_natively() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1beta/models/text-embedding-004:embedContent"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "embedding": { "values": [0.1, 0.2, 0.3] }
        })))
        .mount(&server)
        .await;

    let channel = channel_at(&server.uri(), Protocol::Gemini, &["text-embedding-004"]);
    let state = state_with(vec![channel]).await;

    let response =
        crate::http_test::TestRequest::post("/v1beta/models/text-embedding-004:embedContent")
            .json(&serde_json::json!({
                "content": { "parts": [{ "text": "hello" }] }
            }))
            .send(state.clone())
            .await;

    assert_eq!(response.status(), 200);
    let body: Value = serde_json::from_slice(response.body()).unwrap();
    assert_eq!(body["embedding"]["values"][0], 0.1);
}

#[tokio::test]
async fn gemini_batch_embed_routes_natively() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1beta/models/text-embedding-004:batchEmbedContents"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "embeddings": [{ "values": [0.5] }]
        })))
        .mount(&server)
        .await;

    let channel = channel_at(&server.uri(), Protocol::Gemini, &["text-embedding-004"]);
    let state = state_with(vec![channel]).await;

    let response =
        crate::http_test::TestRequest::post("/v1beta/models/text-embedding-004:batchEmbedContents")
            .json(&serde_json::json!({
                "requests": [{ "model": "models/text-embedding-004",
                               "content": { "parts": [{ "text": "hi" }] } }]
            }))
            .send(state.clone())
            .await;

    assert_eq!(response.status(), 200);
}

#[tokio::test]
async fn unknown_gemini_verbs_are_rejected_up_front() {
    let state = state_with(vec![]).await;
    let response = crate::http_test::TestRequest::post("/v1beta/models/gemini-2.5-pro:tuneModel")
        .json(&serde_json::json!({}))
        .send(state.clone())
        .await;

    assert_eq!(response.status(), 400);
    let body: Value = serde_json::from_slice(response.body()).unwrap();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unsupported Gemini action")
    );
}

#[tokio::test]
async fn tts_speech_passes_binary_audio_back() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/speech"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "audio/mpeg")
                .set_body_bytes(b"ID3FAKEMP3".to_vec()),
        )
        .mount(&server)
        .await;

    let channel = channel_at(&server.uri(), Protocol::Chat, &["tts-1"]);
    let state = state_with(vec![channel]).await;

    let response = crate::http_test::TestRequest::post("/v1/audio/speech")
        .json(&serde_json::json!({ "model": "tts-1", "input": "hi", "voice": "alloy" }))
        .send(state.clone())
        .await;

    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "audio/mpeg"
    );
    assert_eq!(response.body(), b"ID3FAKEMP3");
}

#[test]
fn multipart_model_extraction_handles_standard_forms() {
    let form =
        b"--B\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nwhisper-1\r\n--B--\r\n";
    assert_eq!(multipart_model(form).as_deref(), Some("whisper-1"));

    let no_model = b"--B\r\nContent-Disposition: form-data; name=\"file\"\r\n\r\nX\r\n--B--\r\n";
    assert_eq!(multipart_model(no_model), None);

    // 属性边界：`filename="model"` 不能冒充 model 字段；
    // 真正的 model 字段排在后面也要能找到。
    let filename_trap =
        b"--B\r\nContent-Disposition: form-data; name=\"file\"; filename=\"model\"\r\nContent-Type: application/octet-stream\r\n\r\nfake\r\n--B\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nwhisper-1\r\n--B--\r\n";
    assert_eq!(multipart_model(filename_trap).as_deref(), Some("whisper-1"));
}

#[test]
fn body_snapshot_truncates_on_char_boundary() {
    let small = body_snapshot(b"{\"a\":1}");
    assert_eq!(small, "{\"a\":1}");

    // 用多字节字符填满，截断点不能落在字符中间。
    let big = "好".repeat(BODY_SNAPSHOT_LIMIT / 3 + 100);
    let cut = body_snapshot(big.as_bytes());
    assert!(cut.contains("[truncated"));
    assert!(cut.len() < big.len());
}

#[tokio::test]
async fn bodies_are_captured_into_the_log_and_served_by_detail_api() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-1",
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "captured!" },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
        })))
        .mount(&server)
        .await;

    let state = state_with(vec![channel_at(&server.uri(), Protocol::Chat, &["gpt-4o"])]).await;
    let response = crate::http_test::TestRequest::post("/v1/chat/completions")
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "messages": [{ "role": "user", "content": "secret-ping" }]
        }))
        .send(state.clone())
        .await;
    assert_eq!(response.status(), 200);

    let log = only_log(&state).await;
    // 列表查询不带正文。
    assert!(log.request_body.is_none());
    assert!(log.response_body.is_none());

    // 单条详情带完整正文。
    let full = state
        .log_repo()
        .get(refract_core::DEFAULT_OWNER_ID, log.id)
        .await
        .unwrap();
    assert!(
        full.request_body
            .as_deref()
            .unwrap()
            .contains("secret-ping")
    );
    assert!(full.response_body.as_deref().unwrap().contains("captured!"));
}

#[tokio::test]
async fn body_capture_can_be_disabled() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "c", "model": "gpt-4o",
            "choices": [{ "index": 0, "message": { "role": "assistant", "content": "x" }, "finish_reason": "stop" }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
        })))
        .mount(&server)
        .await;

    let state = state_with(vec![channel_at(&server.uri(), Protocol::Chat, &["gpt-4o"])]).await;
    state
        .settings_repo()
        .set_capture_bodies(false)
        .await
        .unwrap();
    state.reload_capture_bodies().await.unwrap();

    let response = crate::http_test::TestRequest::post("/v1/chat/completions")
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "messages": [{ "role": "user", "content": "private" }]
        }))
        .send(state.clone())
        .await;
    assert_eq!(response.status(), 200);

    let log = only_log(&state).await;
    let full = state
        .log_repo()
        .get(refract_core::DEFAULT_OWNER_ID, log.id)
        .await
        .unwrap();
    assert!(full.request_body.is_none());
    assert!(full.response_body.is_none());
}

#[tokio::test]
async fn failed_requests_keep_the_upstream_error_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_string(r#"{"error":{"message":"upstream says no"}}"#),
        )
        .mount(&server)
        .await;

    let state = state_with(vec![channel_at(&server.uri(), Protocol::Chat, &["gpt-4o"])]).await;
    let response = crate::http_test::TestRequest::post("/v1/chat/completions")
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "messages": [{ "role": "user", "content": "boom" }]
        }))
        .send(state.clone())
        .await;
    assert_eq!(response.status(), 400);

    let log = only_log(&state).await;
    let full = state
        .log_repo()
        .get(refract_core::DEFAULT_OWNER_ID, log.id)
        .await
        .unwrap();
    assert!(full.request_body.as_deref().unwrap().contains("boom"));
    assert!(
        full.response_body
            .as_deref()
            .unwrap()
            .contains("upstream says no")
    );
}

#[tokio::test]
async fn request_cost_is_priced_into_the_log() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-1",
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "ok" },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 1_000_000, "completion_tokens": 500_000, "total_tokens": 1_500_000 }
        })))
        .mount(&server)
        .await;

    let state = state_with(vec![channel_at(&server.uri(), Protocol::Chat, &["gpt-4o"])]).await;
    state
        .settings_repo()
        .set_pricing(&[refract_store::ModelPrice {
            pattern: "gpt-4o".into(),
            input_per_m: 2.0,
            output_per_m: 8.0,
            cached_input_per_m: None,
            cache_write_per_m: None,
        }])
        .await
        .unwrap();
    state.reload_pricing().await.unwrap();

    let response = crate::http_test::TestRequest::post("/v1/chat/completions")
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "messages": [{ "role": "user", "content": "hi" }]
        }))
        .send(state.clone())
        .await;
    assert_eq!(response.status(), 200);

    // 1M 输入 × $2/M + 0.5M 输出 × $8/M = $6。
    let log = only_log(&state).await;
    assert!((log.cost - 6.0).abs() < 1e-9, "cost = {}", log.cost);
}

#[tokio::test]
async fn repeated_auth_failures_auto_disable_the_channel() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
            "error": { "message": "invalid api key", "type": "invalid_request_error" }
        })))
        .mount(&server)
        .await;

    let state = state_with(vec![channel_at(&server.uri(), Protocol::Chat, &["gpt-4o"])]).await;
    let channel_id = state.channels()[0].id;

    // 连续三次凭据错误触发自动禁用（阈值 = 3）。
    for _ in 0..3 {
        let response = crate::http_test::TestRequest::post("/v1/chat/completions")
            .json(&serde_json::json!({
                "model": "gpt-4o",
                "messages": [{ "role": "user", "content": "hi" }]
            }))
            .send(state.clone())
            .await;
        assert_eq!(response.status(), 401);
    }

    // 事件消费在后台任务里，轮询等它落库。
    let mut disabled = false;
    for _ in 0..100 {
        let channel = state
            .channel_repo()
            .get(refract_core::DEFAULT_OWNER_ID, channel_id)
            .await
            .unwrap();
        if channel.auto_disabled && !channel.enabled {
            disabled = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        disabled,
        "channel should be auto-disabled after 3 auth failures"
    );

    // 手动重新启用会清掉自动禁用标记。
    state
        .channel_repo()
        .set_enabled(refract_core::DEFAULT_OWNER_ID, channel_id, true)
        .await
        .unwrap();
    let channel = state
        .channel_repo()
        .get(refract_core::DEFAULT_OWNER_ID, channel_id)
        .await
        .unwrap();
    assert!(channel.enabled);
    assert!(!channel.auto_disabled);
}

#[tokio::test]
async fn breaker_suspension_fires_the_webhook() {
    // 假上游：一直 500。
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&upstream)
        .await;
    // webhook 接收端。
    let hook = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/notify"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&hook)
        .await;

    let state = state_with(vec![channel_at(
        &upstream.uri(),
        Protocol::Chat,
        &["gpt-4o"],
    )])
    .await;
    state
        .settings_repo()
        .set_webhook_url(Some(&format!("{}/notify", hook.uri())))
        .await
        .unwrap();
    state.reload_webhook().await.unwrap();
    // 阈值降到 1：第一次失败即熔断。
    state
        .settings_repo()
        .set_breaker_policy(&refract_store::BreakerPolicy {
            failure_threshold: 1,
            base_cooldown_secs: 30,
            max_cooldown_secs: 900,
        })
        .await
        .unwrap();
    state.reload_breaker().await.unwrap();

    let response = crate::http_test::TestRequest::post("/v1/chat/completions")
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "messages": [{ "role": "user", "content": "hi" }]
        }))
        .send(state.clone())
        .await;
    assert!(response.status().is_server_error());

    // webhook 送达是异步的，轮询 mock 收件箱。
    let mut delivered = false;
    for _ in 0..100 {
        let requests = hook.received_requests().await.unwrap();
        if let Some(first) = requests.first() {
            let body: Value = serde_json::from_slice(&first.body).unwrap();
            assert_eq!(body["event"], "endpoint.suspended");
            assert_eq!(body["source"], "refract");
            delivered = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(delivered, "suspension webhook should be delivered");
}

#[tokio::test]
async fn anthropic_cache_traffic_is_billed_and_logged() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "msg-1",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-4-6",
            "content": [{ "type": "text", "text": "ok" }],
            "stop_reason": "end_turn",
            // Anthropic 口径：input_tokens 不含缓存读写。
            "usage": {
                "input_tokens": 400_000,
                "output_tokens": 200_000,
                "cache_read_input_tokens": 500_000,
                "cache_creation_input_tokens": 100_000
            }
        })))
        .mount(&server)
        .await;

    let state = state_with(vec![channel_at(
        &server.uri(),
        Protocol::Messages,
        &["claude-sonnet-4-6"],
    )])
    .await;
    state
        .settings_repo()
        .set_pricing(&[refract_store::ModelPrice {
            pattern: "claude-*".into(),
            input_per_m: 3.0,
            output_per_m: 15.0,
            cached_input_per_m: Some(0.3),
            cache_write_per_m: Some(3.75),
        }])
        .await
        .unwrap();
    state.reload_pricing().await.unwrap();

    let response = crate::http_test::TestRequest::post("/v1/messages")
        .json(&serde_json::json!({
            "model": "claude-sonnet-4-6",
            "max_tokens": 100,
            "messages": [{ "role": "user", "content": "hi" }]
        }))
        .send(state.clone())
        .await;
    assert_eq!(response.status(), 200);

    let log = only_log(&state).await;
    // 计费口径归一：总输入 = 400k + 500k(读) + 100k(写) = 1M。
    assert_eq!(log.input_tokens, 1_000_000);
    assert_eq!(log.cached_tokens, 500_000);
    assert_eq!(log.cache_write_tokens, 100_000);
    // 成本 = 0.4M×$3 + 0.5M×$0.3 + 0.1M×$3.75 + 0.2M×$15 = $4.725
    let expected = 0.4 * 3.0 + 0.5 * 0.3 + 0.1 * 3.75 + 0.2 * 15.0;
    assert!((log.cost - expected).abs() < 1e-9, "cost = {}", log.cost);
}

#[tokio::test]
async fn rate_limited_key_gets_429_with_retry_after() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-1",
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "ok" },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
        })))
        .mount(&server)
        .await;

    let db = Database::open_in_memory().await.unwrap();
    refract_store::ChannelRepo::new(db.clone())
        .create(&channel_at(&server.uri(), Protocol::Chat, &["gpt-4o"]))
        .await
        .unwrap();
    let (_, plaintext) = refract_store::ApiKeyRepo::new(db.clone())
        .create(
            refract_core::DEFAULT_OWNER_ID,
            refract_store::NewApiKey {
                name: "throttled".into(),
                rpm_limit: 1,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let client = refract_upstream::UpstreamClient::new(Default::default()).unwrap();
    let state = AppState::bootstrap(db, client, true).await.unwrap();
    let request = || {
        crate::http_test::TestRequest::post("/v1/chat/completions")
            .header("authorization", format!("Bearer {plaintext}"))
            .json(&serde_json::json!({
                "model": "gpt-4o",
                "messages": [{ "role": "user", "content": "hi" }]
            }))
    };

    let first = request().send(state.clone()).await;
    assert_eq!(first.status(), 200);

    // 同一分钟内的第二个请求超过 rpm=1，429 且带 Retry-After。
    let second = request().send(state.clone()).await;
    assert_eq!(second.status(), 429);
    let retry_after: u64 = second
        .headers()
        .get("retry-after")
        .expect("retry-after header")
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((1..=60).contains(&retry_after));
    let body: Value = serde_json::from_slice(second.body()).unwrap();
    assert!(body["error"]["message"].as_str().unwrap().contains("RPM"));
}

/// 全局 TPM 在免鉴权模式下也必须生效：这是 `record()` 无条件给全局窗口
/// 记账的唯一验收点 —— 只挂密钥的旧实现在这里会放行第二个请求。
#[tokio::test]
async fn global_tpm_blocks_the_next_request_without_any_api_key() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-1",
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "ok" },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 90, "completion_tokens": 10, "total_tokens": 100 }
        })))
        .mount(&server)
        .await;

    // require_auth = false：没有任何网关密钥，key_id 恒为 None。
    let state = state_with(vec![channel_at(&server.uri(), Protocol::Chat, &["gpt-4o"])]).await;
    state
        .settings_repo()
        .set_global_limits(&refract_store::GlobalLimits {
            tpm: 100,
            ..Default::default()
        })
        .await
        .unwrap();
    state.reload_global_limits().await.unwrap();
    let request = || {
        crate::http_test::TestRequest::post("/v1/chat/completions").json(&serde_json::json!({
            "model": "gpt-4o",
            "messages": [{ "role": "user", "content": "hi" }]
        }))
    };

    // 第一个请求放行（窗口初始为空），并把 100 token 计入全局窗口。
    assert_eq!(request().send(state.clone()).await.status(), 200);

    // 窗口已达 tpm=100 上限，下一个请求被挡。
    let blocked = request().send(state.clone()).await;
    assert_eq!(blocked.status(), 429);
    let retry_after: u64 = blocked
        .headers()
        .get("retry-after")
        .expect("retry-after header")
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((1..=60).contains(&retry_after));
    let body: Value = serde_json::from_slice(blocked.body()).unwrap();
    let message = body["error"]["message"].as_str().unwrap();
    // 报的是 TPM 维度与 TPM 上限，而不是 RPM。
    assert!(message.contains("TPM"), "{message}");
    assert!(message.contains("100"), "{message}");
    assert!(!message.contains("RPM"), "{message}");
}

/// 全局 RPM 与 TPM 同时配置时，RPM 先触发；且请求只计一次 —— 两个维度
/// 共用一个窗口条目，分开 admit 会让 requests 计数翻倍。
#[tokio::test]
async fn global_rpm_and_tpm_share_one_window_and_report_the_rpm_dimension() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-1",
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "ok" },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
        })))
        .mount(&server)
        .await;

    let state = state_with(vec![channel_at(&server.uri(), Protocol::Chat, &["gpt-4o"])]).await;
    state
        .settings_repo()
        .set_global_limits(&refract_store::GlobalLimits {
            rpm: 2,
            tpm: 1_000_000,
            ..Default::default()
        })
        .await
        .unwrap();
    state.reload_global_limits().await.unwrap();
    let request = || {
        crate::http_test::TestRequest::post("/v1/chat/completions").json(&serde_json::json!({
            "model": "gpt-4o",
            "messages": [{ "role": "user", "content": "hi" }]
        }))
    };

    // rpm=2 意味着恰好放行两个请求：若两个维度各计一次 requests，第二个就会被挡。
    assert_eq!(request().send(state.clone()).await.status(), 200);
    assert_eq!(request().send(state.clone()).await.status(), 200);

    let blocked = request().send(state.clone()).await;
    assert_eq!(blocked.status(), 429);
    let message = serde_json::from_slice::<Value>(blocked.body()).unwrap()["error"]["message"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(message.contains("RPM"), "{message}");
    assert!(!message.contains("TPM"), "{message}");
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

    let response = crate::http_test::TestRequest::post("/v1/embeddings")
        .json(&serde_json::json!({ "model": "embed-model", "input": "x" }))
        .send(state.clone())
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
            ResponseTemplate::new(200).set_body_raw(upstream_body.as_slice(), "application/json"),
        )
        .mount(&server)
        .await;
    let state = state_with(vec![channel_at(&server.uri(), Protocol::Chat, &["gpt-4o"])]).await;
    let request_body = br#"{ "model" : "gpt-4o", "messages" : [], "future_request" : [1,2,3] }"#;

    let response = crate::http_test::TestRequest::post("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(request_body.as_slice())
        .send(state.clone())
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

    let response = crate::http_test::TestRequest::post("/v1/chat/completions")
        .json(&serde_json::json!({"model": "gpt-4o", "messages": []}))
        .send(state.clone())
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
    let mut source = HeaderMap::new();
    source.insert("connection", "x-hop".parse().unwrap());
    source.insert("x-hop", "private".parse().unwrap());
    source.insert("transfer-encoding", "chunked".parse().unwrap());
    source.insert("content-length", "42".parse().unwrap());
    source.insert("x-request-id", "req-1".parse().unwrap());

    let mut unary = HeaderMap::new();
    copy_end_to_end_headers(&source, &mut unary, false);
    assert_eq!(unary.get("content-length").unwrap(), "42");
    assert_eq!(unary.get("x-request-id").unwrap(), "req-1");
    assert!(!unary.contains_key("connection"));
    assert!(!unary.contains_key("x-hop"));
    assert!(!unary.contains_key("transfer-encoding"));

    let mut streaming = HeaderMap::new();
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

    let response = crate::http_test::TestRequest::post("/v1/chat/completions")
        .json(&serde_json::json!({
            "model": "does-not-exist",
            "messages": [{ "role": "user", "content": "hi" }]
        }))
        .send(state.clone())
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

    let response = crate::http_test::TestRequest::post("/v1/messages")
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "max_tokens": 16,
            "messages": [{ "role": "user", "content": "hi" }]
        }))
        .send(state.clone())
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

    let response = crate::http_test::TestRequest::post("/v1/messages")
        .json(&serde_json::json!({
            "model": "nope",
            "max_tokens": 16,
            "messages": [{ "role": "user", "content": "hi" }]
        }))
        .send(state.clone())
        .await;

    let body: Value = serde_json::from_slice(response.body()).unwrap();
    assert_eq!(body["type"], "error");
    assert!(body["error"]["type"].is_string());
}

#[tokio::test]
async fn malformed_json_uses_the_inbound_protocol_error_envelope() {
    let state = state_with(vec![]).await;

    let chat = crate::http_test::TestRequest::post("/v1/chat/completions")
        .header("content-type", "application/json")
        .body("{")
        .send(state.clone())
        .await;
    assert_eq!(chat.status(), 400);
    let chat_body: Value = serde_json::from_slice(chat.body()).unwrap();
    assert!(chat_body["error"]["message"].is_string());
    assert!(chat_body["error"]["type"].is_string());

    let messages = crate::http_test::TestRequest::post("/v1/messages")
        .header("content-type", "application/json")
        .body("{")
        .send(state.clone())
        .await;
    assert_eq!(messages.status(), 400);
    let messages_body: Value = serde_json::from_slice(messages.body()).unwrap();
    assert_eq!(messages_body["type"], "error");
    assert!(messages_body["error"]["type"].is_string());
}

#[tokio::test]
async fn wrong_method_uses_the_protocol_envelope_and_405() {
    let state = state_with(vec![]).await;
    let response = crate::http_test::TestRequest::get("/v1/responses")
        .send(state.clone())
        .await;

    assert_eq!(response.status(), 405);
    assert_eq!(response.headers()["allow"], "POST");
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

    let response = crate::http_test::TestRequest::post("/v1/messages")
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "max_tokens": 32,
            "messages": [{ "role": "user", "content": "hi" }]
        }))
        .send(state.clone())
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

    let response =
        crate::http_test::TestRequest::post("/v1beta/models/gemini-2.5-pro:generateContent")
            .json(&serde_json::json!({
                "contents": [{ "role": "user", "parts": [{ "text": "hi" }] }]
            }))
            .send(state.clone())
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

    crate::http_test::TestRequest::post("/v1/chat/completions")
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "messages": [{ "role": "user", "content": "hi" }]
        }))
        .send(state.clone())
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
    let response = crate::http_test::TestRequest::post("/v1/chat/completions")
        .json(&serde_json::json!({"model": "gpt-4o", "messages": []}))
        .send(state.clone())
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
    let failure = crate::http_test::TestRequest::post("/v1/chat/completions")
        .json(&serde_json::json!({"model": "ghost", "messages": []}))
        .send(state.clone())
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

    crate::http_test::TestRequest::post("/v1/chat/completions")
        .json(&serde_json::json!({
            "model": "ghost",
            "messages": [{ "role": "user", "content": "hi" }]
        }))
        .send(state.clone())
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

    let response = crate::http_test::TestRequest::get("/v1/models")
        .send(state.clone())
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

    let response = crate::http_test::TestRequest::post("/v1/chat/completions")
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "stream": true,
            "messages": [{ "role": "user", "content": "hi" }]
        }))
        .send(state.clone())
        .await;

    assert_eq!(response.status(), 200);
    let ctypes: Vec<_> = response.headers().get_all("content-type").iter().collect();
    assert_eq!(ctypes.len(), 1, "exactly one content-type: {ctypes:?}");
    assert!(
        ctypes[0].to_str().unwrap().starts_with("text/event-stream"),
        "streaming must not degrade to a JSON response, got {ctypes:?}"
    );
}
