use super::*;
use refract_core::{ChannelEndpoint, ChannelKind, Credential, ModelEntry, UpstreamAddress};
use refract_store::Database;

async fn test_state() -> AppState {
    let db = Database::open_in_memory().await.unwrap();
    let client = refract_upstream::UpstreamClient::new(Default::default()).unwrap();
    AppState::bootstrap(db, client, false).await.unwrap()
}

fn sample() -> Channel {
    Channel {
        id: 0,
        owner_id: refract_core::DEFAULT_OWNER_ID,
        name: "openai".into(),
        kind: ChannelKind::Single(Protocol::Chat),
        enabled: true,
        priority: 0,
        weight: 1,
        credential: Credential::new("test-key"),
        credentials: Vec::new(),
        key_strategy: Default::default(),
        address: UpstreamAddress::default(),
        endpoints: vec![ChannelEndpoint {
            models: vec![ModelEntry::plain("gpt-4o")],
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
#[test]
fn channel_test_resolves_model_alias_to_upstream_name() {
    let mut endpoint = ChannelEndpoint::new(Protocol::Chat);
    endpoint.models = vec![ModelEntry::mapped("public-name", "vendor/model-v2")];
    let mut channel = sample();
    channel.endpoints = vec![endpoint.clone()];
    assert_eq!(
        test_upstream_model(&channel, &endpoint, Some("public-name")),
        "vendor/model-v2"
    );
    assert_eq!(
        test_upstream_model(&channel, &endpoint, None),
        "vendor/model-v2"
    );
}

#[test]
fn channel_test_falls_back_to_other_endpoint_alias() {
    // 聚合渠道：别名挂在另一个端点上时，也要解析出它的上游名，
    // 而不是把别名原样发给被测端点。
    let mut ep_a = ChannelEndpoint::new(Protocol::Chat);
    ep_a.models = vec![ModelEntry::plain("gpt-4o")];
    let mut ep_b = ChannelEndpoint::new(Protocol::Responses);
    ep_b.order = 1;
    ep_b.models = vec![ModelEntry::mapped("shared-alias", "vendor/other")];
    let mut channel = sample();
    channel.kind = ChannelKind::Aggregate;
    channel.endpoints = vec![ep_a.clone(), ep_b];
    assert_eq!(
        test_upstream_model(&channel, &ep_a, Some("shared-alias")),
        "vendor/other"
    );
    // 完全未登记的模型名照旧原样透传。
    assert_eq!(
        test_upstream_model(&channel, &ep_a, Some("brand-new-model")),
        "brand-new-model"
    );
}

#[tokio::test]
async fn creating_a_channel_refreshes_the_routing_snapshot() {
    // 这是本模块最重要的契约：写完库，路由立刻能看到新渠道。
    let state = test_state().await;
    assert_eq!(state.channels().len(), 0);

    let response = crate::http_test::TestRequest::post("/api/channels")
        .json(&sample())
        .send(state.clone())
        .await;

    assert_eq!(response.status(), 200);
    assert_eq!(
        state.channels().len(),
        1,
        "snapshot must be reloaded after a write"
    );
}

#[tokio::test]
async fn deleting_a_channel_refreshes_the_routing_snapshot() {
    let state = test_state().await;
    let created = state.channel_repo().create(&sample()).await.unwrap();
    state.reload_channels().await.unwrap();
    assert_eq!(state.channels().len(), 1);

    let response = crate::http_test::TestRequest::delete(&format!("/api/channels/{}", created.id))
        .send(state.clone())
        .await;

    assert_eq!(response.status(), 200);
    assert_eq!(state.channels().len(), 0);
}

#[tokio::test]
async fn path_id_wins_over_body_id_on_update() {
    // 防越权改写：PUT /channels/1 带 body.id=2 不能动到 2。
    let state = test_state().await;
    let first = state.channel_repo().create(&sample()).await.unwrap();
    let mut other = sample();
    other.name = "second".into();
    let second = state.channel_repo().create(&other).await.unwrap();

    let mut payload = sample();
    payload.id = second.id;
    payload.name = "hijacked".into();

    let response = crate::http_test::TestRequest::put(&format!("/api/channels/{}", first.id))
        .json(&payload)
        .send(state.clone())
        .await;

    assert_eq!(response.status(), 200);
    let untouched = state
        .channel_repo()
        .get(refract_core::DEFAULT_OWNER_ID, second.id)
        .await
        .unwrap();
    assert_eq!(untouched.name, "second", "another channel was modified");
}

#[tokio::test]
async fn owner_id_from_the_client_is_ignored() {
    let state = test_state().await;
    let mut payload = sample();
    payload.owner_id = 9999;

    let response = crate::http_test::TestRequest::post("/api/channels")
        .json(&payload)
        .send(state.clone())
        .await;

    assert_eq!(response.status(), 200);
    // 能被列出来就说明 owner_id 被改回了服务端常量。
    let listed = state
        .channel_repo()
        .list(refract_core::DEFAULT_OWNER_ID)
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
}

#[tokio::test]
async fn channel_credentials_are_masked_in_every_admin_response() {
    let state = test_state().await;
    let mut payload = sample();
    payload.credential = Credential::new("sk-default-super-secret");
    payload.endpoints[0].credential = Some(Credential::new("sk-endpoint-super-secret"));

    let created = crate::http_test::TestRequest::post("/api/channels")
        .json(&payload)
        .send(state.clone())
        .await;
    assert_eq!(created.status(), 200);
    let created_text = String::from_utf8_lossy(created.body());
    assert!(!created_text.contains("sk-default-super-secret"));
    assert!(!created_text.contains("sk-endpoint-super-secret"));
    assert!(created_text.contains("sk-d…cret"));
    assert!(created_text.contains("sk-e…cret"));

    for path in ["/api/channels", "/api/channels/1"] {
        let response = crate::http_test::TestRequest::get(path)
            .send(state.clone())
            .await;
        assert_eq!(response.status(), 200);
        let text = String::from_utf8_lossy(response.body());
        assert!(
            !text.contains("super-secret"),
            "credential leaked from {path}"
        );
    }
}

#[tokio::test]
async fn updating_a_redacted_channel_preserves_unchanged_credentials() {
    let state = test_state().await;
    let mut original = sample();
    original.credential = Credential::new("sk-default-super-secret");
    original.endpoints[0].credential = Some(Credential::new("sk-endpoint-super-secret"));
    let created = state.channel_repo().create(&original).await.unwrap();

    let response = crate::http_test::TestRequest::get(&format!("/api/channels/{}", created.id))
        .send(state.clone())
        .await;
    let envelope: serde_json::Value = serde_json::from_slice(response.body()).unwrap();
    let mut redacted: Channel = serde_json::from_value(envelope["data"].clone()).unwrap();
    redacted.name = "renamed".into();

    let updated = crate::http_test::TestRequest::put(&format!("/api/channels/{}", created.id))
        .json(&redacted)
        .send(state.clone())
        .await;
    assert_eq!(updated.status(), 200);
    let saved = state
        .channel_repo()
        .get(refract_core::DEFAULT_OWNER_ID, created.id)
        .await
        .unwrap();
    assert_eq!(saved.name, "renamed");
    assert_eq!(saved.credential.expose(), "sk-default-super-secret");
    assert_eq!(
        saved.endpoints[0].credential.as_ref().unwrap().expose(),
        "sk-endpoint-super-secret"
    );
}

#[tokio::test]
async fn masked_credential_that_cannot_be_restored_is_rejected_not_saved() {
    let state = test_state().await;
    let mut original = sample();
    original.endpoints[0].credential = Some(Credential::new("sk-endpoint-super-secret"));
    let created = state.channel_repo().create(&original).await.unwrap();

    // 管理端取回脱敏后的渠道，把端点协议从 chat 改成 messages ——
    // 掩码找不到原端点，还原逻辑无能为力，必须拒绝而不是存掩码。
    let response = crate::http_test::TestRequest::get(&format!("/api/channels/{}", created.id))
        .send(state.clone())
        .await;
    let envelope: serde_json::Value = serde_json::from_slice(response.body()).unwrap();
    let mut redacted: Channel = serde_json::from_value(envelope["data"].clone()).unwrap();
    redacted.kind = ChannelKind::Single(Protocol::Messages);
    redacted.endpoints[0].protocol = Protocol::Messages;

    let update = crate::http_test::TestRequest::put(&format!("/api/channels/{}", created.id))
        .json(&redacted)
        .send(state.clone())
        .await;
    assert_eq!(
        update.status(),
        400,
        "{}",
        String::from_utf8_lossy(update.body())
    );

    // 数据库里的原凭据毫发无损。
    let saved = state
        .channel_repo()
        .get(refract_core::DEFAULT_OWNER_ID, created.id)
        .await
        .unwrap();
    assert_eq!(
        saved.endpoints[0].credential.as_ref().unwrap().expose(),
        "sk-endpoint-super-secret"
    );
}

#[tokio::test]
async fn importing_a_redacted_admin_response_is_rejected_before_touching_data() {
    let state = test_state().await;
    let mut existing = sample();
    existing.credential = Credential::new("sk-keep-me");
    state.channel_repo().create(&existing).await.unwrap();

    // 构造一份「凭据是掩码」的导入文档（等价于把管理 GET 响应喂回来）。
    let mut masked = sample();
    masked.name = "from-redacted-export".into();
    masked.credential = Credential::new(Credential::new("sk-masked-secret").masked());
    let document = serde_json::json!({
        "version": 1,
        "channels": [masked],
        "keys": [],
        "settings": {}
    });

    let response = crate::http_test::TestRequest::post("/api/import")
        .json(&serde_json::json!({ "mode": "replace", "data": document }))
        .send(state.clone())
        .await;
    assert_eq!(
        response.status(),
        400,
        "{}",
        String::from_utf8_lossy(response.body())
    );

    // replace 模式被拒后，现有渠道必须原封不动。
    let channels = state
        .channel_repo()
        .list(refract_core::DEFAULT_OWNER_ID)
        .await
        .unwrap();
    assert_eq!(channels.len(), 1);
    assert_eq!(channels[0].credential.expose(), "sk-keep-me");
}

#[tokio::test]
async fn duplicating_a_channel_creates_a_disabled_copy_with_full_config() {
    let state = test_state().await;
    let mut original = sample();
    original.credential = Credential::new("sk-copy-me");
    let created = state.channel_repo().create(&original).await.unwrap();

    let response =
        crate::http_test::TestRequest::post(&format!("/api/channels/{}/duplicate", created.id))
            .send(state.clone())
            .await;
    assert_eq!(response.status(), 200);

    let listed = state
        .channel_repo()
        .list(refract_core::DEFAULT_OWNER_ID)
        .await
        .unwrap();
    assert_eq!(listed.len(), 2);
    let copy = listed.iter().find(|c| c.id != created.id).unwrap();
    assert_eq!(copy.name, format!("{} 副本", created.name));
    assert!(!copy.enabled, "duplicated channel must start disabled");
    // 凭据要复制到位（数据库里的领域实体，不是响应里的掩码）。
    assert_eq!(copy.credential.expose(), "sk-copy-me");
    assert_eq!(copy.endpoints.len(), created.endpoints.len());
}

#[tokio::test]
async fn bulk_action_applies_to_every_listed_channel() {
    let state = test_state().await;
    let mut ids = Vec::new();
    for name in ["a", "b", "c"] {
        let mut channel = sample();
        channel.name = name.into();
        ids.push(state.channel_repo().create(&channel).await.unwrap().id);
    }

    let disable = crate::http_test::TestRequest::post("/api/channels/bulk")
        .json(&serde_json::json!({ "ids": ids, "action": "disable" }))
        .send(state.clone())
        .await;
    assert_eq!(disable.status(), 200);
    let listed = state
        .channel_repo()
        .list(refract_core::DEFAULT_OWNER_ID)
        .await
        .unwrap();
    assert!(listed.iter().all(|c| !c.enabled));
    // 路由快照也必须同步刷新。
    assert!(state.channels().iter().all(|c| !c.enabled));

    // 删除时包含一个不存在的 id：批量操作必须宽容缺席者。
    let delete = crate::http_test::TestRequest::post("/api/channels/bulk")
        .json(&serde_json::json!({ "ids": [ids[0], ids[1], 424242], "action": "delete" }))
        .send(state.clone())
        .await;
    assert_eq!(delete.status(), 200);
    let body: serde_json::Value = serde_json::from_slice(delete.body()).unwrap();
    assert_eq!(body["data"]["affected"], 2);
    assert_eq!(
        state
            .channel_repo()
            .list(refract_core::DEFAULT_OWNER_ID)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn export_import_roundtrip_restores_channels_keys_and_settings() {
    let source = test_state().await;
    let mut channel = sample();
    channel.credential = Credential::new("sk-export-secret");
    source.channel_repo().create(&channel).await.unwrap();
    let (_, plaintext) = source
        .key_repo()
        .create(
            refract_core::DEFAULT_OWNER_ID,
            refract_store::NewApiKey {
                name: "backup-key".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let mut policy = source.policy();
    policy.native_first = false;
    policy.max_attempts = 5;
    source
        .settings_repo()
        .set_routing_policy(&policy)
        .await
        .unwrap();
    source.reload_policy().await.unwrap();

    let exported = crate::http_test::TestRequest::get("/api/export")
        .send(source.clone())
        .await;
    assert_eq!(exported.status(), 200);
    let envelope: serde_json::Value = serde_json::from_slice(exported.body()).unwrap();
    let document = envelope["data"].clone();
    // 导出必须携带可恢复的凭据明文与密钥哈希。
    assert_eq!(
        document["channels"][0]["credential"], "sk-export-secret",
        "export must carry usable credentials"
    );
    assert!(document["keys"][0]["key_hash"].is_string());

    // 在全新实例导入。
    let target = test_state().await;
    let imported = crate::http_test::TestRequest::post("/api/import")
        .json(&serde_json::json!({ "mode": "replace", "data": document }))
        .send(target.clone())
        .await;
    assert_eq!(imported.status(), 200);
    let result: serde_json::Value = serde_json::from_slice(imported.body()).unwrap();
    assert_eq!(result["data"]["channels_imported"], 1);
    assert_eq!(result["data"]["keys_imported"], 1);

    // 渠道、密钥（原明文可鉴权）、策略全部就位，且路由快照已刷新。
    assert_eq!(target.channels().len(), 1);
    assert_eq!(target.channels()[0].credential.expose(), "sk-export-secret");
    assert!(
        target
            .key_repo()
            .find_by_plaintext(&plaintext)
            .await
            .unwrap()
            .is_some(),
        "the original key plaintext must keep working after restore"
    );
    assert!(!target.policy().native_first);
    assert_eq!(target.policy().max_attempts, 5);

    // 再次以 merge 导入：全部跳过，不产生重复。
    let merged = crate::http_test::TestRequest::post("/api/import")
        .json(&serde_json::json!({ "mode": "merge", "data": envelope["data"] }))
        .send(target.clone())
        .await;
    let result: serde_json::Value = serde_json::from_slice(merged.body()).unwrap();
    assert_eq!(result["data"]["channels_skipped"], 1);
    assert_eq!(result["data"]["keys_skipped"], 1);
    // 跳过明细带名字：用户要知道的是「哪些没进来」，不只是数量。
    assert_eq!(result["data"]["skipped_channels"][0], "openai");
    assert_eq!(result["data"]["skipped_keys"][0], "backup-key");
    assert_eq!(target.channels().len(), 1);
}

#[tokio::test]
async fn import_rejects_unknown_versions_and_invalid_channels() {
    let state = test_state().await;

    let wrong_version = crate::http_test::TestRequest::post("/api/import")
        .json(&serde_json::json!({
            "data": {
                "version": 99,
                "settings": { "routing_policy": refract_core::RoutingPolicy::default(), "log_retention_days": 30 }
            }
        }))
        .send(state.clone())
        .await;
    assert_eq!(wrong_version.status(), 400);

    // 无端点的渠道非法，导入必须整体拒绝且不落任何数据。
    let mut invalid = sample();
    invalid.endpoints.clear();
    let response = crate::http_test::TestRequest::post("/api/import")
        .json(&serde_json::json!({
            "data": {
                "version": 1,
                "channels": [invalid],
                "settings": { "routing_policy": refract_core::RoutingPolicy::default(), "log_retention_days": 30 }
            }
        }))
        .send(state.clone())
        .await;
    assert_eq!(response.status(), 400);
    assert!(state.channels().is_empty());
}

#[tokio::test]
async fn duplicate_protocol_endpoints_are_rejected() {
    let state = test_state().await;
    let mut payload = sample();
    payload.kind = ChannelKind::Aggregate;
    payload.endpoints.push(ChannelEndpoint {
        models: vec![ModelEntry::plain("gpt-4o-mini")],
        ..ChannelEndpoint::new(Protocol::Chat)
    });

    let response = crate::http_test::TestRequest::post("/api/channels")
        .json(&payload)
        .send(state.clone())
        .await;

    assert_eq!(response.status(), 400);
    let body = String::from_utf8_lossy(response.body());
    assert!(body.contains("duplicate endpoint"), "got: {body}");
}

#[tokio::test]
async fn a_channel_without_endpoints_is_rejected() {
    let state = test_state().await;
    let mut payload = sample();
    payload.endpoints.clear();

    let response = crate::http_test::TestRequest::post("/api/channels")
        .json(&payload)
        .send(state.clone())
        .await;

    assert_eq!(response.status(), 400);
}

#[tokio::test]
async fn single_kind_must_match_its_endpoint() {
    let state = test_state().await;
    let mut payload = sample();
    // 声明是 Messages 渠道，但端点只有 Chat —— UI 会显示错误的类型。
    payload.kind = ChannelKind::Single(Protocol::Messages);

    let response = crate::http_test::TestRequest::post("/api/channels")
        .json(&payload)
        .send(state.clone())
        .await;

    assert_eq!(response.status(), 400);
}

#[tokio::test]
async fn enabled_toggle_is_not_shadowed_by_the_id_route() {
    // 路由顺序回归测试：`/channels/:id/enabled` 不能被 `/channels/:id` 吃掉。
    let state = test_state().await;
    let created = state.channel_repo().create(&sample()).await.unwrap();

    let response =
        crate::http_test::TestRequest::post(&format!("/api/channels/{}/enabled", created.id))
            .json(&serde_json::json!({ "enabled": false }))
            .send(state.clone())
            .await;

    assert_eq!(response.status(), 200);
    let updated = state
        .channel_repo()
        .get(refract_core::DEFAULT_OWNER_ID, created.id)
        .await
        .unwrap();
    assert!(!updated.enabled);
    // 快照也要跟上，否则被禁用的渠道还在被路由。
    assert!(state.channels().iter().all(|c| !c.enabled));
}

#[tokio::test]
async fn api_key_plaintext_is_returned_exactly_once() {
    let state = test_state().await;

    let response = crate::http_test::TestRequest::post("/api/keys")
        .json(&serde_json::json!({ "name": "laptop" }))
        .send(state.clone())
        .await;

    assert_eq!(response.status(), 200);
    let body: serde_json::Value = serde_json::from_slice(response.body()).unwrap();
    let plaintext = body["data"]["plaintext"].as_str().unwrap().to_owned();
    assert!(plaintext.starts_with("rk-"));

    // 列表接口不能再带出明文。
    let listed = crate::http_test::TestRequest::get("/api/keys")
        .send(state.clone())
        .await;
    let text = String::from_utf8_lossy(listed.body()).into_owned();
    assert!(
        !text.contains(&plaintext),
        "plaintext key leaked through the list endpoint"
    );
}

#[tokio::test]
async fn balance_probe_uses_the_billing_endpoints_and_caches_the_result() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path(
            "/v1/dashboard/billing/subscription",
        ))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "hard_limit_usd": 120.0 })),
        )
        .mount(&server)
        .await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/v1/dashboard/billing/usage"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                // total_usage 单位是美分。
                .set_body_json(serde_json::json!({ "total_usage": 2550.0 })),
        )
        .mount(&server)
        .await;

    let state = test_state().await;
    let mut channel = sample();
    channel.address = UpstreamAddress {
        unofficial: true,
        full_address: false,
        base_url: Some(server.uri()),
        version_prefix: None,
        path: None,
    };
    state.channel_repo().create(&channel).await.unwrap();
    state.reload_channels().await.unwrap();
    let id = state.channels()[0].id;

    let response = crate::http_test::TestRequest::post(&format!("/api/channels/{id}/balance"))
        .send(state.clone())
        .await;
    assert_eq!(response.status(), 200);
    let body: serde_json::Value = serde_json::from_slice(response.body()).unwrap();
    // 120 − 25.50 = 94.50
    assert!((body["data"]["balance"].as_f64().unwrap() - 94.5).abs() < 1e-9);

    // 结果缓存进渠道行。
    let channel = state
        .channel_repo()
        .get(refract_core::DEFAULT_OWNER_ID, id)
        .await
        .unwrap();
    assert!((channel.balance.unwrap() - 94.5).abs() < 1e-9);
    assert!(channel.balance_updated_at.is_some());
}

#[tokio::test]
async fn balance_probe_honors_a_path_prefixed_base() {
    // 中转站的 base 常带路径前缀（如 `…/relay`）：账单端点必须和
    // `/models` 同级拼出来，而不是被字符串裁剪弄丢前缀。
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path(
            "/relay/v1/dashboard/billing/subscription",
        ))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "hard_limit_usd": 10.0 })),
        )
        .mount(&server)
        .await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path(
            "/relay/v1/dashboard/billing/usage",
        ))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "total_usage": 0.0 })),
        )
        .mount(&server)
        .await;

    let state = test_state().await;
    let mut channel = sample();
    channel.address = UpstreamAddress {
        unofficial: true,
        full_address: false,
        base_url: Some(format!("{}/relay", server.uri())),
        version_prefix: None,
        path: None,
    };
    state.channel_repo().create(&channel).await.unwrap();
    state.reload_channels().await.unwrap();
    let id = state.channels()[0].id;

    let response = crate::http_test::TestRequest::post(&format!("/api/channels/{id}/balance"))
        .send(state.clone())
        .await;
    assert_eq!(response.status(), 200);
    let body: serde_json::Value = serde_json::from_slice(response.body()).unwrap();
    assert!((body["data"]["balance"].as_f64().unwrap() - 10.0).abs() < 1e-9);
}

#[tokio::test]
async fn key_update_and_usage_reset_keep_the_key_itself() {
    let state = test_state().await;
    let (created, plaintext) = state
        .key_repo()
        .create(
            refract_core::DEFAULT_OWNER_ID,
            refract_store::NewApiKey {
                name: "before".into(),
                quota: 100,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    state
        .key_repo()
        .record_usage(created.id, 60, 0.0)
        .await
        .unwrap();

    // 编辑治理属性：名字、限速、备注全改，密钥本体不动。
    let response = crate::http_test::TestRequest::put(&format!("/api/keys/{}", created.id))
        .json(&serde_json::json!({
            "name": "after",
            "quota": 500,
            "rpm_limit": 30,
            "tpm_limit": 100000,
            "note": "给 Cursor 用的"
        }))
        .send(state.clone())
        .await;
    assert_eq!(response.status(), 200);
    let body: serde_json::Value = serde_json::from_slice(response.body()).unwrap();
    assert_eq!(body["data"]["name"], "after");
    assert_eq!(body["data"]["rpm_limit"], 30);
    assert_eq!(body["data"]["note"], "给 Cursor 用的");
    // 已用配额不因编辑而变。
    assert_eq!(body["data"]["used_quota"], 60);

    // 原明文照常可用（key_hash 未变）。
    let found = state
        .key_repo()
        .find_by_plaintext(&plaintext)
        .await
        .unwrap()
        .expect("plaintext still resolves");
    assert_eq!(found.name, "after");

    // 重置用量。
    let response =
        crate::http_test::TestRequest::post(&format!("/api/keys/{}/reset-usage", created.id))
            .send(state.clone())
            .await;
    assert_eq!(response.status(), 200);
    let after = state
        .key_repo()
        .get(refract_core::DEFAULT_OWNER_ID, created.id)
        .await
        .unwrap();
    assert_eq!(after.used_quota, 0);
}

#[tokio::test]
async fn playground_chat_goes_through_the_full_gateway_pipeline() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/chat/completions"))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "chatcmpl-pg",
                "model": "gpt-4o",
                "choices": [{
                    "index": 0,
                    "message": { "role": "assistant", "content": "pong" },
                    "finish_reason": "stop"
                }],
                "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
            })),
        )
        .mount(&server)
        .await;

    let state = test_state().await;
    let mut channel = sample();
    channel.address = UpstreamAddress {
        unofficial: true,
        full_address: false,
        base_url: Some(server.uri()),
        version_prefix: None,
        path: None,
    };
    state.channel_repo().create(&channel).await.unwrap();
    state.reload_channels().await.unwrap();

    let response = crate::http_test::TestRequest::post("/api/playground/chat")
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "messages": [{ "role": "user", "content": "ping" }]
        }))
        .send(state.clone())
        .await;

    assert_eq!(response.status(), 200);
    let body: serde_json::Value = serde_json::from_slice(response.body()).unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], "pong");
}

#[tokio::test]
async fn models_endpoint_derives_from_enabled_channels_only() {
    let state = test_state().await;
    state.channel_repo().create(&sample()).await.unwrap();

    let mut disabled = sample();
    disabled.name = "off".into();
    disabled.enabled = false;
    disabled.endpoints[0].models = vec![ModelEntry::plain("hidden-model")];
    state.channel_repo().create(&disabled).await.unwrap();
    state.reload_channels().await.unwrap();

    let response = crate::http_test::TestRequest::get("/api/models")
        .send(state.clone())
        .await;

    let body: serde_json::Value = serde_json::from_slice(response.body()).unwrap();
    let names: Vec<&str> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["gpt-4o"]);
}

#[tokio::test]
async fn routing_policy_roundtrips_and_updates_the_snapshot() {
    let state = test_state().await;
    let mut policy = state.policy();
    policy.native_first = !policy.native_first;

    let response = crate::http_test::TestRequest::put("/api/settings/routing")
        .json(&policy)
        .send(state.clone())
        .await;

    assert_eq!(response.status(), 200);
    assert_eq!(state.policy().native_first, policy.native_first);
}

#[tokio::test]
async fn log_retention_roundtrips_and_rejects_out_of_range_values() {
    let state = test_state().await;

    let initial = crate::http_test::TestRequest::get("/api/settings/log-retention")
        .send(state.clone())
        .await;
    assert_eq!(initial.status(), 200);
    let initial_body: serde_json::Value = serde_json::from_slice(initial.body()).unwrap();
    assert_eq!(
        initial_body["data"]["days"],
        refract_store::settings_repo::DEFAULT_LOG_RETENTION_DAYS
    );

    let updated = crate::http_test::TestRequest::put("/api/settings/log-retention")
        .json(&serde_json::json!({ "days": 90 }))
        .send(state.clone())
        .await;
    assert_eq!(updated.status(), 200);
    assert_eq!(state.settings_repo().log_retention_days().await, 90);

    for days in [0, refract_store::settings_repo::MAX_LOG_RETENTION_DAYS + 1] {
        let rejected = crate::http_test::TestRequest::put("/api/settings/log-retention")
            .json(&serde_json::json!({ "days": days }))
            .send(state.clone())
            .await;
        assert_eq!(rejected.status(), 400, "days={days}");
    }
    assert_eq!(state.settings_repo().log_retention_days().await, 90);
}

#[tokio::test]
async fn breaker_policy_roundtrips_hot_updates_and_rejects_bad_values() {
    let state = test_state().await;

    // 默认值可读。
    let initial = crate::http_test::TestRequest::get("/api/settings/breaker")
        .send(state.clone())
        .await;
    assert_eq!(initial.status(), 200);
    let body: serde_json::Value = serde_json::from_slice(initial.body()).unwrap();
    assert_eq!(body["data"]["failure_threshold"], 5);

    // 更新后：持久化 + 共享健康仓储热更新（不用重启）。
    let updated = crate::http_test::TestRequest::put("/api/settings/breaker")
        .json(&serde_json::json!({
            "failure_threshold": 3,
            "base_cooldown_secs": 10,
            "max_cooldown_secs": 300,
        }))
        .send(state.clone())
        .await;
    assert_eq!(updated.status(), 200);
    assert_eq!(state.health_repo().policy().failure_threshold, 3);
    assert_eq!(state.health_repo().policy().base_cooldown_secs, 10);

    // 非法组合被拒，且不影响已生效的策略。
    for bad in [
        serde_json::json!({ "failure_threshold": 3, "base_cooldown_secs": 0, "max_cooldown_secs": 300 }),
        serde_json::json!({ "failure_threshold": 3, "base_cooldown_secs": 600, "max_cooldown_secs": 300 }),
        serde_json::json!({ "failure_threshold": 1_000_000, "base_cooldown_secs": 10, "max_cooldown_secs": 300 }),
    ] {
        let rejected = crate::http_test::TestRequest::put("/api/settings/breaker")
            .json(&bad)
            .send(state.clone())
            .await;
        assert_eq!(rejected.status(), 400, "{bad}");
    }
    assert_eq!(state.health_repo().policy().failure_threshold, 3);
}

#[tokio::test]
async fn global_limits_roundtrip_hot_updates_and_rejects_bad_values() {
    let state = test_state().await;

    // 默认全 0（不限）。
    let initial = crate::http_test::TestRequest::get("/api/settings/limits")
        .send(state.clone())
        .await;
    assert_eq!(initial.status(), 200);
    let body: serde_json::Value = serde_json::from_slice(initial.body()).unwrap();
    assert_eq!(body["data"]["rpm"], 0);
    assert_eq!(body["data"]["tpm"], 0);
    assert_eq!(body["data"]["max_concurrency"], 0);

    // 更新后：持久化 + AppState 快照热更新（不用重启）。
    let updated = crate::http_test::TestRequest::put("/api/settings/limits")
        .json(&serde_json::json!({
            "rpm": 600,
            "tpm": 2_000_000,
            "max_concurrency": 16,
        }))
        .send(state.clone())
        .await;
    assert_eq!(updated.status(), 200);
    assert_eq!(state.global_limits().rpm, 600);
    assert_eq!(state.global_limits().tpm, 2_000_000);
    assert_eq!(state.global_limits().max_concurrency, 16);

    // 省略 tpm 的旧前端请求体仍可接受，缺省为 0（不限）。
    let legacy = crate::http_test::TestRequest::put("/api/settings/limits")
        .json(&serde_json::json!({ "rpm": 60, "max_concurrency": 8 }))
        .send(state.clone())
        .await;
    assert_eq!(legacy.status(), 200);
    assert_eq!(state.global_limits().tpm, 0);

    // 越界值被拒，且不影响已生效的限制。
    for bad in [
        serde_json::json!({ "rpm": 1_000_001 }),
        serde_json::json!({ "tpm": 1_000_000_001u64 }),
        serde_json::json!({ "max_concurrency": 100_001 }),
    ] {
        let rejected = crate::http_test::TestRequest::put("/api/settings/limits")
            .json(&bad)
            .send(state.clone())
            .await;
        assert_eq!(rejected.status(), 400, "{bad}");
    }
    assert_eq!(state.global_limits().rpm, 60);
    assert_eq!(state.global_limits().max_concurrency, 8);
}

#[tokio::test]
async fn admin_token_can_be_set_and_cleared_but_never_read() {
    let state = test_state().await;

    let set = crate::http_test::TestRequest::put("/api/settings/admin-token")
        .json(&serde_json::json!({ "token": "s3cret" }))
        .send(state.clone())
        .await;
    assert_eq!(set.status(), 200);

    // 设置之后，无令牌的请求必须被拒。
    let denied = crate::http_test::TestRequest::get("/api/channels")
        .send(state.clone())
        .await;
    assert_eq!(denied.status(), 401);

    // 带上正确令牌可以通过，并能清除。
    let cleared = crate::http_test::TestRequest::put("/api/settings/admin-token")
        .header("x-admin-token", "s3cret")
        .json(&serde_json::json!({ "token": null }))
        .send(state.clone())
        .await;
    assert_eq!(cleared.status(), 200);

    let open = crate::http_test::TestRequest::get("/api/channels")
        .send(state.clone())
        .await;
    assert_eq!(open.status(), 200);
}

#[tokio::test]
async fn unknown_channel_yields_404_not_500() {
    let state = test_state().await;
    let response = crate::http_test::TestRequest::get("/api/channels/424242")
        .send(state.clone())
        .await;
    assert_eq!(response.status(), 404);
}

#[tokio::test]
async fn public_key_endpoint_is_public_and_enables_e2e_envelope_decryption() {
    use aes_gcm::aead::{Aead as _, KeyInit as _, Payload};
    use aes_gcm::{Aes256Gcm, Nonce};
    use base64::Engine as _;
    use hkdf::Hkdf;
    use p256::elliptic_curve::rand_core::OsRng;
    use p256::elliptic_curve::sec1::{FromEncodedPoint as _, ToEncodedPoint as _};
    use p256::{EncodedPoint, PublicKey, SecretKey};
    use sha2::Sha256;

    let state = test_state().await;

    // 1. GET /api/crypto/public-key 无需鉴权即可访问
    let pk_res = crate::http_test::TestRequest::get("/api/crypto/public-key")
        .send(state.clone())
        .await;
    assert_eq!(pk_res.status(), 200);

    let pk_json: serde_json::Value = serde_json::from_slice(pk_res.body()).unwrap();
    let server_pub_raw_b64 = pk_json["data"]["public_key_raw"].as_str().unwrap();

    // 2. 模拟前端：ECDH → HKDF-SHA256 → AES-256-GCM（带 AAD）
    let client_secret = SecretKey::random(&mut OsRng);
    let client_pub = client_secret.public_key();
    let client_raw = client_pub.to_encoded_point(false);
    let client_pub_b64 = base64::engine::general_purpose::STANDARD.encode(client_raw.as_bytes());

    let server_pub_bytes = base64::engine::general_purpose::STANDARD
        .decode(server_pub_raw_b64)
        .unwrap();
    let server_point = EncodedPoint::from_bytes(&server_pub_bytes).unwrap();
    let server_pub = PublicKey::from_encoded_point(&server_point).unwrap();

    let client_shared =
        p256::ecdh::diffie_hellman(client_secret.to_nonzero_scalar(), server_pub.as_affine());
    let mut salt = Vec::with_capacity(130);
    salt.extend_from_slice(client_raw.as_bytes());
    salt.extend_from_slice(&server_pub_bytes);
    let mut derived_key = [0u8; 32];
    Hkdf::<Sha256>::new(Some(&salt), client_shared.raw_secret_bytes().as_ref())
        .expand(crate::crypto::HKDF_INFO, &mut derived_key)
        .unwrap();

    let channel_payload = serde_json::json!({
        "name": "encrypted-e2e-channel",
        "kind": "chat",
        "credential": "sk-secret-from-browser-e2e",
        "endpoints": [{ "protocol": "chat", "models": [{"name": "gpt-4o"}] }]
    });
    let plaintext = serde_json::to_vec(&channel_payload).unwrap();
    let iv_bytes = [42u8; 12];
    let iv_b64 = base64::engine::general_purpose::STANDARD.encode(iv_bytes);
    let aad = format!("{client_pub_b64}:{iv_b64}");
    let cipher = Aes256Gcm::new_from_slice(&derived_key).unwrap();
    let ciphertext_bytes = cipher
        .encrypt(
            &Nonce::from(iv_bytes),
            Payload {
                msg: plaintext.as_ref(),
                aad: aad.as_bytes(),
            },
        )
        .unwrap();

    let envelope = serde_json::json!({
        "__encrypted": true,
        "ephemeral_pub": client_pub_b64,
        "iv": base64::engine::general_purpose::STANDARD.encode(iv_bytes),
        "ciphertext": base64::engine::general_purpose::STANDARD.encode(ciphertext_bytes),
    });

    // 4. POST /api/channels 发送加密信封
    let create_res = crate::http_test::TestRequest::post("/api/channels")
        .json(&envelope)
        .send(state.clone())
        .await;
    if create_res.status() != 200 {
        panic!(
            "create failed with status {}: {}",
            create_res.status(),
            String::from_utf8_lossy(create_res.body())
        );
    }

    let created_json: serde_json::Value = serde_json::from_slice(create_res.body()).unwrap();
    assert_eq!(created_json["data"]["name"], "encrypted-e2e-channel");
    // 返回数据已被脱敏
    assert!(
        created_json["data"]["credential"]
            .as_str()
            .unwrap()
            .contains('…')
    );

    // 5. 校验数据库中实际存入了真实的明文（在内存中透明解密）
    let channel_id = created_json["data"]["id"].as_i64().unwrap();
    let stored = state
        .channel_repo()
        .get(refract_core::DEFAULT_OWNER_ID, channel_id)
        .await
        .unwrap();
    assert_eq!(stored.credential.expose(), "sk-secret-from-browser-e2e");
}

#[tokio::test]
async fn auth_session_and_cookie_login_logout_flow() {
    let state = test_state().await;
    // 1. 未配置令牌时：/api/auth/session 返回 configured: false, authenticated: true
    let res = crate::http_test::TestRequest::get("/api/auth/session")
        .send(state.clone())
        .await;
    assert_eq!(res.status(), 200);
    let val: serde_json::Value = serde_json::from_slice(res.body()).unwrap();
    assert_eq!(val["data"]["configured"], false);
    assert_eq!(val["data"]["authenticated"], true);

    // 2. 配置管理令牌
    let set_res = crate::http_test::TestRequest::put("/api/settings/admin-token")
        .json(&serde_json::json!({ "token": "admin-secret-pwd" }))
        .send(state.clone())
        .await;
    assert_eq!(set_res.status(), 200);
    let cookie_header = set_res
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(cookie_header.contains("refract_session="));
    assert!(cookie_header.contains("HttpOnly"));
    assert!(cookie_header.contains("SameSite=Strict"));

    // 3. 无 Cookie 访问受保护接口返回 401
    let blocked = crate::http_test::TestRequest::get("/api/settings/ip-limits")
        .send(state.clone())
        .await;
    assert_eq!(blocked.status(), 401);

    // 4. 携带生成的 Cookie 访问受保护接口成功
    let session_val = cookie_header.split(';').next().unwrap();
    let authed = crate::http_test::TestRequest::get("/api/settings/ip-limits")
        .header("cookie", session_val)
        .send(state.clone())
        .await;
    assert_eq!(authed.status(), 200);

    // 5. 错误 Token 登录被拒绝
    let wrong_login = crate::http_test::TestRequest::post("/api/auth/login")
        .json(&serde_json::json!({ "token": "wrong-token" }))
        .send(state.clone())
        .await;
    assert_eq!(wrong_login.status(), 401);

    // 6. 正确 Token 登录成功并获取新 Cookie
    let login_res = crate::http_test::TestRequest::post("/api/auth/login")
        .json(&serde_json::json!({ "token": "admin-secret-pwd" }))
        .send(state.clone())
        .await;
    assert_eq!(login_res.status(), 200);
    let new_cookie = login_res
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(new_cookie.contains("refract_session="));

    // 7. 登出清除 Cookie
    let logout_res = crate::http_test::TestRequest::post("/api/auth/logout")
        .send(state.clone())
        .await;
    assert_eq!(logout_res.status(), 200);
    let clear_cookie = logout_res
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(clear_cookie.contains("refract_session="));
    assert!(clear_cookie.contains("Max-Age=0"));
}

#[tokio::test]
async fn https_forwarded_proto_sets_secure_session_cookie() {
    let state = test_state().await;
    crate::http_test::TestRequest::put("/api/settings/admin-token")
        .json(&serde_json::json!({ "token": "admin-secret-pwd" }))
        .send(state.clone())
        .await;

    let login = crate::http_test::TestRequest::post("/api/auth/login")
        .header("x-forwarded-proto", "https")
        .json(&serde_json::json!({ "token": "admin-secret-pwd" }))
        .send(state.clone())
        .await;
    assert_eq!(login.status(), 200);
    let cookie = login.headers().get("set-cookie").unwrap().to_str().unwrap();
    assert!(cookie.contains("Secure"), "{cookie}");

    let plain = crate::http_test::TestRequest::post("/api/auth/login")
        .json(&serde_json::json!({ "token": "admin-secret-pwd" }))
        .send(state)
        .await;
    let plain_cookie = plain.headers().get("set-cookie").unwrap().to_str().unwrap();
    assert!(!plain_cookie.contains("Secure"), "{plain_cookie}");
}

#[tokio::test]
async fn failed_logins_lock_out_with_retry_after() {
    let state = test_state().await;
    crate::http_test::TestRequest::put("/api/settings/admin-token")
        .json(&serde_json::json!({ "token": "admin-secret-pwd" }))
        .send(state.clone())
        .await;

    for attempt in 1..=5 {
        let denied = crate::http_test::TestRequest::post("/api/auth/login")
            .json(&serde_json::json!({ "token": "wrong" }))
            .send(state.clone())
            .await;
        if attempt < 5 {
            assert_eq!(denied.status(), 401, "attempt {attempt}");
        } else {
            assert_eq!(denied.status(), 403, "fifth failure should lock");
            assert!(denied.headers().get("retry-after").is_some());
        }
    }
}
