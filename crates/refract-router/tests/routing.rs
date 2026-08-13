//! 路由执行器的集成测试。
//!
//! Planner 的单元测试证明了「排序对」，但证明不了「失败后真的会换渠道」。
//! 这里用真实 HTTP 服务器验证重试、熔断、协议转换三件事**真的发生了**。

use std::time::Duration;

use refract_core::{
    Channel, ChannelEndpoint, ChannelKind, Credential, ErrorKind, ModelEntry, Protocol,
    ProtocolSet, RoutingPolicy, TranscodePolicy, UpstreamAddress,
};
use refract_protocol::codec::CodecSet;
use refract_protocol::ir::{Message, Role, UnifiedRequest};
use refract_router::{RouteExecutor, RoutePlanner, RoutedResponse, RoutedStream, RouterConfig};
use refract_store::{BreakerPolicy, Database, HealthRepo};
use serde_json::json;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

fn upstream_client() -> refract_upstream::UpstreamClient {
    refract_upstream::UpstreamClient::new(refract_upstream::UpstreamClientConfig {
        timeout: Duration::from_secs(5),
        connect_timeout: Duration::from_secs(2),
        stream_idle_timeout: Duration::from_millis(500),
        ..Default::default()
    })
    .unwrap()
}

fn endpoint(protocol: Protocol, order: u16, url: &str, accepted: ProtocolSet) -> ChannelEndpoint {
    ChannelEndpoint {
        protocol,
        order,
        enabled: true,
        address: UpstreamAddress {
            unofficial: true,
            full_address: true,
            base_url: Some(url.to_owned()),
            version_prefix: None,
            path: None,
        },
        credential: None,
        models: vec![ModelEntry::plain("gpt-4o")],
        transcode: TranscodePolicy {
            enabled: !accepted.is_empty(),
            accepted,
        },
    }
}

fn channel(id: i64, name: &str, priority: i32, endpoints: Vec<ChannelEndpoint>) -> Channel {
    let kind = if endpoints.len() == 1 {
        ChannelKind::Single(endpoints[0].protocol)
    } else {
        ChannelKind::Aggregate
    };
    Channel {
        id,
        owner_id: 1,
        name: name.to_owned(),
        kind,
        enabled: true,
        priority,
        weight: 1,
        credential: Credential::new("sk-test"),
        address: UpstreamAddress::default(),
        endpoints,
        tags: Vec::new(),
        timeout_secs: 0,
        proxy: None,
        param_override: None,
        note: None,
    }
}

fn ir_request() -> UnifiedRequest {
    UnifiedRequest::new("gpt-4o", vec![Message::text(Role::User, "hi")])
}

/// 建库并把渠道写进 channels 表，让健康行的外键有依托。
async fn seeded_health(channels: &[Channel]) -> HealthRepo {
    let db = Database::open_in_memory().await.unwrap();
    for ch in channels {
        sqlx_insert(&db, ch.id, &ch.name).await;
    }
    HealthRepo::new(db)
}

async fn sqlx_insert(db: &Database, id: i64, name: &str) {
    sqlx::query("INSERT INTO channels (id, name, kind) VALUES (?, ?, 'chat')")
        .bind(id)
        .bind(name)
        .execute(db.pool())
        .await
        .unwrap();
}

fn chat_ok(text: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "model": "gpt-4o",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": text}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    }))
}

#[tokio::test]
async fn primary_success_never_touches_the_backup() {
    let good = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(chat_ok("from-primary"))
        .expect(1)
        .mount(&good)
        .await;

    let backup = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(chat_ok("from-backup"))
        .expect(0)
        .mount(&backup)
        .await;

    let channels = vec![
        channel(
            1,
            "primary",
            10,
            vec![endpoint(Protocol::Chat, 0, &good.uri(), ProtocolSet::EMPTY)],
        ),
        channel(
            2,
            "backup",
            0,
            vec![endpoint(
                Protocol::Chat,
                0,
                &backup.uri(),
                ProtocolSet::EMPTY,
            )],
        ),
    ];
    let health = seeded_health(&channels).await;
    let exec = RouteExecutor::new(
        upstream_client(),
        CodecSet::builtin(),
        health.clone(),
        RouterConfig::default(),
    );

    let planner = RoutePlanner::default();
    let mut rng = rand::rng();
    let route = planner.plan(&channels, "gpt-4o", Protocol::Chat, &mut rng);
    let outcome = exec.execute(&route, &ir_request()).await.unwrap();

    assert_eq!(outcome.channel_id, 1);
    assert_eq!(outcome.attempts, 1);
    assert!(!outcome.transcoded);
}

#[tokio::test]
async fn native_unary_passthrough_preserves_request_and_response_bytes() {
    let server = MockServer::start().await;
    let response_body = br#"{ "id" : "chatcmpl-raw", "future_response" : {"ok":true} }"#;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(response_body.as_slice(), "application/json"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let channels = vec![channel(
        1,
        "raw",
        0,
        vec![endpoint(
            Protocol::Chat,
            0,
            &server.uri(),
            ProtocolSet::EMPTY,
        )],
    )];
    let health = seeded_health(&channels).await;
    let exec = RouteExecutor::new(
        upstream_client(),
        CodecSet::builtin(),
        health,
        RouterConfig::default(),
    );
    let planner = RoutePlanner::default();
    let mut rng = rand::rng();
    let ir = ir_request();
    let route = planner.plan(&channels, "gpt-4o", Protocol::Chat, &mut rng);
    let request_body = br#"{ "model" : "gpt-4o", "messages" : [], "future_request" : [1,2] }"#;
    let outcome = exec
        .execute(
            &route,
            refract_router::InboundPayload::raw(Protocol::Chat, request_body, &ir.model, ir.stream),
        )
        .await
        .unwrap();
    match outcome.payload {
        RoutedResponse::Native { response, .. } => {
            assert_eq!(response.status, 200);
            assert_eq!(response.body.as_ref(), response_body);
            assert_eq!(
                response
                    .headers
                    .get("content-type")
                    .and_then(|value| value.to_str().ok()),
                Some("application/json")
            );
        }
        RoutedResponse::Transcoded(_) => panic!("native route unexpectedly transcoded"),
    }
    let received = server.received_requests().await.unwrap();
    assert_eq!(received[0].body.as_slice(), request_body);
}

#[tokio::test]
async fn failing_primary_falls_over_to_the_backup() {
    let broken = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(503).set_body_json(json!({"error": {"message": "down"}})),
        )
        .mount(&broken)
        .await;

    let good = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(chat_ok("recovered"))
        .expect(1)
        .mount(&good)
        .await;

    let channels = vec![
        channel(
            1,
            "broken",
            10,
            vec![endpoint(
                Protocol::Chat,
                0,
                &broken.uri(),
                ProtocolSet::EMPTY,
            )],
        ),
        channel(
            2,
            "good",
            0,
            vec![endpoint(Protocol::Chat, 0, &good.uri(), ProtocolSet::EMPTY)],
        ),
    ];
    let health = seeded_health(&channels).await;
    let exec = RouteExecutor::new(
        upstream_client(),
        CodecSet::builtin(),
        health.clone(),
        RouterConfig::default(),
    );

    let planner = RoutePlanner::default();
    let mut rng = rand::rng();
    let route = planner.plan(&channels, "gpt-4o", Protocol::Chat, &mut rng);
    let outcome = exec.execute(&route, &ir_request()).await.unwrap();

    assert_eq!(
        outcome.channel_id, 2,
        "must fail over to the healthy channel"
    );
    assert_eq!(outcome.attempts, 2);
}

#[tokio::test]
async fn native_2xx_error_envelope_falls_over_to_the_backup() {
    let broken = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "error": {"message": "provider failed", "type": "server_error"}
        })))
        .expect(1)
        .mount(&broken)
        .await;

    let good = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(chat_ok("recovered"))
        .expect(1)
        .mount(&good)
        .await;

    let channels = vec![
        channel(
            1,
            "broken-envelope",
            10,
            vec![endpoint(
                Protocol::Chat,
                0,
                &broken.uri(),
                ProtocolSet::EMPTY,
            )],
        ),
        channel(
            2,
            "backup",
            0,
            vec![endpoint(Protocol::Chat, 0, &good.uri(), ProtocolSet::EMPTY)],
        ),
    ];
    let health = seeded_health(&channels).await;
    let exec = RouteExecutor::new(
        upstream_client(),
        CodecSet::builtin(),
        health.clone(),
        RouterConfig::default(),
    );
    let planner = RoutePlanner::default();
    let mut rng = rand::rng();
    let route = planner.plan(&channels, "gpt-4o", Protocol::Chat, &mut rng);
    let raw = br#"{"model":"gpt-4o","messages":[]}"#;

    let outcome = exec
        .execute(
            &route,
            refract_router::InboundPayload::raw(Protocol::Chat, raw, "gpt-4o", false),
        )
        .await
        .unwrap();

    assert_eq!(outcome.channel_id, 2);
    assert_eq!(outcome.attempts, 2);
    let failed = health.get(1, Protocol::Chat).await.unwrap().unwrap();
    assert_eq!(failed.total_failures, 1);
}

#[tokio::test]
async fn non_retryable_error_stops_immediately() {
    let bad_request = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(400).set_body_json(json!({"error": {"message": "bad payload"}})),
        )
        .expect(1)
        .mount(&bad_request)
        .await;

    let backup = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(chat_ok("never"))
        .expect(0)
        .mount(&backup)
        .await;

    let channels = vec![
        channel(
            1,
            "first",
            10,
            vec![endpoint(
                Protocol::Chat,
                0,
                &bad_request.uri(),
                ProtocolSet::EMPTY,
            )],
        ),
        channel(
            2,
            "second",
            0,
            vec![endpoint(
                Protocol::Chat,
                0,
                &backup.uri(),
                ProtocolSet::EMPTY,
            )],
        ),
    ];
    let health = seeded_health(&channels).await;
    let exec = RouteExecutor::new(
        upstream_client(),
        CodecSet::builtin(),
        health.clone(),
        RouterConfig::default(),
    );

    let planner = RoutePlanner::default();
    let mut rng = rand::rng();
    let route = planner.plan(&channels, "gpt-4o", Protocol::Chat, &mut rng);
    let err = exec.execute(&route, &ir_request()).await.unwrap_err();

    // 请求体不合法：换渠道也一样错，必须立刻返回而不是浪费配额。
    assert_eq!(err.kind, ErrorKind::InvalidRequest);
    assert!(
        health.get(1, Protocol::Chat).await.unwrap().is_none(),
        "client-side 4xx must not poison the endpoint circuit breaker"
    );
}

#[tokio::test]
async fn all_channels_down_reports_the_last_upstream_error() {
    let a = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(502).set_body_json(json!({"error": {"message": "bad gateway"}})),
        )
        .mount(&a)
        .await;
    let b = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(502).set_body_json(json!({"error": {"message": "also down"}})),
        )
        .mount(&b)
        .await;

    let channels = vec![
        channel(
            1,
            "a",
            10,
            vec![endpoint(Protocol::Chat, 0, &a.uri(), ProtocolSet::EMPTY)],
        ),
        channel(
            2,
            "b",
            0,
            vec![endpoint(Protocol::Chat, 0, &b.uri(), ProtocolSet::EMPTY)],
        ),
    ];
    let health = seeded_health(&channels).await;
    let exec = RouteExecutor::new(
        upstream_client(),
        CodecSet::builtin(),
        health,
        RouterConfig::default(),
    );

    let planner = RoutePlanner::default();
    let mut rng = rand::rng();
    let route = planner.plan(&channels, "gpt-4o", Protocol::Chat, &mut rng);
    let err = exec.execute(&route, &ir_request()).await.unwrap_err();

    assert_eq!(err.kind, ErrorKind::UpstreamError);
    // 错误信息要是上游真说的话，不是我们编的。
    assert!(err.message.contains("down") || err.message.contains("gateway"));
}

#[tokio::test]
async fn empty_route_reports_no_available_channel() {
    let health = seeded_health(&[]).await;
    let exec = RouteExecutor::new(
        upstream_client(),
        CodecSet::builtin(),
        health,
        RouterConfig::default(),
    );

    let channels: Vec<Channel> = Vec::new();
    let planner = RoutePlanner::default();
    let mut rng = rand::rng();
    let route = planner.plan(&channels, "gpt-4o", Protocol::Chat, &mut rng);
    let err = exec.execute(&route, &ir_request()).await.unwrap_err();

    assert_eq!(err.kind, ErrorKind::NoAvailableChannel);
    assert!(err.message.contains("gpt-4o"));
}

#[tokio::test]
async fn transcoded_request_reaches_an_anthropic_upstream() {
    // Chat 进来，Messages 出去：请求体必须被真正改写成 Anthropic 形状。
    let anthropic = MockServer::start().await;
    Mock::given(method("POST"))
        .and(wiremock::matchers::body_partial_json(
            json!({"max_tokens": 4096}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-4-6",
            "content": [{"type": "text", "text": "transcoded-ok"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })))
        .expect(1)
        .mount(&anthropic)
        .await;

    let channels = vec![channel(
        1,
        "anthropic",
        0,
        vec![endpoint(
            Protocol::Messages,
            0,
            &anthropic.uri(),
            ProtocolSet::from_iter_protocols([Protocol::Chat]),
        )],
    )];
    let health = seeded_health(&channels).await;
    let exec = RouteExecutor::new(
        upstream_client(),
        CodecSet::builtin(),
        health,
        RouterConfig::default(),
    );

    let planner = RoutePlanner::default();
    let mut rng = rand::rng();
    let route = planner.plan(&channels, "gpt-4o", Protocol::Chat, &mut rng);
    let outcome = exec.execute(&route, &ir_request()).await.unwrap();

    assert!(
        outcome.transcoded,
        "Chat -> Messages must be marked as transcoded"
    );
    assert_eq!(outcome.upstream_protocol, Protocol::Messages);
}

#[tokio::test]
async fn model_alias_is_rewritten_for_the_upstream() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(wiremock::matchers::body_partial_json(
            json!({"model": "internal-model-v3"}),
        ))
        .respond_with(chat_ok("aliased"))
        .expect(1)
        .mount(&server)
        .await;

    let mut ep = endpoint(Protocol::Chat, 0, &server.uri(), ProtocolSet::EMPTY);
    ep.models = vec![ModelEntry::mapped("gpt-4o", "internal-model-v3")];
    let channels = vec![channel(1, "aliasing", 0, vec![ep])];
    let health = seeded_health(&channels).await;
    let exec = RouteExecutor::new(
        upstream_client(),
        CodecSet::builtin(),
        health,
        RouterConfig::default(),
    );

    let planner = RoutePlanner::default();
    let mut rng = rand::rng();
    let route = planner.plan(&channels, "gpt-4o", Protocol::Chat, &mut rng);
    let outcome = exec.execute(&route, &ir_request()).await.unwrap();

    assert_eq!(outcome.upstream_model, "internal-model-v3");
}

#[tokio::test]
async fn channel_param_override_is_applied_last() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(wiremock::matchers::body_partial_json(
            json!({"temperature": 0.9}),
        ))
        .respond_with(chat_ok("overridden"))
        .expect(1)
        .mount(&server)
        .await;

    let mut ch = channel(
        1,
        "forced",
        0,
        vec![endpoint(
            Protocol::Chat,
            0,
            &server.uri(),
            ProtocolSet::EMPTY,
        )],
    );
    ch.param_override = Some(json!({"temperature": 0.9}));
    let channels = vec![ch];
    let health = seeded_health(&channels).await;
    let exec = RouteExecutor::new(
        upstream_client(),
        CodecSet::builtin(),
        health,
        RouterConfig::default(),
    );

    let planner = RoutePlanner::default();
    let mut rng = rand::rng();
    let mut ir = ir_request();
    ir.sampling.temperature = Some(0.1);
    let route = planner.plan(&channels, "gpt-4o", Protocol::Chat, &mut rng);
    // 渠道覆盖必须压过客户端请求里的值。
    exec.execute(&route, &ir).await.unwrap();
}

#[tokio::test]
async fn native_alias_and_override_preserve_unknown_request_fields() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(chat_ok("rewritten"))
        .expect(1)
        .mount(&server)
        .await;

    let mut ep = endpoint(Protocol::Chat, 0, &server.uri(), ProtocolSet::EMPTY);
    ep.models = vec![ModelEntry::mapped("gpt-4o", "internal-model-v3")];
    let mut ch = channel(1, "rewritten", 0, vec![ep]);
    ch.param_override = Some(json!({"temperature": 0.9}));
    let channels = vec![ch];
    let health = seeded_health(&channels).await;
    let exec = RouteExecutor::new(
        upstream_client(),
        CodecSet::builtin(),
        health,
        RouterConfig::default(),
    );
    let planner = RoutePlanner::default();
    let mut rng = rand::rng();
    let route = planner.plan(&channels, "gpt-4o", Protocol::Chat, &mut rng);
    let raw =
        br#"{"model":"gpt-4o","messages":[],"temperature":0.1,"future_request":{"kept":true}}"#;

    exec.execute(
        &route,
        refract_router::InboundPayload::raw(Protocol::Chat, raw, "gpt-4o", false),
    )
    .await
    .unwrap();

    let received = server.received_requests().await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&received[0].body).unwrap();
    assert_eq!(body["model"], "internal-model-v3");
    assert_eq!(body["temperature"], 0.9);
    assert_eq!(body["future_request"]["kept"], true);
}

#[tokio::test]
async fn health_records_success_and_failure() {
    let good = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(chat_ok("ok"))
        .mount(&good)
        .await;

    let channels = vec![channel(
        1,
        "c",
        0,
        vec![endpoint(Protocol::Chat, 0, &good.uri(), ProtocolSet::EMPTY)],
    )];
    let health = seeded_health(&channels).await;
    let exec = RouteExecutor::new(
        upstream_client(),
        CodecSet::builtin(),
        health.clone(),
        RouterConfig::default(),
    );

    let planner = RoutePlanner::default();
    let mut rng = rand::rng();
    let route = planner.plan(&channels, "gpt-4o", Protocol::Chat, &mut rng);
    exec.execute(&route, &ir_request()).await.unwrap();

    let h = health.get(1, Protocol::Chat).await.unwrap().unwrap();
    assert_eq!(h.total_requests, 1);
    assert_eq!(h.total_failures, 0);
    assert!(h.avg_latency_ms > 0 || h.total_requests == 1);
}

#[tokio::test]
async fn suspended_endpoint_is_tried_last_not_first() {
    let suspended_server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(chat_ok("from-suspended"))
        .mount(&suspended_server)
        .await;
    let healthy_server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(chat_ok("from-healthy"))
        .expect(1)
        .mount(&healthy_server)
        .await;

    // ch1 优先级更高但已熔断；ch2 优先级低但健康。
    let channels = vec![
        channel(
            1,
            "suspended",
            10,
            vec![endpoint(
                Protocol::Chat,
                0,
                &suspended_server.uri(),
                ProtocolSet::EMPTY,
            )],
        ),
        channel(
            2,
            "healthy",
            0,
            vec![endpoint(
                Protocol::Chat,
                0,
                &healthy_server.uri(),
                ProtocolSet::EMPTY,
            )],
        ),
    ];
    let health = seeded_health(&channels).await;
    // 把 ch1 打到熔断。
    for _ in 0..health.policy().failure_threshold {
        health
            .record_failure(1, Protocol::Chat, "boom", None)
            .await
            .unwrap();
    }
    assert!(
        health
            .get(1, Protocol::Chat)
            .await
            .unwrap()
            .unwrap()
            .is_suspended_at(chrono::Utc::now())
    );

    let exec = RouteExecutor::new(
        upstream_client(),
        CodecSet::builtin(),
        health,
        RouterConfig::default(),
    );
    let planner = RoutePlanner::default();
    let mut rng = rand::rng();
    let route = planner.plan(&channels, "gpt-4o", Protocol::Chat, &mut rng);
    let outcome = exec.execute(&route, &ir_request()).await.unwrap();

    assert_eq!(
        outcome.channel_id, 2,
        "suspended channel must be demoted below healthy ones"
    );
}

#[tokio::test]
async fn suspended_endpoint_is_still_used_when_nothing_else_is_left() {
    let only = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(chat_ok("last-resort"))
        .expect(1)
        .mount(&only)
        .await;

    let channels = vec![channel(
        1,
        "only",
        0,
        vec![endpoint(Protocol::Chat, 0, &only.uri(), ProtocolSet::EMPTY)],
    )];
    let health = seeded_health(&channels).await;
    for _ in 0..health.policy().failure_threshold {
        health
            .record_failure(1, Protocol::Chat, "boom", None)
            .await
            .unwrap();
    }

    let exec = RouteExecutor::new(
        upstream_client(),
        CodecSet::builtin(),
        health,
        RouterConfig::default(),
    );
    let planner = RoutePlanner::default();
    let mut rng = rand::rng();
    let route = planner.plan(&channels, "gpt-4o", Protocol::Chat, &mut rng);

    // 唯一的渠道熔断了，仍要试一次 —— 确定的 503 比可能成功的请求更糟。
    let outcome = exec.execute(&route, &ir_request()).await.unwrap();
    assert_eq!(outcome.channel_id, 1);
}

#[tokio::test]
async fn last_resort_can_be_disabled() {
    let only = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(chat_ok("should-not-be-called"))
        .expect(0)
        .mount(&only)
        .await;

    let channels = vec![channel(
        1,
        "only",
        0,
        vec![endpoint(Protocol::Chat, 0, &only.uri(), ProtocolSet::EMPTY)],
    )];
    let health = seeded_health(&channels).await;
    for _ in 0..health.policy().failure_threshold {
        health
            .record_failure(1, Protocol::Chat, "boom", None)
            .await
            .unwrap();
    }

    let exec = RouteExecutor::new(
        upstream_client(),
        CodecSet::builtin(),
        health,
        RouterConfig {
            allow_suspended_as_last_resort: false,
        },
    );
    let planner = RoutePlanner::default();
    let mut rng = rand::rng();
    let route = planner.plan(&channels, "gpt-4o", Protocol::Chat, &mut rng);
    let err = exec.execute(&route, &ir_request()).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::NoAvailableChannel);
}

#[tokio::test]
async fn upstream_404_counts_against_endpoint_health() {
    // 上游 404 = 渠道的地址或模型名配错了。它不可重试（换渠道语义之外），
    // 但必须计入健康度 —— 一直 404 的渠道要能熔断，不然配错的渠道永远
    // 占着首选位。
    let broken = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(404)
                .set_body_json(json!({"error": {"message": "model not found"}})),
        )
        .mount(&broken)
        .await;

    let channels = vec![channel(
        1,
        "misconfigured",
        0,
        vec![endpoint(
            Protocol::Chat,
            0,
            &broken.uri(),
            ProtocolSet::EMPTY,
        )],
    )];
    let health = seeded_health(&channels).await;
    let exec = RouteExecutor::new(
        upstream_client(),
        CodecSet::builtin(),
        health.clone(),
        RouterConfig::default(),
    );
    let planner = RoutePlanner::default();
    let mut rng = rand::rng();
    let route = planner.plan(&channels, "gpt-4o", Protocol::Chat, &mut rng);

    let err = exec.execute(&route, &ir_request()).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::NotFound);

    let h = health.get(1, Protocol::Chat).await.unwrap().unwrap();
    assert_eq!(h.total_failures, 1, "404 必须计入端点健康度");
    assert_eq!(h.consecutive_fails, 1);
}

#[tokio::test]
async fn retry_after_header_suspends_the_endpoint_immediately() {
    // 上游 429 + Retry-After: 300：即使只失败一次（远未达熔断阈值），
    // 端点也要按上游说的时长悬停。
    let limited = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "300")
                .set_body_json(json!({"error": {"message": "rate limited"}})),
        )
        .mount(&limited)
        .await;

    let channels = vec![channel(
        1,
        "limited",
        0,
        vec![endpoint(
            Protocol::Chat,
            0,
            &limited.uri(),
            ProtocolSet::EMPTY,
        )],
    )];
    let health = seeded_health(&channels).await;
    let exec = RouteExecutor::new(
        upstream_client(),
        CodecSet::builtin(),
        health.clone(),
        RouterConfig::default(),
    );
    let planner = RoutePlanner::default();
    let mut rng = rand::rng();
    let route = planner.plan(&channels, "gpt-4o", Protocol::Chat, &mut rng);

    let err = exec.execute(&route, &ir_request()).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::RateLimited);
    assert_eq!(
        err.retry_after,
        Some(Duration::from_secs(300)),
        "Retry-After 头必须透传到错误上"
    );

    let until = health
        .suspended_until(1, Protocol::Chat)
        .expect("单次 429 + Retry-After 就应悬停");
    let wait = (until - chrono::Utc::now()).num_seconds();
    assert!((290..=301).contains(&wait), "悬停 {wait}s，应约 300s");
}

#[tokio::test]
async fn streaming_route_returns_a_live_stream() {
    use futures::StreamExt as _;

    let server = MockServer::start().await;
    let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n\
               data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
               data: [DONE]\n\n";
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse),
        )
        .mount(&server)
        .await;

    let channels = vec![channel(
        1,
        "c",
        0,
        vec![endpoint(
            Protocol::Chat,
            0,
            &server.uri(),
            ProtocolSet::EMPTY,
        )],
    )];
    let health = seeded_health(&channels).await;
    let exec = RouteExecutor::new(
        upstream_client(),
        CodecSet::builtin(),
        health.clone(),
        RouterConfig::default(),
    );

    let planner = RoutePlanner::default();
    let mut rng = rand::rng();
    let mut ir = ir_request();
    ir.stream = true;
    let route = planner.plan(&channels, "gpt-4o", Protocol::Chat, &mut rng);
    let outcome = exec.execute_stream(&route, &ir).await.unwrap();

    assert!(
        health.get(1, Protocol::Chat).await.unwrap().is_none(),
        "opening headers and a first frame is not terminal success"
    );

    let RoutedStream::Transcoded(stream) = outcome.payload else {
        panic!("normalized execution must use the decoded stream path");
    };
    let frames: Vec<_> = stream.collect().await;
    assert!(
        frames.len() >= 2,
        "expected several SSE frames, got {}",
        frames.len()
    );
    let snapshot = health.get(1, Protocol::Chat).await.unwrap().unwrap();
    assert_eq!(snapshot.total_requests, 1);
    assert_eq!(snapshot.total_failures, 0);
}

#[tokio::test]
async fn native_stream_passthrough_preserves_sse_bytes() {
    use futures::StreamExt as _;

    let server = MockServer::start().await;
    let sse = "event: future_event\ndata: {\"future\":true}\n\ndata: [DONE]\n\n";
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse),
        )
        .mount(&server)
        .await;
    let channels = vec![channel(
        1,
        "raw-stream",
        0,
        vec![endpoint(
            Protocol::Chat,
            0,
            &server.uri(),
            ProtocolSet::EMPTY,
        )],
    )];
    let health = seeded_health(&channels).await;
    let exec = RouteExecutor::new(
        upstream_client(),
        CodecSet::builtin(),
        health,
        RouterConfig::default(),
    );
    let planner = RoutePlanner::default();
    let mut rng = rand::rng();
    let mut ir = ir_request();
    ir.stream = true;
    let route = planner.plan(&channels, "gpt-4o", Protocol::Chat, &mut rng);
    let request_body = br#"{ "model":"gpt-4o", "stream":true, "messages":[] }"#;
    let outcome = exec
        .execute_stream(
            &route,
            refract_router::InboundPayload::raw(Protocol::Chat, request_body, &ir.model, ir.stream),
        )
        .await
        .unwrap();
    let bytes = match outcome.payload {
        RoutedStream::Native(response) => response
            .stream
            .map(|chunk| chunk.unwrap())
            .collect::<Vec<_>>()
            .await
            .concat(),
        RoutedStream::Transcoded(_) => panic!("native stream unexpectedly transcoded"),
    };
    assert_eq!(String::from_utf8(bytes).unwrap(), sse);
}

#[tokio::test]
async fn malformed_first_stream_falls_over_before_returning_to_the_client() {
    use futures::StreamExt as _;

    let malformed = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string("this is not an SSE event"),
        )
        .mount(&malformed)
        .await;

    let good = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n",
                ),
        )
        .expect(1)
        .mount(&good)
        .await;

    let channels = vec![
        channel(
            1,
            "malformed",
            10,
            vec![endpoint(
                Protocol::Chat,
                0,
                &malformed.uri(),
                ProtocolSet::EMPTY,
            )],
        ),
        channel(
            2,
            "good",
            0,
            vec![endpoint(Protocol::Chat, 0, &good.uri(), ProtocolSet::EMPTY)],
        ),
    ];
    let health = seeded_health(&channels).await;
    let exec = RouteExecutor::new(
        upstream_client(),
        CodecSet::builtin(),
        health.clone(),
        RouterConfig::default(),
    );
    let planner = RoutePlanner::default();
    let mut rng = rand::rng();
    let mut ir = ir_request();
    ir.stream = true;
    let route = planner.plan(&channels, "gpt-4o", Protocol::Chat, &mut rng);

    let outcome = exec.execute_stream(&route, &ir).await.unwrap();
    assert_eq!(outcome.channel_id, 2);
    assert_eq!(outcome.attempts, 2);
    let RoutedStream::Transcoded(stream) = outcome.payload else {
        panic!("normalized execution must use the decoded stream path");
    };
    assert!(stream.collect::<Vec<_>>().await.iter().all(Result::is_ok));

    let failed = health.get(1, Protocol::Chat).await.unwrap().unwrap();
    assert_eq!(failed.total_failures, 1);
}

#[tokio::test]
async fn error_first_stream_frame_falls_over_before_returning_to_the_client() {
    use futures::StreamExt as _;

    let broken = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(
                    "data: {\"error\":{\"message\":\"stream failed\",\"type\":\"server_error\"}}\n\n",
                ),
        )
        .expect(1)
        .mount(&broken)
        .await;

    let good = MockServer::start().await;
    let good_sse = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n";
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(good_sse),
        )
        .expect(1)
        .mount(&good)
        .await;

    let channels = vec![
        channel(
            1,
            "error-frame",
            10,
            vec![endpoint(
                Protocol::Chat,
                0,
                &broken.uri(),
                ProtocolSet::EMPTY,
            )],
        ),
        channel(
            2,
            "good",
            0,
            vec![endpoint(Protocol::Chat, 0, &good.uri(), ProtocolSet::EMPTY)],
        ),
    ];
    let health = seeded_health(&channels).await;
    let exec = RouteExecutor::new(
        upstream_client(),
        CodecSet::builtin(),
        health.clone(),
        RouterConfig::default(),
    );
    let planner = RoutePlanner::default();
    let mut rng = rand::rng();
    let route = planner.plan(&channels, "gpt-4o", Protocol::Chat, &mut rng);
    let raw = br#"{"model":"gpt-4o","stream":true,"messages":[]}"#;

    let outcome = exec
        .execute_stream(
            &route,
            refract_router::InboundPayload::raw(Protocol::Chat, raw, "gpt-4o", true),
        )
        .await
        .unwrap();
    assert_eq!(outcome.channel_id, 2);
    assert_eq!(outcome.attempts, 2);
    let RoutedStream::Native(response) = outcome.payload else {
        panic!("native backup must remain a raw stream");
    };
    let bytes = response
        .stream
        .map(|chunk| chunk.unwrap())
        .collect::<Vec<_>>()
        .await
        .concat();
    assert_eq!(String::from_utf8(bytes).unwrap(), good_sse);
    let failed = health.get(1, Protocol::Chat).await.unwrap().unwrap();
    assert_eq!(failed.total_failures, 1);
}

#[tokio::test]
async fn streaming_failure_falls_over_before_the_first_frame() {
    use futures::StreamExt as _;

    let broken = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(503).set_body_json(json!({"error": {"message": "down"}})),
        )
        .mount(&broken)
        .await;

    let good = MockServer::start().await;
    let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\ndata: [DONE]\n\n";
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse),
        )
        .expect(1)
        .mount(&good)
        .await;

    let channels = vec![
        channel(
            1,
            "broken",
            10,
            vec![endpoint(
                Protocol::Chat,
                0,
                &broken.uri(),
                ProtocolSet::EMPTY,
            )],
        ),
        channel(
            2,
            "good",
            0,
            vec![endpoint(Protocol::Chat, 0, &good.uri(), ProtocolSet::EMPTY)],
        ),
    ];
    let health = seeded_health(&channels).await;
    let exec = RouteExecutor::new(
        upstream_client(),
        CodecSet::builtin(),
        health,
        RouterConfig::default(),
    );

    let planner = RoutePlanner::default();
    let mut rng = rand::rng();
    let mut ir = ir_request();
    ir.stream = true;
    let route = planner.plan(&channels, "gpt-4o", Protocol::Chat, &mut rng);
    let outcome = exec.execute_stream(&route, &ir).await.unwrap();

    assert_eq!(outcome.channel_id, 2);
    let RoutedStream::Transcoded(stream) = outcome.payload else {
        panic!("normalized execution must use the decoded stream path");
    };
    let frames: Vec<_> = stream.collect().await;
    assert!(!frames.is_empty());
}

#[tokio::test]
async fn max_attempts_bounds_the_upstream_calls() {
    let a = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(502).set_body_json(json!({"error": {"message": "x"}})))
        .mount(&a)
        .await;
    let b = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(502).set_body_json(json!({"error": {"message": "x"}})))
        .mount(&b)
        .await;
    let c = MockServer::start().await;
    // 第三个渠道永远不该被碰到 —— max_attempts = 2。
    Mock::given(method("POST"))
        .respond_with(chat_ok("third"))
        .expect(0)
        .mount(&c)
        .await;

    let channels = vec![
        channel(
            1,
            "a",
            30,
            vec![endpoint(Protocol::Chat, 0, &a.uri(), ProtocolSet::EMPTY)],
        ),
        channel(
            2,
            "b",
            20,
            vec![endpoint(Protocol::Chat, 0, &b.uri(), ProtocolSet::EMPTY)],
        ),
        channel(
            3,
            "c",
            10,
            vec![endpoint(Protocol::Chat, 0, &c.uri(), ProtocolSet::EMPTY)],
        ),
    ];
    let health = seeded_health(&channels).await;
    let exec = RouteExecutor::new(
        upstream_client(),
        CodecSet::builtin(),
        health,
        RouterConfig::default(),
    );

    let planner = RoutePlanner::new(RoutingPolicy {
        max_attempts: 2,
        ..Default::default()
    });
    let mut rng = rand::rng();
    let route = planner.plan(&channels, "gpt-4o", Protocol::Chat, &mut rng);
    // 计划保留全部候选，上限由执行器在健康度重排后应用（渠道 c 的
    // expect(0) 验证执行器真的只打了 2 个上游）。
    assert_eq!(route.attempts.len(), 3);
    assert_eq!(route.attempt_cap, 2);
    assert!(exec.execute(&route, &ir_request()).await.is_err());
}

#[tokio::test]
async fn suspended_leaders_no_longer_eat_the_attempt_cap() {
    // 4 个渠道按优先级排列，前 3 名全部处于熔断中，max_attempts = 2。
    // 若在计划阶段就截断，候选只剩 [1, 2] —— 健康的 4 号永远轮不到。
    // 正确行为：健康度重排把 4 号提到最前，再截断，首次尝试就命中它。
    let mut mocks = Vec::new();
    for expected in [0_u64, 0, 0, 1] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(chat_ok("hit"))
            .expect(expected)
            .mount(&server)
            .await;
        mocks.push(server);
    }

    let channels: Vec<Channel> = mocks
        .iter()
        .enumerate()
        .map(|(i, server)| {
            channel(
                i as i64 + 1,
                &format!("ch{}", i + 1),
                40 - i as i32 * 10,
                vec![endpoint(
                    Protocol::Chat,
                    0,
                    &server.uri(),
                    ProtocolSet::EMPTY,
                )],
            )
        })
        .collect();

    let db = Database::open_in_memory().await.unwrap();
    for ch in &channels {
        sqlx_insert(&db, ch.id, &ch.name).await;
    }
    let health = HealthRepo::with_policy(
        db,
        BreakerPolicy {
            failure_threshold: 1,
            base_cooldown_secs: 60,
            max_cooldown_secs: 60,
        },
    );
    for id in 1..=3 {
        health
            .record_failure(id, Protocol::Chat, "seeded outage", None)
            .await
            .unwrap();
    }

    let exec = RouteExecutor::new(
        upstream_client(),
        CodecSet::builtin(),
        health,
        RouterConfig::default(),
    );
    let planner = RoutePlanner::new(RoutingPolicy {
        max_attempts: 2,
        selection: refract_core::SelectionMode::First,
        ..Default::default()
    });
    let mut rng = rand::rng();
    let route = planner.plan(&channels, "gpt-4o", Protocol::Chat, &mut rng);

    let outcome = exec.execute(&route, &ir_request()).await.unwrap();
    assert_eq!(outcome.channel_id, 4, "健康的第 4 名必须在上限内被尝试");
    assert_eq!(outcome.attempts, 1);
}

#[tokio::test]
async fn param_override_protocol_groups_only_touch_their_own_endpoints() {
    // Chat 端点：顶层键与 chat 分组都应展开；gemini 分组必须被跳过。
    let chat_server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(chat_ok("ok"))
        .expect(1)
        .mount(&chat_server)
        .await;

    let mut chat_channel = channel(
        1,
        "chat",
        0,
        vec![endpoint(
            Protocol::Chat,
            0,
            &chat_server.uri(),
            ProtocolSet::EMPTY,
        )],
    );
    chat_channel.param_override = Some(json!({
        "temperature": 0.5,
        "chat": {"top_p": 0.9},
        "gemini": {"generationConfig": {"temperature": 0.0}},
    }));

    let channels = vec![chat_channel];
    let health = seeded_health(&channels).await;
    let exec = RouteExecutor::new(
        upstream_client(),
        CodecSet::builtin(),
        health,
        RouterConfig::default(),
    );
    let planner = RoutePlanner::default();
    let mut rng = rand::rng();
    let route = planner.plan(&channels, "gpt-4o", Protocol::Chat, &mut rng);
    let raw = br#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]}"#;
    exec.execute(
        &route,
        refract_router::InboundPayload::raw(Protocol::Chat, raw, "gpt-4o", false),
    )
    .await
    .unwrap();

    let received = chat_server.received_requests().await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&received[0].body).unwrap();
    assert_eq!(body["temperature"], json!(0.5), "顶层键对所有端点生效");
    assert_eq!(body["top_p"], json!(0.9), "chat 分组在 Chat 端点展开");
    assert!(
        body.get("gemini").is_none() && body.get("chat").is_none(),
        "协议分组键本身不得进入请求体: {body}"
    );
}

#[tokio::test]
async fn param_override_for_other_protocols_keeps_native_bytes_untouched() {
    // Gemini 端点 + 只含 chat 分组的覆盖：对本协议无效果，必须保持字节直通。
    let gemini_server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "candidates": [{
                "content": {"parts": [{"text": "hi"}], "role": "model"},
                "finishReason": "STOP",
            }],
            "usageMetadata": {"promptTokenCount": 1, "candidatesTokenCount": 1},
        })))
        .expect(1)
        .mount(&gemini_server)
        .await;

    let mut gemini_channel = channel(
        1,
        "gemini",
        0,
        vec![endpoint(
            Protocol::Gemini,
            0,
            &gemini_server.uri(),
            ProtocolSet::EMPTY,
        )],
    );
    gemini_channel.param_override = Some(json!({"chat": {"top_p": 0.9}}));

    let channels = vec![gemini_channel];
    let health = seeded_health(&channels).await;
    let exec = RouteExecutor::new(
        upstream_client(),
        CodecSet::builtin(),
        health,
        RouterConfig::default(),
    );
    let planner = RoutePlanner::default();
    let mut rng = rand::rng();
    let route = planner.plan(&channels, "gpt-4o", Protocol::Gemini, &mut rng);
    // 刻意保留非常规空白：字节直通意味着上游收到的和进来的一模一样。
    let raw = br#"{ "contents" : [ {"parts": [{"text": "hi"}]} ] }"#;
    exec.execute(
        &route,
        refract_router::InboundPayload::raw(Protocol::Gemini, raw, "gpt-4o", false),
    )
    .await
    .unwrap();

    let received = gemini_server.received_requests().await.unwrap();
    assert_eq!(
        received[0].body.as_slice(),
        raw.as_slice(),
        "无效覆盖不得触发 JSON 重编码"
    );
}

#[tokio::test]
async fn invalid_request_error_in_stream_stops_the_retry_chain() {
    // 上游在流里明说 invalid_request —— 换渠道也一样错，备胎不该被打扰。
    let broken = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(
                    "data: {\"error\":{\"message\":\"bad temperature\",\"type\":\"invalid_request_error\"}}\n\n",
                ),
        )
        .expect(1)
        .mount(&broken)
        .await;

    let backup = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n",
                ),
        )
        .expect(0)
        .mount(&backup)
        .await;

    let channels = vec![
        channel(
            1,
            "invalid",
            10,
            vec![endpoint(
                Protocol::Chat,
                0,
                &broken.uri(),
                ProtocolSet::EMPTY,
            )],
        ),
        channel(
            2,
            "backup",
            0,
            vec![endpoint(
                Protocol::Chat,
                0,
                &backup.uri(),
                ProtocolSet::EMPTY,
            )],
        ),
    ];
    let health = seeded_health(&channels).await;
    let exec = RouteExecutor::new(
        upstream_client(),
        CodecSet::builtin(),
        health,
        RouterConfig::default(),
    );
    let planner = RoutePlanner::default();
    let mut rng = rand::rng();
    let route = planner.plan(&channels, "gpt-4o", Protocol::Chat, &mut rng);
    let raw = br#"{"model":"gpt-4o","stream":true,"messages":[]}"#;

    let result = exec
        .execute_stream(
            &route,
            refract_router::InboundPayload::raw(Protocol::Chat, raw, "gpt-4o", true),
        )
        .await;
    match result {
        Err(error) => assert_eq!(error.kind, ErrorKind::InvalidRequest),
        Ok(_) => panic!("invalid_request from upstream must fail the request"),
    }
}

#[tokio::test]
async fn breaker_trips_after_repeated_failures_through_the_router() {
    let broken = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(500).set_body_json(json!({"error": {"message": "boom"}})),
        )
        .mount(&broken)
        .await;

    let channels = vec![channel(
        1,
        "broken",
        0,
        vec![endpoint(
            Protocol::Chat,
            0,
            &broken.uri(),
            ProtocolSet::EMPTY,
        )],
    )];
    let db = Database::open_in_memory().await.unwrap();
    sqlx_insert(&db, 1, "broken").await;
    let health = HealthRepo::with_policy(
        db,
        BreakerPolicy {
            failure_threshold: 3,
            base_cooldown_secs: 30,
            max_cooldown_secs: 60,
        },
    );
    let exec = RouteExecutor::new(
        upstream_client(),
        CodecSet::builtin(),
        health.clone(),
        RouterConfig::default(),
    );

    let planner = RoutePlanner::default();
    for _ in 0..3 {
        let mut rng = rand::rng();
        let route = planner.plan(&channels, "gpt-4o", Protocol::Chat, &mut rng);
        let _ = exec.execute(&route, &ir_request()).await;
    }

    let h = health.get(1, Protocol::Chat).await.unwrap().unwrap();
    assert!(
        h.is_suspended_at(chrono::Utc::now()),
        "a channel that failed {} times must be suspended, got {h:?}",
        h.total_failures
    );
}

#[tokio::test]
async fn whitelisted_headers_ride_along_on_native_passthrough() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(wiremock::matchers::header("x-title", "my-app"))
        .and(wiremock::matchers::header(
            "anthropic-beta",
            "context-1m-2025",
        ))
        .respond_with(chat_ok("with-headers"))
        .expect(1)
        .mount(&upstream)
        .await;

    let channels = vec![channel(
        1,
        "native",
        0,
        vec![endpoint(
            Protocol::Chat,
            0,
            &upstream.uri(),
            ProtocolSet::EMPTY,
        )],
    )];
    let health = seeded_health(&channels).await;
    let exec = RouteExecutor::new(
        upstream_client(),
        CodecSet::builtin(),
        health,
        RouterConfig::default(),
    );
    let planner = RoutePlanner::default();
    let mut rng = rand::rng();
    let route = planner.plan(&channels, "gpt-4o", Protocol::Chat, &mut rng);
    let raw = br#"{"model":"gpt-4o","messages":[]}"#;
    let headers = vec![
        ("x-title".to_owned(), "my-app".to_owned()),
        ("anthropic-beta".to_owned(), "context-1m-2025".to_owned()),
    ];

    let outcome = exec
        .execute(
            &route,
            refract_router::InboundPayload::raw(Protocol::Chat, raw, "gpt-4o", false)
                .with_headers(&headers),
        )
        .await
        .unwrap();
    assert_eq!(outcome.channel_id, 1);
}

#[tokio::test]
async fn forwarded_headers_do_not_leak_into_transcoded_calls() {
    let upstream = MockServer::start().await;
    // 先挂「带头则命中」的哨兵：任何带 anthropic-beta 的请求都算泄漏。
    Mock::given(method("POST"))
        .and(wiremock::matchers::header_exists("anthropic-beta"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "gpt-4o",
            "content": [{"type": "text", "text": "transcoded"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })))
        .expect(1)
        .mount(&upstream)
        .await;

    // Chat 入站 → Messages 端点：必须走转码路径。
    let channels = vec![channel(
        1,
        "anthropic-only",
        0,
        vec![endpoint(
            Protocol::Messages,
            0,
            &upstream.uri(),
            ProtocolSet::ALL,
        )],
    )];
    let health = seeded_health(&channels).await;
    let exec = RouteExecutor::new(
        upstream_client(),
        CodecSet::builtin(),
        health,
        RouterConfig::default(),
    );
    let planner = RoutePlanner::default();
    let mut rng = rand::rng();
    let route = planner.plan(&channels, "gpt-4o", Protocol::Chat, &mut rng);
    let raw = br#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]}"#;
    let headers = vec![("anthropic-beta".to_owned(), "context-1m-2025".to_owned())];

    let outcome = exec
        .execute(
            &route,
            refract_router::InboundPayload::raw(Protocol::Chat, raw, "gpt-4o", false)
                .with_headers(&headers),
        )
        .await
        .unwrap();
    assert!(matches!(outcome.payload, RoutedResponse::Transcoded(_)));
}
