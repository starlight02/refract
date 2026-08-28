use crate::http_test::{TestRequest, bootstrap_state};
use crate::state::AppState;
use refract_core::{
    Channel, ChannelEndpoint, ChannelKind, ChannelVisibility, Credential, ModelEntry, Protocol,
    UpstreamAddress,
};
use refract_store::{LedgerKind, LogFilter, NewRequestLog};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PASSWORD: &str = "Passw0rd1234";

fn data(response: &crate::http_test::TestResponse) -> serde_json::Value {
    response.json()["data"].clone()
}

async fn register_verify_login(state: &AppState, email: &str) -> (i64, String) {
    let registered = TestRequest::post("/api/auth/register")
        .json(&serde_json::json!({
            "email": email,
            "password": PASSWORD,
            "display_name": email,
        }))
        .send(state.clone())
        .await;
    assert_eq!(registered.status(), 200, "{}", registered.json());
    let user_id = data(&registered)["user_id"].as_i64().unwrap();

    let codes = TestRequest::get(&format!("/api/auth/dev-codes?email={email}"))
        .send(state.clone())
        .await;
    let code = data(&codes)["code"].as_str().unwrap().to_owned();
    let verified = TestRequest::post("/api/auth/verify-email")
        .json(&serde_json::json!({ "email": email, "code": code }))
        .send(state.clone())
        .await;
    assert_eq!(verified.status(), 200, "{}", verified.json());

    let logged_in = TestRequest::post("/api/auth/login")
        .json(&serde_json::json!({ "email": email, "password": PASSWORD }))
        .send(state.clone())
        .await;
    assert_eq!(logged_in.status(), 200, "{}", logged_in.json());
    (user_id, logged_in.session_cookie().expect("session cookie"))
}

fn sample_channel(base: &str, model: &str) -> Channel {
    Channel {
        id: 0,
        owner_id: refract_core::DEFAULT_OWNER_ID,
        visibility: ChannelVisibility::Shared,
        user_id: None,
        name: format!("{model}-upstream"),
        kind: ChannelKind::Single(Protocol::Chat),
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
            models: vec![ModelEntry::plain(model)],
            ..ChannelEndpoint::new(Protocol::Chat)
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

async fn wait_ledger(
    state: &AppState,
    user_id: i64,
    expected: usize,
) -> Vec<refract_store::LedgerEntry> {
    for _ in 0..100 {
        let rows = state
            .wallet_repo()
            .ledger(user_id, 50, 0, None, None, None)
            .await
            .unwrap();
        if rows.len() >= expected {
            return rows;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("expected {expected} ledger rows");
}

#[tokio::test]
async fn topup_then_charges_exhaust_balance() {
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
            "usage": { "prompt_tokens": 500_000, "completion_tokens": 0, "total_tokens": 500_000 }
        })))
        .mount(&server)
        .await;

    let db = refract_store::Database::open_in_memory().await.unwrap();
    refract_store::ChannelRepo::new(db.clone())
        .create(&sample_channel(&server.uri(), "gpt-4o"))
        .await
        .unwrap();
    let client = refract_upstream::UpstreamClient::new(Default::default()).unwrap();
    let state = AppState::bootstrap(db, client, true).await.unwrap();
    state.set_dev_mode(true);

    state
        .settings_repo()
        .set_pricing(&[refract_store::ModelPrice {
            pattern: "gpt-4o".into(),
            input_per_m: 1.0,
            output_per_m: 0.0,
            cached_input_per_m: None,
            cache_write_per_m: None,
        }])
        .await
        .unwrap();
    state.reload_pricing().await.unwrap();

    let (user_id, cookie) = register_verify_login(&state, "billed@example.com").await;
    let created = TestRequest::post("/api/me/keys")
        .header("cookie", &cookie)
        .json(&serde_json::json!({ "name": "billed" }))
        .send(state.clone())
        .await;
    assert_eq!(created.status(), 200, "{}", created.json());
    let plaintext = data(&created)["plaintext"].as_str().unwrap().to_owned();

    let topup = TestRequest::post(&format!("/api/admin/users/{user_id}/wallet/topup"))
        .json(&serde_json::json!({ "amount": 1.0, "note": "test" }))
        .send(state.clone())
        .await;
    assert_eq!(topup.status(), 200, "{}", topup.json());

    for i in 0..2 {
        let response = TestRequest::post("/v1/chat/completions")
            .header("authorization", format!("Bearer {plaintext}"))
            .json(&serde_json::json!({
                "model": "gpt-4o",
                "messages": [{ "role": "user", "content": "hi" }]
            }))
            .send(state.clone())
            .await;
        assert_eq!(response.status(), 200, "call {i}: {}", response.json());
    }
    let _ = wait_ledger(&state, user_id, 3).await;

    let denied = TestRequest::post("/v1/chat/completions")
        .header("authorization", format!("Bearer {plaintext}"))
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "messages": [{ "role": "user", "content": "hi" }]
        }))
        .send(state.clone())
        .await;
    assert_eq!(denied.status(), 403, "{}", denied.json());
    // D8 精确形状：OpenAI 错误体里带机器可判的 type 与 balance。
    let body = denied.json();
    assert_eq!(body["error"]["type"], "insufficient_balance", "{body}");
    assert_eq!(body["error"]["balance"], 0.0, "{body}");

    let mut rows = wait_ledger(&state, user_id, 3).await;
    rows.sort_by_key(|row| row.id);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].kind, LedgerKind::Topup);
    assert_eq!(rows[1].kind, LedgerKind::Charge);
    assert_eq!(rows[2].kind, LedgerKind::Charge);
    let mut expected = 0.0;
    for row in &rows {
        expected += row.delta;
        assert!(
            (row.balance_after - expected).abs() < 1e-9,
            "balance_after {} vs {expected}",
            row.balance_after
        );
    }

    // 验证 4 导出：NDJSON 行数 == 3，CSV header + 3 数据行。
    let ndjson = TestRequest::get("/api/me/wallet/ledger/export?format=ndjson")
        .header("cookie", &cookie)
        .send(state.clone())
        .await;
    assert_eq!(ndjson.status(), 200);
    let ndjson_body = String::from_utf8(ndjson.body().to_vec()).unwrap();
    assert_eq!(ndjson_body.lines().count(), 3, "{ndjson_body}");

    let csv = TestRequest::get("/api/me/wallet/ledger/export?format=csv")
        .header("cookie", &cookie)
        .send(state.clone())
        .await;
    assert_eq!(csv.status(), 200);
    let csv_body = String::from_utf8(csv.body().to_vec()).unwrap();
    let mut csv_lines = csv_body.lines();
    assert_eq!(
        csv_lines.next(),
        Some("created_at,kind,delta,balance_after,ref_id,note")
    );
    assert_eq!(csv_lines.count(), 3, "{csv_body}");
}

#[tokio::test]
async fn wallet_apply_same_ref_id_is_idempotent() {
    let state = bootstrap_state(false).await;
    let user_id = state.bootstrap_admin_id();
    let first = state
        .wallet_repo()
        .apply(user_id, -0.5, LedgerKind::Charge, Some("req-1"), "m")
        .await
        .unwrap();
    let second = state
        .wallet_repo()
        .apply(user_id, -0.5, LedgerKind::Charge, Some("req-1"), "m")
        .await
        .unwrap();
    assert!(first);
    assert!(!second);
}

#[tokio::test]
async fn me_logs_are_isolated_across_users() {
    let state = bootstrap_state(false).await;
    let (alice_id, alice_cookie) = register_verify_login(&state, "alice-logs@example.com").await;
    let (bob_id, _) = register_verify_login(&state, "bob-logs@example.com").await;

    let mut alice_log = NewRequestLog::new(
        refract_core::DEFAULT_OWNER_ID,
        "alice-req".into(),
        None,
        Protocol::Chat,
        "alice-model".into(),
        false,
    );
    alice_log.user_id = Some(alice_id);
    let mut bob_log = NewRequestLog::new(
        refract_core::DEFAULT_OWNER_ID,
        "bob-req".into(),
        None,
        Protocol::Chat,
        "bob-model".into(),
        false,
    );
    bob_log.user_id = Some(bob_id);
    state.log_repo().append(&alice_log).await.unwrap();
    state.log_repo().append(&bob_log).await.unwrap();

    let listed = TestRequest::get("/api/me/logs")
        .header("cookie", &alice_cookie)
        .send(state.clone())
        .await;
    assert_eq!(listed.status(), 200, "{}", listed.json());
    let items = data(&listed);
    let items = items.as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["model"], "alice-model");
    let alice_log_id = items[0]["id"].as_i64().unwrap();

    let bob_rows = state
        .log_repo()
        .query(
            refract_core::DEFAULT_OWNER_ID,
            &LogFilter {
                user_id: Some(bob_id),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let bob_log_id = bob_rows[0].id;

    let hidden = TestRequest::get(&format!("/api/me/logs/{bob_log_id}"))
        .header("cookie", &alice_cookie)
        .send(state.clone())
        .await;
    assert_eq!(hidden.status(), 404, "{}", hidden.json());

    let own = TestRequest::get(&format!("/api/me/logs/{alice_log_id}"))
        .header("cookie", alice_cookie)
        .send(state)
        .await;
    assert_eq!(own.status(), 200, "{}", own.json());
}

#[tokio::test]
async fn user_cannot_hit_admin_users_but_admin_can_hit_me_keys() {
    let state = bootstrap_state(false).await;
    let (_, cookie) = register_verify_login(&state, "normal@example.com").await;

    let forbidden = TestRequest::get("/api/admin/users")
        .header("cookie", cookie)
        .send(state.clone())
        .await;
    assert_eq!(forbidden.status(), 403, "{}", forbidden.json());

    let keys = TestRequest::get("/api/me/keys").send(state).await;
    assert_eq!(keys.status(), 200, "{}", keys.json());
}

#[tokio::test]
async fn private_channel_is_invisible_to_other_users() {
    let state = bootstrap_state(true).await;
    let (alice_id, alice_cookie) = register_verify_login(&state, "alice-ch@example.com").await;
    let (bob_id, bob_cookie) = register_verify_login(&state, "bob-ch@example.com").await;

    for id in [alice_id, bob_id] {
        let topup = TestRequest::post(&format!("/api/admin/users/{id}/wallet/topup"))
            .json(&serde_json::json!({ "amount": 10.0, "note": "test" }))
            .send(state.clone())
            .await;
        assert_eq!(topup.status(), 200, "{}", topup.json());
    }

    let mut private = sample_channel("https://private.example", "model-z");
    private.visibility = ChannelVisibility::Private;
    let created = TestRequest::post("/api/me/channels")
        .header("cookie", &alice_cookie)
        .json(&private)
        .send(state.clone())
        .await;
    assert_eq!(created.status(), 200, "{}", created.json());

    let bob_key = TestRequest::post("/api/me/keys")
        .header("cookie", &bob_cookie)
        .json(&serde_json::json!({ "name": "bob" }))
        .send(state.clone())
        .await;
    assert_eq!(bob_key.status(), 200, "{}", bob_key.json());
    let plaintext = data(&bob_key)["plaintext"].as_str().unwrap().to_owned();

    let models = TestRequest::get("/v1/models")
        .header("authorization", format!("Bearer {plaintext}"))
        .send(state)
        .await;
    assert_eq!(models.status(), 200, "{}", models.json());
    let listed = models.json()["data"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        listed.iter().all(|item| item["id"] != "model-z"),
        "{listed:?}"
    );
}
