//! 管理 REST API（`/api/...`）。
//!
//! 只服务本项目的前端，形状由我们决定 —— 所以用统一的 `{data}` / `{error}` 包裹，
//! 而不是模仿任何上游厂商。
//!
//! 三条贯穿全模块的规则：
//!
//! 1. **写渠道必刷快照**。路由读的是 `AppState` 里的 `ArcSwap` 内存快照，
//!    不刷新等于改了配置不生效 —— 这是最容易漏、也最难debug的一类 bug，
//!    所以收敛到 [`commit_channels`] 一个函数里，每个写处理器都必须走它。
//! 2. **owner_id 永远来自服务端常量**，不接受客户端传入。现在是单用户，
//!    但把它做成「从请求里读」会在加多用户那天变成越权漏洞。
//! 3. **密钥明文只在创建响应里出现一次**。库里只有哈希，取不回来。

use refract_core::{
    Action, Channel, ChannelId, Credential, GatewayError, Protocol, RoutingPolicy, UpstreamAddress,
};
use refract_store::LogFilter;
use serde::{Deserialize, Serialize};
use xitca_web::body::{RequestBody, ResponseBody};
use xitca_web::handler::body::Body;
use xitca_web::handler::handler_service;
use xitca_web::handler::params::Params;
use xitca_web::handler::query::Query;
use xitca_web::handler::state::StateRef;
use xitca_web::http::{HeaderMap, HeaderValue, StatusCode, WebResponse};
use xitca_web::route::{get, post, put};
use xitca_web::{App, NestApp};

use crate::auth::require_admin;
use crate::error::{AppError, collect_limited, json_response, store_to_gateway};
use crate::extract::{AdminJson, decode_admin_json};
use crate::state::AppState;

/// 统一成功包裹。
///
/// 前端只需要认一种形状：成功看 `data`，失败看 `error`（见 [`crate::ErrorEnvelope`]）。
#[derive(Debug, Serialize)]
struct Envelope<T> {
    data: T,
}

/// 把仓储结果渲染成 JSON 响应。
fn ok<T: Serialize>(value: T) -> Result<WebResponse, AppError> {
    Ok(json_response(StatusCode::OK, &Envelope { data: value }))
}

/// 把 `StoreError` 转成管理面错误。
fn reject(err: refract_store::StoreError) -> AppError {
    AppError::Admin(store_to_gateway(err))
}

/// 提交渠道变更：写库之后**必须**刷新内存快照。
///
/// 这个函数存在的唯一目的就是让「忘记刷新」变得不可能 —— 写路径全部经由它。
async fn commit_channels(state: &AppState) -> Result<(), AppError> {
    state.reload_channels().await.map_err(reject)
}

/// 把渠道凭据替换成不可用于鉴权的掩码后再返回管理端。
///
/// 领域实体必须保留明文才能持久化和请求上游，因此脱敏只能发生在 HTTP 边界；
/// 若改 `Credential` 的全局 `Serialize`，数据库 JSON 也会被写成掩码。
fn redact_channel(mut channel: Channel) -> Channel {
    channel.credential = Credential::new(channel.credential.masked());
    // 池子里的每一把都脱敏；空主密钥的纯池子渠道同样逐把处理。
    channel.credentials = channel
        .credentials
        .iter()
        .map(|c| Credential::new(c.masked()))
        .collect();
    for endpoint in &mut channel.endpoints {
        if let Some(credential) = &mut endpoint.credential {
            *credential = Credential::new(credential.masked());
        }
    }
    channel
}

/// 凭据是否是我们自己的脱敏占位符。
///
/// 真实 API key 是 ASCII；`…`(U+2026) 与 `•`(U+2022) 只会出现在
/// [`Credential::masked`] 的输出里。含这两个字符的「凭据」一定是管理端
/// 把脱敏值原样带了回来。
fn looks_masked(value: &str) -> bool {
    value.contains('…') || value.contains('•')
}

/// 拒绝还原不了的掩码凭据。
///
/// [`restore_unchanged_credentials`] 只能还原「还在原位」的掩码；端点协议
/// 一旦变更，掩码就找不到对应的旧凭据了。放行意味着把 `sk-a…9f2c` 这样的
/// 占位符存成真实密钥 —— 之后每个请求都 401，而用户看不出配置哪里错了。
fn reject_masked_credentials(channel: &Channel) -> Result<(), AppError> {
    let offending = if looks_masked(channel.credential.expose()) {
        Some("渠道默认".to_owned())
    } else if channel.credentials.iter().any(|c| looks_masked(c.expose())) {
        Some("密钥池".to_owned())
    } else {
        channel
            .endpoints
            .iter()
            .find(|ep| {
                ep.credential
                    .as_ref()
                    .is_some_and(|c| looks_masked(c.expose()))
            })
            .map(|ep| format!("{} 端点", ep.protocol))
    };
    match offending {
        Some(place) => Err(AppError::Admin(GatewayError::invalid_request(format!(
            "{place}密钥是脱敏占位符（含 … 或 •）。修改协议或复制配置后，请重新输入真实密钥"
        )))),
        None => Ok(()),
    }
}

/// 更新渠道时，把客户端原样带回的掩码还原成数据库里的凭据。
///
/// 新字符串表示替换；端点传 `null` 仍表示改为继承渠道默认值。这样管理端既看
/// 不到明文，也不会因为编辑了无关字段就把掩码误存成真实密钥。
fn restore_unchanged_credentials(existing: &Channel, incoming: &mut Channel) {
    if incoming.credential.expose() == existing.credential.masked() {
        incoming.credential = existing.credential.clone();
    }

    // 池子按行脱敏，前端可能重排/增删行，掩码按「掩码串 → 旧明文」匹配，
    // 不依赖索引位置；主密钥也参与匹配（它同样可能被挪进池子）。
    let mut restore_from: Vec<Credential> = Vec::with_capacity(existing.credentials.len() + 1);
    if !existing.credential.is_empty() {
        restore_from.push(existing.credential.clone());
    }
    restore_from.extend(existing.credentials.iter().cloned());
    for incoming_credential in &mut incoming.credentials {
        let exposed = incoming_credential.expose();
        if !looks_masked(exposed) {
            continue;
        }
        if let Some(found) = restore_from
            .iter()
            .find(|candidate| candidate.masked() == exposed)
        {
            *incoming_credential = found.clone();
        }
    }
    for endpoint in &mut incoming.endpoints {
        let Some(incoming_credential) = &mut endpoint.credential else {
            continue;
        };
        let Some(existing_credential) = existing
            .endpoints
            .iter()
            .find(|candidate| candidate.protocol == endpoint.protocol)
            .and_then(|candidate| candidate.credential.as_ref())
        else {
            continue;
        };
        if incoming_credential.expose() == existing_credential.masked() {
            *incoming_credential = existing_credential.clone();
        }
    }
}

/// 装配管理路由。路径相对 `/api`（由外层 `App::at("/api", admin::nest())` 挂载）。
pub fn nest() -> NestApp<AppState> {
    App::new()
        .at(
            "/crypto/public-key",
            get(handler_service(crypto_public_key)),
        )
        .at("/auth/login", post(handler_service(auth_login)))
        .at("/auth/logout", post(handler_service(auth_logout)))
        .at("/auth/session", get(handler_service(auth_session)))
        .at(
            "/channels",
            get(handler_service(list_channels)).post(handler_service(create_channel)),
        )
        .at("/channels/bulk", post(handler_service(bulk_channels)))
        .at(
            "/channels/probe-direct",
            post(handler_service(probe_direct)),
        )
        .at(
            "/channels/{id}",
            get(handler_service(get_channel))
                .put(handler_service(update_channel))
                .delete(handler_service(delete_channel)),
        )
        .at(
            "/channels/{id}/enabled",
            post(handler_service(toggle_channel)),
        )
        .at("/channels/{id}/probe", post(handler_service(probe_channel)))
        .at("/channels/{id}/test", post(handler_service(test_channel)))
        .at(
            "/channels/{id}/duplicate",
            post(handler_service(duplicate_channel)),
        )
        .at(
            "/channels/{id}/balance",
            post(handler_service(get_channel_balance)),
        )
        .at(
            "/keys",
            get(handler_service(list_keys)).post(handler_service(create_key)),
        )
        .at(
            "/keys/{id}",
            put(handler_service(update_key)).delete(handler_service(delete_key)),
        )
        .at("/keys/{id}/enabled", post(handler_service(toggle_key)))
        .at(
            "/keys/{id}/reset-usage",
            post(handler_service(reset_key_usage)),
        )
        .at("/data/stats", get(handler_service(data_stats)))
        .at("/data/backup", get(handler_service(data_backup)))
        .at("/logs", get(handler_service(query_logs)))
        .at("/logs/prune", post(handler_service(prune_logs)))
        .at("/logs/export", get(handler_service(export_logs)))
        .at("/logs/{id}", get(handler_service(get_log)))
        .at("/stats", get(handler_service(stats_summary)))
        .at("/stats/models", get(handler_service(stats_by_model)))
        .at("/stats/channels", get(handler_service(stats_by_channel)))
        .at("/stats/timeseries", get(handler_service(stats_timeseries)))
        .at("/stats/keys", get(handler_service(stats_by_key)))
        .at(
            "/settings/routing",
            get(handler_service(get_policy)).put(handler_service(set_policy)),
        )
        .at(
            "/settings/log-retention",
            get(handler_service(get_retention)).put(handler_service(set_retention)),
        )
        .at(
            "/settings/breaker",
            get(handler_service(get_breaker)).put(handler_service(set_breaker)),
        )
        .at(
            "/settings/pricing",
            get(handler_service(get_pricing)).put(handler_service(set_pricing)),
        )
        .at(
            "/settings/log-bodies",
            get(handler_service(get_log_bodies)).put(handler_service(set_log_bodies)),
        )
        .at(
            "/settings/limits",
            get(handler_service(get_limits)).put(handler_service(set_limits)),
        )
        .at(
            "/settings/empty-response-retry",
            get(handler_service(get_empty_response_retry))
                .put(handler_service(set_empty_response_retry)),
        )
        .at(
            "/settings/notify",
            get(handler_service(get_notify)).put(handler_service(set_notify)),
        )
        .at("/settings/notify/test", post(handler_service(test_notify)))
        .at(
            "/settings/affinity",
            get(handler_service(get_affinity)).put(handler_service(set_affinity)),
        )
        .at(
            "/settings/affinity/clear",
            post(handler_service(clear_affinity)),
        )
        .at(
            "/settings/affinity/stats",
            get(handler_service(stats_affinity)),
        )
        .at(
            "/settings/admin-token",
            put(handler_service(set_admin_token)),
        )
        .at(
            "/settings/ip-limits",
            get(handler_service(get_ip_limits)).put(handler_service(set_ip_limits)),
        )
        .at(
            "/settings/webhook-secret",
            get(handler_service(get_webhook_secret)).put(handler_service(set_webhook_secret)),
        )
        .at(
            "/settings/backup",
            get(handler_service(get_backup_settings)).put(handler_service(set_backup_settings)),
        )
        .at(
            "/settings/master-key",
            get(handler_service(get_master_key)).put(handler_service(set_master_key)),
        )
        .at("/health/channels", get(handler_service(health_all)))
        .at(
            "/health/channels/{id}/{protocol}/reset",
            post(handler_service(health_reset)),
        )
        .at("/export", get(handler_service(export_config)))
        .at("/import", post(handler_service(import_config)))
        .at(
            "/backups",
            get(handler_service(list_backups)).post(handler_service(run_backup)),
        )
        .at(
            "/backups/{name}",
            get(handler_service(download_backup)).delete(handler_service(delete_backup)),
        )
        .at("/playground/chat", post(handler_service(playground_chat)))
        .at("/models", get(handler_service(list_models)))
}

fn json_with_cookie<T: Serialize>(value: T, cookie: String) -> Result<WebResponse, AppError> {
    let mut response = ok(value)?;
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        response
            .headers_mut()
            .insert(xitca_web::http::header::SET_COOKIE, value);
    }
    Ok(response)
}

fn attachment(bytes: Vec<u8>, filename: &str, content_type: &'static str) -> WebResponse {
    let mut response = WebResponse::new(ResponseBody::bytes(bytes));
    *response.status_mut() = StatusCode::OK;
    response
        .headers_mut()
        .insert("content-type", HeaderValue::from_static(content_type));
    if let Ok(disposition) = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
    {
        response
            .headers_mut()
            .insert("content-disposition", disposition);
    }
    response
}

async fn crypto_public_key(
    StateRef(state): StateRef<'_, AppState>,
) -> Result<WebResponse, AppError> {
    ok(state.transport_crypto().public_key_response())
}

#[derive(Debug, Deserialize)]
struct LoginBody {
    token: String,
}

async fn auth_login(
    StateRef(state): StateRef<'_, AppState>,
    AdminJson(body): AdminJson<LoginBody>,
    headers: &HeaderMap,
    peer: std::net::SocketAddr,
) -> Result<WebResponse, AppError> {
    let client_ip = peer.ip();

    if let Some(wait_secs) = state.admin_guard().check_locked(client_ip) {
        let err = GatewayError::new(
            refract_core::ErrorKind::PermissionDenied,
            format!("too many failed login attempts, locked for {wait_secs}s"),
        )
        .with_retry_after(std::time::Duration::from_secs(wait_secs));
        return Err(AppError::Admin(err));
    }

    let expected: Option<String> = state
        .settings_repo()
        .get(refract_store::settings_repo::KEY_ADMIN_TOKEN_HASH)
        .await
        .map_err(reject)?;
    let Some(expected_hash) = expected.filter(|hash| !hash.trim().is_empty()) else {
        return ok(serde_json::json!({
            "authenticated": true,
            "username": "admin@localhost"
        }));
    };

    let token_hash = refract_store::ApiKeyRepo::hash(body.token.trim());
    if crate::auth::constant_time_eq(token_hash.as_bytes(), expected_hash.as_bytes()) {
        state.admin_guard().record_success(client_ip);
        let ticket = crate::auth::create_session_ticket(
            state.session_secret(),
            &expected_hash,
            crate::auth::SESSION_MAX_AGE_SECS,
        );
        let cookie = crate::auth::session_cookie(
            &ticket,
            crate::auth::SESSION_MAX_AGE_SECS,
            crate::auth::request_is_https(headers),
        );
        return json_with_cookie(
            serde_json::json!({
                "authenticated": true,
                "username": "admin@localhost"
            }),
            cookie,
        );
    }

    let _locked = state.admin_guard().record_failure(client_ip);
    if let Some(wait_secs) = state.admin_guard().check_locked(client_ip) {
        let err = GatewayError::new(
            refract_core::ErrorKind::PermissionDenied,
            format!("too many failed login attempts, locked for {wait_secs}s"),
        )
        .with_retry_after(std::time::Duration::from_secs(wait_secs));
        return Err(AppError::Admin(err));
    }
    Err(AppError::Admin(GatewayError::new(
        refract_core::ErrorKind::Unauthenticated,
        "invalid admin token",
    )))
}

async fn auth_logout(headers: &HeaderMap) -> Result<WebResponse, AppError> {
    let cookie = crate::auth::session_cookie("", 0, crate::auth::request_is_https(headers));
    json_with_cookie(serde_json::json!({ "authenticated": false }), cookie)
}

async fn auth_session(
    StateRef(state): StateRef<'_, AppState>,
    headers: &HeaderMap,
) -> Result<WebResponse, AppError> {
    let expected: Option<String> = state
        .settings_repo()
        .get(refract_store::settings_repo::KEY_ADMIN_TOKEN_HASH)
        .await
        .map_err(reject)?;
    let Some(_expected_hash) = expected.filter(|hash| !hash.trim().is_empty()) else {
        return ok(serde_json::json!({
            "authenticated": true,
            "configured": false,
            "username": "admin@localhost"
        }));
    };

    if require_admin(state, headers, None).await.is_ok() {
        return ok(serde_json::json!({
            "authenticated": true,
            "configured": true,
            "username": "admin@localhost"
        }));
    }
    ok(serde_json::json!({
        "authenticated": false,
        "configured": true,
        "username": null
    }))
}

async fn list_channels(StateRef(state): StateRef<'_, AppState>) -> Result<WebResponse, AppError> {
    let items = state
        .channel_repo()
        .list(refract_core::DEFAULT_OWNER_ID)
        .await
        .map_err(reject)?;
    ok(items.into_iter().map(redact_channel).collect::<Vec<_>>())
}

async fn create_channel(
    StateRef(state): StateRef<'_, AppState>,
    AdminJson(mut channel): AdminJson<Channel>,
) -> Result<WebResponse, AppError> {
    channel.owner_id = refract_core::DEFAULT_OWNER_ID;
    channel.id = 0;
    reject_masked_credentials(&channel)?;
    validate(&channel)?;
    let created = state
        .channel_repo()
        .create(&channel)
        .await
        .map_err(reject)?;
    commit_channels(state).await?;
    ok(redact_channel(created))
}

async fn get_channel(
    StateRef(state): StateRef<'_, AppState>,
    Params(id): Params<ChannelId>,
) -> Result<WebResponse, AppError> {
    let channel = state
        .channel_repo()
        .get(refract_core::DEFAULT_OWNER_ID, id)
        .await
        .map_err(reject)?;
    ok(redact_channel(channel))
}

async fn update_channel(
    StateRef(state): StateRef<'_, AppState>,
    Params(id): Params<ChannelId>,
    AdminJson(mut channel): AdminJson<Channel>,
) -> Result<WebResponse, AppError> {
    channel.id = id;
    channel.owner_id = refract_core::DEFAULT_OWNER_ID;
    let existing = state
        .channel_repo()
        .get(refract_core::DEFAULT_OWNER_ID, id)
        .await
        .map_err(reject)?;
    restore_unchanged_credentials(&existing, &mut channel);
    reject_masked_credentials(&channel)?;
    validate(&channel)?;
    let saved = state
        .channel_repo()
        .update(&channel)
        .await
        .map_err(reject)?;
    commit_channels(state).await?;
    ok(redact_channel(saved))
}

async fn delete_channel(
    StateRef(state): StateRef<'_, AppState>,
    Params(id): Params<ChannelId>,
) -> Result<WebResponse, AppError> {
    state
        .channel_repo()
        .delete(refract_core::DEFAULT_OWNER_ID, id)
        .await
        .map_err(reject)?;
    commit_channels(state).await?;
    ok(serde_json::json!({ "deleted": id }))
}

#[derive(Debug, Deserialize)]
struct EnabledBody {
    enabled: bool,
}

async fn toggle_channel(
    StateRef(state): StateRef<'_, AppState>,
    Params(id): Params<ChannelId>,
    AdminJson(body): AdminJson<EnabledBody>,
) -> Result<WebResponse, AppError> {
    state
        .channel_repo()
        .set_enabled(refract_core::DEFAULT_OWNER_ID, id, body.enabled)
        .await
        .map_err(reject)?;
    commit_channels(state).await?;
    ok(serde_json::json!({ "id": id, "enabled": body.enabled }))
}

async fn probe_channel(
    StateRef(state): StateRef<'_, AppState>,
    Params(id): Params<ChannelId>,
    AdminJson(req): AdminJson<EndpointRef>,
) -> Result<WebResponse, AppError> {
    let channel = state
        .channel_repo()
        .get(refract_core::DEFAULT_OWNER_ID, id)
        .await
        .map_err(reject)?;
    let endpoint = pick_endpoint(&channel, req.protocol)?;
    let models = refract_upstream::probe_models(
        state.upstream(),
        endpoint.protocol,
        channel.effective_address(endpoint),
        channel.effective_credential(endpoint),
        channel.proxy.as_deref(),
    )
    .await
    .map_err(AppError::Admin)?;
    ok(serde_json::json!({ "models": models }))
}

async fn probe_direct(
    StateRef(state): StateRef<'_, AppState>,
    AdminJson(body): AdminJson<DirectProbeRequest>,
) -> Result<WebResponse, AppError> {
    let address = body.address.unwrap_or_default();
    let credential = body.credential.unwrap_or_default();
    let models = refract_upstream::probe_models(
        state.upstream(),
        body.protocol,
        &address,
        &credential,
        body.proxy.as_deref(),
    )
    .await
    .map_err(AppError::Admin)?;
    ok(serde_json::json!({ "models": models }))
}

async fn test_channel(
    StateRef(state): StateRef<'_, AppState>,
    Params(id): Params<ChannelId>,
    Body(mut body): Body<RequestBody>,
) -> Result<WebResponse, AppError> {
    let raw = collect_limited(&mut body, crate::extract::ADMIN_JSON_LIMIT)
        .await
        .unwrap_or_default();
    let req: TestRequest = decode_admin_json(state, &raw).unwrap_or_default();
    let channel = state
        .channel_repo()
        .get(refract_core::DEFAULT_OWNER_ID, id)
        .await
        .map_err(reject)?;
    ok(run_channel_test(state, &channel, req).await)
}

async fn duplicate_channel(
    StateRef(state): StateRef<'_, AppState>,
    Params(id): Params<ChannelId>,
) -> Result<WebResponse, AppError> {
    let mut channel = state
        .channel_repo()
        .get(refract_core::DEFAULT_OWNER_ID, id)
        .await
        .map_err(reject)?;
    channel.id = 0;
    channel.name = format!("{} 副本", channel.name);
    channel.enabled = false;
    let created = state
        .channel_repo()
        .create(&channel)
        .await
        .map_err(reject)?;
    commit_channels(state).await?;
    ok(redact_channel(created))
}

async fn get_channel_balance(
    StateRef(state): StateRef<'_, AppState>,
    Params(id): Params<ChannelId>,
) -> Result<WebResponse, AppError> {
    let channel = state
        .channel_repo()
        .get(refract_core::DEFAULT_OWNER_ID, id)
        .await
        .map_err(reject)?;
    let endpoint = channel
        .endpoints
        .iter()
        .find(|ep| ep.enabled && matches!(ep.protocol, Protocol::Chat | Protocol::Responses))
        .ok_or_else(|| {
            reject(refract_store::StoreError::Invalid(
                "balance probing needs an enabled chat/responses endpoint".into(),
            ))
        })?;
    let amount = refract_upstream::probe_balance(
        state.upstream(),
        endpoint.protocol,
        channel.effective_address(endpoint),
        channel.effective_credential(endpoint),
        channel.proxy.as_deref(),
    )
    .await
    .map_err(AppError::Admin)?;
    state
        .channel_repo()
        .set_balance(refract_core::DEFAULT_OWNER_ID, id, amount)
        .await
        .map_err(reject)?;
    commit_channels(state).await?;
    ok(serde_json::json!({ "id": id, "balance": amount }))
}

async fn bulk_channels(
    StateRef(state): StateRef<'_, AppState>,
    AdminJson(body): AdminJson<BulkRequest>,
) -> Result<WebResponse, AppError> {
    let repo = state.channel_repo();
    let owner = refract_core::DEFAULT_OWNER_ID;
    let affected = match body.action {
        BulkAction::Enable => repo.set_enabled_many(owner, &body.ids, true).await,
        BulkAction::Disable => repo.set_enabled_many(owner, &body.ids, false).await,
        BulkAction::Delete => repo.delete_many(owner, &body.ids).await,
    }
    .map_err(reject)?;
    commit_channels(state).await?;
    ok(serde_json::json!({ "affected": affected }))
}

async fn list_keys(StateRef(state): StateRef<'_, AppState>) -> Result<WebResponse, AppError> {
    let items = state
        .key_repo()
        .list(refract_core::DEFAULT_OWNER_ID)
        .await
        .map_err(reject)?;
    ok(items)
}

async fn create_key(
    StateRef(state): StateRef<'_, AppState>,
    AdminJson(spec): AdminJson<refract_store::NewApiKey>,
) -> Result<WebResponse, AppError> {
    let (key, plaintext) = state
        .key_repo()
        .create(refract_core::DEFAULT_OWNER_ID, spec)
        .await
        .map_err(reject)?;
    ok(serde_json::json!({ "key": key, "plaintext": plaintext }))
}

async fn update_key(
    StateRef(state): StateRef<'_, AppState>,
    Params(id): Params<i64>,
    AdminJson(spec): AdminJson<refract_store::NewApiKey>,
) -> Result<WebResponse, AppError> {
    let key = state
        .key_repo()
        .update(refract_core::DEFAULT_OWNER_ID, id, &spec)
        .await
        .map_err(reject)?;
    ok(key)
}

async fn toggle_key(
    StateRef(state): StateRef<'_, AppState>,
    Params(id): Params<i64>,
    AdminJson(body): AdminJson<EnabledBody>,
) -> Result<WebResponse, AppError> {
    state
        .key_repo()
        .set_enabled(refract_core::DEFAULT_OWNER_ID, id, body.enabled)
        .await
        .map_err(reject)?;
    ok(serde_json::json!({ "id": id, "enabled": body.enabled }))
}

async fn reset_key_usage(
    StateRef(state): StateRef<'_, AppState>,
    Params(id): Params<i64>,
) -> Result<WebResponse, AppError> {
    state
        .key_repo()
        .reset_usage(refract_core::DEFAULT_OWNER_ID, id)
        .await
        .map_err(reject)?;
    ok(serde_json::json!({ "id": id, "used_quota": 0 }))
}

async fn delete_key(
    StateRef(state): StateRef<'_, AppState>,
    Params(id): Params<i64>,
) -> Result<WebResponse, AppError> {
    state
        .key_repo()
        .delete(refract_core::DEFAULT_OWNER_ID, id)
        .await
        .map_err(reject)?;
    ok(serde_json::json!({ "deleted": id }))
}

async fn data_stats(StateRef(state): StateRef<'_, AppState>) -> Result<WebResponse, AppError> {
    let (db_bytes, log_rows, oldest) = state.db().stats().await.map_err(reject)?;
    ok(serde_json::json!({
        "db_bytes": db_bytes,
        "log_rows": log_rows,
        "oldest_log_at": oldest,
    }))
}

async fn data_backup(StateRef(state): StateRef<'_, AppState>) -> Result<WebResponse, AppError> {
    const MAX_INLINE_BACKUP: u64 = 512 * 1024 * 1024;
    let target = std::env::temp_dir().join(format!(
        "refract-backup-{}-{}.db",
        chrono::Utc::now().format("%Y%m%d-%H%M%S"),
        uuid::Uuid::new_v4().simple()
    ));
    if let Err(error) = state.db().vacuum_into(&target).await {
        let _ = tokio::fs::remove_file(&target).await;
        return Err(reject(error));
    }
    let metadata = tokio::fs::metadata(&target).await.ok();
    if metadata.is_none_or(|item| item.len() > MAX_INLINE_BACKUP) {
        let _ = tokio::fs::remove_file(&target).await;
        return Err(reject(refract_store::StoreError::Invalid(
            "database exceeds 512 MB — back it up offline (see OPERATIONS.md)".into(),
        )));
    }
    let bytes = tokio::fs::read(&target)
        .await
        .map_err(|error| AppError::Admin(GatewayError::internal(error.to_string())))?;
    let _ = tokio::fs::remove_file(&target).await;
    let filename = format!("refract-{}.db", chrono::Utc::now().format("%Y%m%d-%H%M%S"));
    Ok(attachment(bytes, &filename, "application/vnd.sqlite3"))
}

async fn query_logs(
    StateRef(state): StateRef<'_, AppState>,
    Query(filter): Query<LogFilter>,
) -> Result<WebResponse, AppError> {
    let items = state
        .log_repo()
        .query(refract_core::DEFAULT_OWNER_ID, &filter)
        .await
        .map_err(reject)?;
    ok(items)
}

async fn prune_logs(
    StateRef(state): StateRef<'_, AppState>,
    AdminJson(body): AdminJson<PruneBody>,
) -> Result<WebResponse, AppError> {
    let removed = state.log_repo().prune(body.days).await.map_err(reject)?;
    ok(serde_json::json!({ "removed": removed }))
}

async fn export_logs(
    StateRef(state): StateRef<'_, AppState>,
    Query(mut filter): Query<LogFilter>,
) -> Result<WebResponse, AppError> {
    filter.limit = Some(filter.limit.unwrap_or(50_000).min(50_000));
    filter.offset = None;
    let items = state
        .log_repo()
        .query(refract_core::DEFAULT_OWNER_ID, &filter)
        .await
        .map_err(reject)?;
    let mut body = String::with_capacity(items.len() * 256);
    for item in &items {
        if let Ok(line) = serde_json::to_string(item) {
            body.push_str(&line);
            body.push('\n');
        }
    }
    Ok(attachment(
        body.into_bytes(),
        "refract-logs.ndjson",
        "application/x-ndjson; charset=utf-8",
    ))
}

async fn get_log(
    StateRef(state): StateRef<'_, AppState>,
    Params(id): Params<i64>,
) -> Result<WebResponse, AppError> {
    let log = state
        .log_repo()
        .get(refract_core::DEFAULT_OWNER_ID, id)
        .await
        .map_err(reject)?;
    ok(log)
}

async fn stats_summary(
    StateRef(state): StateRef<'_, AppState>,
    Query(query): Query<StatsQuery>,
) -> Result<WebResponse, AppError> {
    ok(state
        .log_repo()
        .summary(refract_core::DEFAULT_OWNER_ID, query.hours)
        .await
        .map_err(reject)?)
}

async fn stats_by_model(
    StateRef(state): StateRef<'_, AppState>,
    Query(query): Query<StatsQuery>,
) -> Result<WebResponse, AppError> {
    ok(state
        .log_repo()
        .by_model(refract_core::DEFAULT_OWNER_ID, query.hours, 50)
        .await
        .map_err(reject)?)
}

async fn stats_by_channel(
    StateRef(state): StateRef<'_, AppState>,
    Query(query): Query<StatsQuery>,
) -> Result<WebResponse, AppError> {
    ok(state
        .log_repo()
        .by_channel(refract_core::DEFAULT_OWNER_ID, query.hours)
        .await
        .map_err(reject)?)
}

async fn stats_timeseries(
    StateRef(state): StateRef<'_, AppState>,
    Query(query): Query<TimeseriesQuery>,
) -> Result<WebResponse, AppError> {
    ok(state
        .log_repo()
        .timeseries(
            refract_core::DEFAULT_OWNER_ID,
            query.hours,
            query.bucket == "day",
        )
        .await
        .map_err(reject)?)
}

async fn stats_by_key(
    StateRef(state): StateRef<'_, AppState>,
    Query(query): Query<StatsQuery>,
) -> Result<WebResponse, AppError> {
    ok(state
        .log_repo()
        .by_key(refract_core::DEFAULT_OWNER_ID, query.hours)
        .await
        .map_err(reject)?)
}

async fn get_policy(StateRef(state): StateRef<'_, AppState>) -> Result<WebResponse, AppError> {
    ok(state.policy())
}

async fn set_policy(
    StateRef(state): StateRef<'_, AppState>,
    AdminJson(policy): AdminJson<RoutingPolicy>,
) -> Result<WebResponse, AppError> {
    state
        .settings_repo()
        .set_routing_policy(&policy)
        .await
        .map_err(reject)?;
    state.reload_policy().await.map_err(reject)?;
    ok(policy)
}

async fn get_retention(StateRef(state): StateRef<'_, AppState>) -> Result<WebResponse, AppError> {
    let days = state.settings_repo().log_retention_days().await;
    ok(LogRetentionBody { days })
}

async fn set_retention(
    StateRef(state): StateRef<'_, AppState>,
    AdminJson(body): AdminJson<LogRetentionBody>,
) -> Result<WebResponse, AppError> {
    state
        .settings_repo()
        .set_log_retention_days(body.days)
        .await
        .map_err(reject)?;
    ok(body)
}

async fn get_breaker(StateRef(state): StateRef<'_, AppState>) -> Result<WebResponse, AppError> {
    ok(state
        .settings_repo()
        .breaker_policy()
        .await
        .map_err(reject)?)
}

async fn set_breaker(
    StateRef(state): StateRef<'_, AppState>,
    AdminJson(policy): AdminJson<refract_store::BreakerPolicy>,
) -> Result<WebResponse, AppError> {
    state
        .settings_repo()
        .set_breaker_policy(&policy)
        .await
        .map_err(reject)?;
    state.reload_breaker().await.map_err(reject)?;
    ok(policy)
}

async fn get_pricing(StateRef(state): StateRef<'_, AppState>) -> Result<WebResponse, AppError> {
    ok(state.settings_repo().pricing().await.map_err(reject)?)
}

async fn set_pricing(
    StateRef(state): StateRef<'_, AppState>,
    AdminJson(pricing): AdminJson<Vec<refract_store::ModelPrice>>,
) -> Result<WebResponse, AppError> {
    state
        .settings_repo()
        .set_pricing(&pricing)
        .await
        .map_err(reject)?;
    state.reload_pricing().await.map_err(reject)?;
    ok(pricing)
}

async fn get_log_bodies(StateRef(state): StateRef<'_, AppState>) -> Result<WebResponse, AppError> {
    let enabled = state
        .settings_repo()
        .capture_bodies()
        .await
        .map_err(reject)?;
    ok(serde_json::json!({ "enabled": enabled }))
}

async fn set_log_bodies(
    StateRef(state): StateRef<'_, AppState>,
    AdminJson(body): AdminJson<LogBodiesBody>,
) -> Result<WebResponse, AppError> {
    state
        .settings_repo()
        .set_capture_bodies(body.enabled)
        .await
        .map_err(reject)?;
    state.reload_capture_bodies().await.map_err(reject)?;
    ok(serde_json::json!({ "enabled": body.enabled }))
}

async fn get_limits(StateRef(state): StateRef<'_, AppState>) -> Result<WebResponse, AppError> {
    ok(state
        .settings_repo()
        .global_limits()
        .await
        .map_err(reject)?)
}

async fn set_limits(
    StateRef(state): StateRef<'_, AppState>,
    AdminJson(limits): AdminJson<refract_store::GlobalLimits>,
) -> Result<WebResponse, AppError> {
    state
        .settings_repo()
        .set_global_limits(&limits)
        .await
        .map_err(reject)?;
    state.reload_global_limits().await.map_err(reject)?;
    ok(limits)
}

async fn get_empty_response_retry(
    StateRef(state): StateRef<'_, AppState>,
) -> Result<WebResponse, AppError> {
    ok(state.empty_response_retry())
}

async fn set_empty_response_retry(
    StateRef(state): StateRef<'_, AppState>,
    AdminJson(policy): AdminJson<refract_core::EmptyResponseRetryPolicy>,
) -> Result<WebResponse, AppError> {
    state
        .settings_repo()
        .set_empty_response_retry(policy)
        .await
        .map_err(reject)?;
    state.reload_empty_response_retry().await.map_err(reject)?;
    ok(policy)
}

async fn get_notify(StateRef(state): StateRef<'_, AppState>) -> Result<WebResponse, AppError> {
    let webhook_url = state.settings_repo().webhook_url().await.map_err(reject)?;
    let retest_minutes = state.settings_repo().retest_minutes().await;
    ok(serde_json::json!({
        "webhook_url": webhook_url,
        "retest_minutes": retest_minutes,
    }))
}

async fn set_notify(
    StateRef(state): StateRef<'_, AppState>,
    AdminJson(body): AdminJson<NotifyBody>,
) -> Result<WebResponse, AppError> {
    state
        .settings_repo()
        .set_notify(body.webhook_url.as_deref(), body.retest_minutes)
        .await
        .map_err(reject)?;
    state.reload_webhook().await.map_err(reject)?;
    ok(body)
}

async fn test_notify(StateRef(state): StateRef<'_, AppState>) -> Result<WebResponse, AppError> {
    let Some(url) = state.webhook_url() else {
        return Err(reject(refract_store::StoreError::Invalid(
            "webhook url is not configured — save it first".into(),
        )));
    };
    let secret = state.webhook_secret();
    crate::notify::send_webhook(
        &url,
        "notify.test",
        "refract",
        None,
        "这是一条测试通知；收到即表示 webhook 配置正确",
        secret.as_deref(),
    )
    .await;
    ok(serde_json::json!({ "sent": true }))
}

async fn get_affinity(StateRef(state): StateRef<'_, AppState>) -> Result<WebResponse, AppError> {
    ok(state.settings_repo().affinity().await.map_err(reject)?)
}

async fn set_affinity(
    StateRef(state): StateRef<'_, AppState>,
    AdminJson(policy): AdminJson<refract_core::AffinitySettings>,
) -> Result<WebResponse, AppError> {
    state
        .settings_repo()
        .set_affinity(&policy)
        .await
        .map_err(reject)?;
    state.reload_affinity().await.map_err(reject)?;
    ok(policy)
}

async fn clear_affinity(StateRef(state): StateRef<'_, AppState>) -> Result<WebResponse, AppError> {
    let cleared = state.affinity().clear();
    ok(serde_json::json!({ "cleared": cleared }))
}

async fn stats_affinity(StateRef(state): StateRef<'_, AppState>) -> Result<WebResponse, AppError> {
    ok(serde_json::json!({
        "active": state.affinity().is_active(),
        "stats": state.affinity().stats(),
    }))
}

async fn set_admin_token(
    StateRef(state): StateRef<'_, AppState>,
    AdminJson(body): AdminJson<AdminTokenBody>,
    headers: &HeaderMap,
) -> Result<WebResponse, AppError> {
    let repo = state.settings_repo();
    let secure = crate::auth::request_is_https(headers);
    match body.token.filter(|token| !token.trim().is_empty()) {
        Some(token) => {
            let hash = refract_store::ApiKeyRepo::hash(&token);
            repo.set_admin_token(Some(&hash)).await.map_err(reject)?;
            let ticket = crate::auth::create_session_ticket(
                state.session_secret(),
                &hash,
                crate::auth::SESSION_MAX_AGE_SECS,
            );
            let cookie =
                crate::auth::session_cookie(&ticket, crate::auth::SESSION_MAX_AGE_SECS, secure);
            json_with_cookie(serde_json::json!({ "configured": true }), cookie)
        }
        None => {
            repo.set_admin_token(None).await.map_err(reject)?;
            let cookie = crate::auth::session_cookie("", 0, secure);
            json_with_cookie(serde_json::json!({ "configured": false }), cookie)
        }
    }
}

async fn get_ip_limits(StateRef(state): StateRef<'_, AppState>) -> Result<WebResponse, AppError> {
    ok(state.settings_repo().ip_limits().await.map_err(reject)?)
}

async fn set_ip_limits(
    StateRef(state): StateRef<'_, AppState>,
    AdminJson(limits): AdminJson<refract_store::IpLimits>,
) -> Result<WebResponse, AppError> {
    state
        .settings_repo()
        .set_ip_limits(&limits)
        .await
        .map_err(reject)?;
    state.reload_ip_limits().await.map_err(reject)?;
    ok(limits)
}

async fn get_webhook_secret(
    StateRef(state): StateRef<'_, AppState>,
) -> Result<WebResponse, AppError> {
    let secret = state
        .settings_repo()
        .webhook_secret()
        .await
        .map_err(reject)?;
    ok(serde_json::json!({ "configured": secret.is_some_and(|item| !item.is_empty()) }))
}

async fn set_webhook_secret(
    StateRef(state): StateRef<'_, AppState>,
    AdminJson(body): AdminJson<WebhookSecretBody>,
) -> Result<WebResponse, AppError> {
    state
        .settings_repo()
        .set_webhook_secret(body.secret.as_deref())
        .await
        .map_err(reject)?;
    state.reload_webhook_secret().await.map_err(reject)?;
    ok(serde_json::json!({ "configured": body.secret.is_some_and(|item| !item.is_empty()) }))
}

async fn get_backup_settings(
    StateRef(state): StateRef<'_, AppState>,
) -> Result<WebResponse, AppError> {
    ok(state
        .settings_repo()
        .backup_settings()
        .await
        .map_err(reject)?)
}

async fn set_backup_settings(
    StateRef(state): StateRef<'_, AppState>,
    AdminJson(settings): AdminJson<refract_store::BackupSettings>,
) -> Result<WebResponse, AppError> {
    state
        .settings_repo()
        .set_backup_settings(&settings)
        .await
        .map_err(reject)?;
    state.reload_backup().await.map_err(reject)?;
    ok(settings)
}

async fn get_master_key(StateRef(state): StateRef<'_, AppState>) -> Result<WebResponse, AppError> {
    ok(serde_json::json!({ "configured": state.master_key().is_some() }))
}

async fn set_master_key(
    StateRef(state): StateRef<'_, AppState>,
    AdminJson(body): AdminJson<MasterKeyBody>,
) -> Result<WebResponse, AppError> {
    match body.key.filter(|key| !key.trim().is_empty()) {
        Some(key) => {
            refract_store::parse_master_key(&key).map_err(|error| {
                AppError::Admin(GatewayError::invalid_request(error.to_string()))
            })?;
            state
                .settings_repo()
                .set_master_key(Some(&key))
                .await
                .map_err(reject)?;
            state.reload_master_key().await.map_err(reject)?;
            ok(serde_json::json!({ "configured": true }))
        }
        None => {
            state
                .settings_repo()
                .set_master_key(None)
                .await
                .map_err(reject)?;
            state.reload_master_key().await.map_err(reject)?;
            ok(serde_json::json!({ "configured": false }))
        }
    }
}

async fn health_all(StateRef(state): StateRef<'_, AppState>) -> Result<WebResponse, AppError> {
    ok(state.health_repo().all().await.map_err(reject)?)
}

async fn health_reset(
    StateRef(state): StateRef<'_, AppState>,
    Params((id, protocol)): Params<(ChannelId, Protocol)>,
) -> Result<WebResponse, AppError> {
    state
        .health_repo()
        .reset(id, protocol)
        .await
        .map_err(reject)?;
    ok(serde_json::json!({ "reset": id, "protocol": protocol }))
}

async fn export_config(StateRef(state): StateRef<'_, AppState>) -> Result<WebResponse, AppError> {
    let owner = refract_core::DEFAULT_OWNER_ID;
    let channels = state.channel_repo().list(owner).await.map_err(reject)?;
    let keys = state.key_repo().export(owner).await.map_err(reject)?;
    ok(ExportDocument {
        version: EXPORT_VERSION,
        exported_at: Some(chrono::Utc::now().to_rfc3339()),
        channels,
        keys,
        settings: ExportedSettings {
            routing_policy: state.policy(),
            log_retention_days: state.settings_repo().log_retention_days().await,
            breaker_policy: state
                .settings_repo()
                .breaker_policy()
                .await
                .map_err(reject)?,
            pricing: state.settings_repo().pricing().await.map_err(reject)?,
            empty_response_retry: state.empty_response_retry(),
        },
    })
}

async fn import_config(
    StateRef(state): StateRef<'_, AppState>,
    AdminJson(req): AdminJson<ImportRequest>,
) -> Result<WebResponse, AppError> {
    import_document(req, state).await
}

async fn list_backups(StateRef(state): StateRef<'_, AppState>) -> Result<WebResponse, AppError> {
    let dir = crate::backup::resolve_backup_dir(state);
    ok(crate::backup::list_backups(&dir))
}

async fn run_backup(StateRef(state): StateRef<'_, AppState>) -> Result<WebResponse, AppError> {
    let filename = crate::backup::run_backup_once(state)
        .await
        .map_err(reject)?;
    ok(serde_json::json!({ "name": filename }))
}

async fn download_backup(
    StateRef(state): StateRef<'_, AppState>,
    Params(name): Params<String>,
) -> Result<WebResponse, AppError> {
    if !crate::backup::is_valid_backup_name(&name) {
        return Err(AppError::Admin(GatewayError::invalid_request(
            "invalid backup filename",
        )));
    }
    let dir = crate::backup::resolve_backup_dir(state);
    let target = dir.join(&name);
    if !target.is_file() {
        return Err(AppError::Admin(GatewayError::not_found(format!(
            "backup file `{name}` not found"
        ))));
    }
    let bytes = tokio::fs::read(&target).await.map_err(|error| {
        AppError::Admin(GatewayError::internal(format!(
            "failed to read backup file: {error}"
        )))
    })?;
    Ok(attachment(bytes, &name, "application/vnd.sqlite3"))
}

async fn delete_backup(
    StateRef(state): StateRef<'_, AppState>,
    Params(name): Params<String>,
) -> Result<WebResponse, AppError> {
    if !crate::backup::is_valid_backup_name(&name) {
        return Err(AppError::Admin(GatewayError::invalid_request(
            "invalid backup filename",
        )));
    }
    let dir = crate::backup::resolve_backup_dir(state);
    let target = dir.join(&name);
    if !target.is_file() {
        return Err(AppError::Admin(GatewayError::not_found(format!(
            "backup file `{name}` not found"
        ))));
    }
    tokio::fs::remove_file(&target).await.map_err(|error| {
        AppError::Admin(GatewayError::internal(format!(
            "failed to remove backup file: {error}"
        )))
    })?;
    ok(serde_json::json!({ "deleted": true }))
}

async fn playground_chat(
    StateRef(state): StateRef<'_, AppState>,
    Body(mut body): Body<RequestBody>,
) -> Result<WebResponse, AppError> {
    let raw = collect_limited(&mut body, crate::extract::ADMIN_JSON_LIMIT).await?;
    crate::gateway::playground_chat(state.clone(), raw).await
}

async fn list_models(StateRef(state): StateRef<'_, AppState>) -> Result<WebResponse, AppError> {
    let channels = state.channels();
    let mut names: Vec<&str> = channels
        .iter()
        .filter(|channel| channel.enabled)
        .flat_map(|channel| channel.endpoints.iter())
        .filter(|endpoint| endpoint.enabled)
        .flat_map(|endpoint| endpoint.models.iter())
        .map(|model| model.name.as_str())
        .collect();
    names.sort_unstable();
    names.dedup();
    ok(names)
}

/// 直接探测上游模型列表请求体。
#[derive(Debug, Deserialize)]
struct DirectProbeRequest {
    protocol: Protocol,
    #[serde(default)]
    address: Option<UpstreamAddress>,
    #[serde(default)]
    credential: Option<Credential>,
    #[serde(default)]
    proxy: Option<String>,
}

/// 批量操作请求体。
#[derive(Debug, Deserialize)]
struct BulkRequest {
    ids: Vec<ChannelId>,
    action: BulkAction,
}

/// 批量操作类型。
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum BulkAction {
    Enable,
    Disable,
    Delete,
}

/// 指定要操作聚合渠道的哪个端点。
///
/// 省略时用渠道的首选端点 —— 单协议渠道只有一个，聚合渠道取 order 最小的。
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct EndpointRef {
    protocol: Option<Protocol>,
}

/// 连通性测试的请求体。
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct TestRequest {
    /// 指定端点；省略则用首选端点。
    protocol: Option<Protocol>,
    /// 指定模型；省略则用该端点的第一个模型。
    model: Option<String>,
}

/// 选出要操作的端点。
fn pick_endpoint(
    channel: &Channel,
    protocol: Option<Protocol>,
) -> Result<&refract_core::ChannelEndpoint, AppError> {
    match protocol {
        Some(p) => channel
            .endpoints
            .iter()
            .find(|e| e.protocol == p)
            .ok_or_else(|| {
                AppError::Admin(GatewayError::not_found(format!(
                    "channel `{}` has no `{p}` endpoint",
                    channel.name
                )))
            }),
        // 首选端点：order 最小者。这与路由层的选择一致（需求 5），
        // 所以「测试通过」意味着真实流量走的那条路通了。
        None => channel
            .endpoints_by_order()
            .first()
            .copied()
            .ok_or_else(|| {
                AppError::Admin(GatewayError::invalid_request(format!(
                    "channel `{}` has no enabled endpoint",
                    channel.name
                )))
            }),
    }
}

/// 对一个渠道端点发最小真实请求，验证配置可用。
///
/// 不做协议转换：测试的目标是「这个端点的原生协议能不能正常工作」，
/// 转换能力由路由层保证，不在这里重复验证。
pub(crate) async fn run_channel_test(
    state: &AppState,
    channel: &Channel,
    req: TestRequest,
) -> serde_json::Value {
    let endpoint = match pick_endpoint(channel, req.protocol) {
        Ok(ep) => ep,
        Err(_) => {
            return serde_json::json!({
                "success": false,
                "message": "no enabled endpoint to test",
            });
        }
    };

    // 优先级：本次请求指定 > 渠道配置的测试模型 > 端点第一个模型。
    let model = test_upstream_model(
        channel,
        endpoint,
        req.model.as_deref().or(channel.test_model.as_deref()),
    );

    let ir = refract_protocol::UnifiedRequest::new(
        &model,
        vec![refract_protocol::Message::text(
            refract_protocol::Role::User,
            "ping",
        )],
    );

    let mut body = match state
        .codecs()
        .for_protocol(endpoint.protocol)
        .encode_request(&ir)
    {
        Ok(b) => b,
        Err(e) => {
            return serde_json::json!({
                "success": false,
                "message": format!("failed to encode test request: {e}"),
            });
        }
    };

    // Gemini 不允许 body 里有 model 字段。
    if endpoint.protocol == refract_core::Protocol::Gemini
        && let serde_json::Value::Object(map) = &mut body
    {
        map.remove("model");
    }

    let mut req = refract_upstream::UpstreamRequest::post(
        endpoint.protocol,
        channel.effective_address(endpoint),
        channel.effective_credential(endpoint),
        &model,
        refract_core::Action::Generate,
        &body,
    );
    let channel_headers = channel.extra_headers.clone();
    req.extra_headers = &channel_headers;
    req.proxy = channel.proxy.as_deref();
    // 管理端点的同步操作：用探测超时而不是数据面的 300s，
    // 上游挂死时让管理员拿到可读的超时错误，而不是干等。
    req.timeout = Some(refract_upstream::probe::PROBE_TIMEOUT);

    let started = std::time::Instant::now();
    match state.upstream().send(req).await {
        Ok(resp) => {
            let latency_ms = started.elapsed().as_millis() as u64;
            let status = resp.status;
            let success = (200..300).contains(&status);
            if success {
                // 测试通过等于一次真实成功 —— 顺手解除该端点的熔断，
                // 不必等下一个用户请求去当探针。
                let _ = state
                    .health_repo()
                    .record_success(channel.id, endpoint.protocol, latency_ms)
                    .await;
            }
            serde_json::json!({
                "success": success,
                "message": if success {
                    "upstream responded successfully"
                } else {
                    "upstream returned non-2xx"
                },
                "upstream_status": status,
                "latency_ms": latency_ms,
            })
        }
        Err(e) => serde_json::json!({
            "success": false,
            "message": e.to_string(),
            "latency_ms": started.elapsed().as_millis() as u64,
        }),
    }
}

/// 渠道测试必须使用真实上游模型名，而不是对外别名。
///
/// 别名先在被测端点上找；找不到再按路由顺序扫全渠道 —— 聚合渠道里
/// 一个别名可能挂在别的端点上，把它的上游映射拿来才是真实流量会用的名字。
fn test_upstream_model(
    channel: &Channel,
    endpoint: &refract_core::ChannelEndpoint,
    requested: Option<&str>,
) -> String {
    match requested {
        Some(name) => endpoint
            .find_model(name)
            .map(|entry| entry.upstream_name().to_owned())
            .or_else(|| {
                channel
                    .endpoints_by_order()
                    .iter()
                    .find_map(|ep| ep.find_model(name))
                    .map(|entry| entry.upstream_name().to_owned())
            })
            // 允许管理员临时输入一个尚未保存到列表里的真实模型名。
            .unwrap_or_else(|| name.to_owned()),
        None => endpoint
            .models
            .first()
            .map(|entry| entry.upstream_name().to_owned())
            .unwrap_or_else(|| "test".to_owned()),
    }
}

/// 渠道配置的语义校验。
///
/// 这些检查不能只放在前端：前端可以绕过，而一个语义无效的渠道会在**请求时**
/// 才炸 —— 那时错误信息离原因很远。宁可在保存时就拒绝。
fn validate(channel: &Channel) -> Result<(), AppError> {
    let invalid =
        |message: String| -> AppError { AppError::Admin(GatewayError::invalid_request(message)) };

    channel
        .validate()
        .map_err(|error| invalid(error.to_string()))?;

    // 地址是另一组不变量：领域模型负责渠道结构，地址值对象负责 URL 语义。
    for endpoint in &channel.endpoints {
        let model = endpoint
            .models
            .first()
            .map_or("model", refract_core::ModelEntry::upstream_name);
        channel
            .effective_address(endpoint)
            .resolve(endpoint.protocol, Action::Generate, model)
            .map_err(|error| invalid(error.to_string()))?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// 密钥
// ---------------------------------------------------------------------------

/// 日志清理请求体。
#[derive(Debug, Deserialize)]
struct PruneBody {
    days: u32,
}

/// 统计窗口参数。
#[derive(Debug, Deserialize)]
struct StatsQuery {
    /// 统计窗口（小时）。默认 24。
    #[serde(default = "default_hours")]
    hours: u32,
}

fn default_hours() -> u32 {
    24
}

/// 时序统计的查询参数。
#[derive(Debug, Deserialize)]
struct TimeseriesQuery {
    #[serde(default = "default_hours")]
    hours: u32,
    /// `hour`（默认）或 `day`。
    #[serde(default = "default_bucket")]
    bucket: String,
}

fn default_bucket() -> String {
    "hour".into()
}

/// 设置管理令牌的请求体。
#[derive(Debug, Deserialize)]
struct AdminTokenBody {
    /// 新令牌；`null` 或空串表示清除。
    token: Option<String>,
}

/// 正文快照开关的请求体。
#[derive(Debug, Deserialize)]
struct LogBodiesBody {
    enabled: bool,
}

fn default_retest_minutes() -> u32 {
    refract_store::settings_repo::DEFAULT_RETEST_MINUTES
}

#[derive(Debug, Deserialize, Serialize)]
struct LogRetentionBody {
    days: u32,
}

/// 通知与自愈设置的请求体。
#[derive(Debug, Serialize, Deserialize)]
struct NotifyBody {
    /// 告警 webhook 地址；空或 null 表示关闭通知。
    #[serde(default)]
    webhook_url: Option<String>,
    /// 自动禁用渠道的重测间隔（分钟）；0 关闭自愈。
    #[serde(default = "default_retest_minutes")]
    retest_minutes: u32,
}

/// 设置 Webhook 密钥的请求体。
#[derive(Debug, Deserialize)]
struct WebhookSecretBody {
    #[serde(default)]
    secret: Option<String>,
}

/// 设置主加密密钥的请求体。
#[derive(Debug, Deserialize)]
struct MasterKeyBody {
    #[serde(default)]
    key: Option<String>,
}

/// 备份文档的当前版本号。导入时校验，未来格式变更靠它做兼容。
const EXPORT_VERSION: u32 = 1;

/// 一份完整的配置备份。
///
/// 渠道凭据**明文导出** —— 备份的意义就是可恢复；文件的保管责任与数据库
/// 文件本身相同。网关自身的 API 密钥只有哈希，明文从未落库也就无从导出。
#[derive(Debug, Serialize, Deserialize)]
struct ExportDocument {
    version: u32,
    #[serde(default)]
    exported_at: Option<String>,
    #[serde(default)]
    channels: Vec<Channel>,
    #[serde(default)]
    keys: Vec<refract_store::ExportedApiKey>,
    settings: ExportedSettings,
}

/// 备份中的运行时设置。
#[derive(Debug, Serialize, Deserialize)]
struct ExportedSettings {
    routing_policy: RoutingPolicy,
    log_retention_days: u32,
    /// 旧版本备份没有这个字段，缺省回落默认值。
    #[serde(default)]
    breaker_policy: refract_store::BreakerPolicy,
    /// 模型价表。旧版本备份缺省为空。
    #[serde(default)]
    pricing: Vec<refract_store::ModelPrice>,
    /// HTTP 200 空回复重试策略。
    #[serde(default)]
    empty_response_retry: refract_core::EmptyResponseRetryPolicy,
}

/// 导入请求。
#[derive(Debug, Deserialize)]
struct ImportRequest {
    /// `merge`（默认）：按名字/哈希跳过已存在的；`replace`：清空后导入。
    #[serde(default)]
    mode: ImportMode,
    data: ExportDocument,
}

/// 导入模式。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ImportMode {
    #[default]
    Merge,
    Replace,
}

/// 执行导入。
///
/// 先全量校验再写入：一个坏渠道不应该让备份导到一半 —— 那比导入失败更糟，
/// 用户会拿到一个自己都说不清状态的实例。
async fn import_document(req: ImportRequest, state: &AppState) -> Result<WebResponse, AppError> {
    let owner = refract_core::DEFAULT_OWNER_ID;
    if req.data.version != EXPORT_VERSION {
        return Err(AppError::Admin(GatewayError::invalid_request(format!(
            "unsupported backup version {}; this build accepts version {EXPORT_VERSION}",
            req.data.version
        ))));
    }
    for channel in &req.data.channels {
        channel.validate().map_err(|e| {
            AppError::Admin(GatewayError::invalid_request(format!(
                "channel `{}` in the backup is invalid: {e}",
                channel.name
            )))
        })?;
    }

    let channel_repo = state.channel_repo();
    let key_repo = state.key_repo();

    let mut channels_imported = 0_u32;
    // 跳过明细返回名字而不只是数量：用户导完备份最想知道的是「哪些没进来」。
    let mut skipped_channels: Vec<String> = Vec::new();
    let keys_imported;
    let skipped_keys: Vec<String>;

    if req.mode == ImportMode::Replace {
        // replace 模式走仓储的原子替换：删旧 + 插新同一事务。分成两步独立
        // 提交的话，中途失败会留下「渠道被清空但只导入了一半」的实例。
        // 渠道与密钥分属两个事务：跨域仍非严格原子，但每个域内不会半途而废，
        // 且失败后重导（merge 或 replace）都能收敛到完整状态。
        let mut channels = req.data.channels;
        for channel in &mut channels {
            channel.id = 0;
            channel.owner_id = owner;
            // 有人会把管理 API 的 GET 响应（凭据已脱敏）当备份喂回来 ——
            // 那不是可用的配置，必须在覆盖现有数据**之前**拒绝。
            reject_masked_credentials(channel)?;
        }
        channels_imported = channel_repo
            .replace_all(owner, &channels)
            .await
            .map_err(reject)?;
        (keys_imported, skipped_keys) = key_repo
            .replace_all(owner, &req.data.keys)
            .await
            .map_err(reject)?;
    } else {
        // merge 模式按名字判重：同名渠道视为已存在。名字是用户视角的身份，
        // 数据库 id 在两个实例之间没有意义。
        let existing_names: std::collections::HashSet<String> = channel_repo
            .list(owner)
            .await
            .map_err(reject)?
            .into_iter()
            .map(|c| c.name)
            .collect();

        for mut channel in req.data.channels {
            if existing_names.contains(&channel.name) {
                skipped_channels.push(channel.name);
                continue;
            }
            channel.id = 0;
            channel.owner_id = owner;
            reject_masked_credentials(&channel)?;
            channel_repo.create(&channel).await.map_err(reject)?;
            channels_imported += 1;
        }

        let mut imported = 0_u32;
        let mut skipped = Vec::new();
        for key in &req.data.keys {
            if key_repo.restore(owner, key).await.map_err(reject)? {
                imported += 1;
            } else {
                skipped.push(key.name.clone());
            }
        }
        (keys_imported, skipped_keys) = (imported, skipped);
    }

    let settings = state.settings_repo();
    settings
        .import_settings(
            &req.data.settings.routing_policy,
            req.data.settings.log_retention_days,
            &req.data.settings.breaker_policy,
            &req.data.settings.pricing,
            &req.data.settings.empty_response_retry,
        )
        .await
        .map_err(reject)?;

    commit_channels(state).await?;
    state.reload_policy().await.map_err(reject)?;
    state.reload_breaker().await.map_err(reject)?;
    state.reload_pricing().await.map_err(reject)?;
    state.reload_empty_response_retry().await.map_err(reject)?;

    ok(serde_json::json!({
        "channels_imported": channels_imported,
        "channels_skipped": skipped_channels.len(),
        "keys_imported": keys_imported,
        "keys_skipped": skipped_keys.len(),
        "skipped_channels": skipped_channels,
        "skipped_keys": skipped_keys,
    }))
}

// ---------------------------------------------------------------------------
// 健康与模型
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "admin_tests.rs"]
mod tests;
