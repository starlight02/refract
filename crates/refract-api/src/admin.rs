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
        let cookie = format!(
            "{}={ticket}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}",
            crate::auth::SESSION_COOKIE_NAME,
            crate::auth::SESSION_MAX_AGE_SECS
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

async fn auth_logout() -> Result<WebResponse, AppError> {
    let cookie = format!(
        "{}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0",
        crate::auth::SESSION_COOKIE_NAME
    );
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
) -> Result<WebResponse, AppError> {
    let repo = state.settings_repo();
    match body.token.filter(|token| !token.trim().is_empty()) {
        Some(token) => {
            let hash = refract_store::ApiKeyRepo::hash(&token);
            repo.set_admin_token(Some(&hash)).await.map_err(reject)?;
            let ticket = crate::auth::create_session_ticket(
                state.session_secret(),
                &hash,
                crate::auth::SESSION_MAX_AGE_SECS,
            );
            let cookie = format!(
                "{}={ticket}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}",
                crate::auth::SESSION_COOKIE_NAME,
                crate::auth::SESSION_MAX_AGE_SECS
            );
            json_with_cookie(serde_json::json!({ "configured": true }), cookie)
        }
        None => {
            repo.set_admin_token(None).await.map_err(reject)?;
            let cookie = format!(
                "{}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0",
                crate::auth::SESSION_COOKIE_NAME
            );
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
mod tests {
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

        let response =
            crate::http_test::TestRequest::delete(&format!("/api/channels/{}", created.id))
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
        use aes_gcm::aead::{Aead as _, KeyInit as _};
        use aes_gcm::{Aes256Gcm, Nonce};
        use base64::Engine as _;
        use p256::elliptic_curve::rand_core::OsRng;
        use p256::elliptic_curve::sec1::{FromEncodedPoint as _, ToEncodedPoint as _};
        use p256::{EncodedPoint, PublicKey, SecretKey};

        let state = test_state().await;

        // 1. GET /api/crypto/public-key 无需鉴权即可访问
        let pk_res = crate::http_test::TestRequest::get("/api/crypto/public-key")
            .send(state.clone())
            .await;
        assert_eq!(pk_res.status(), 200);

        let pk_json: serde_json::Value = serde_json::from_slice(pk_res.body()).unwrap();
        let server_pub_raw_b64 = pk_json["data"]["public_key_raw"].as_str().unwrap();

        // 2. 模拟前端 Web Crypto: 生成客户端临时密钥对并协商共享密钥
        let client_secret = SecretKey::random(&mut OsRng);
        let client_pub = client_secret.public_key();
        let client_pub_b64 = base64::engine::general_purpose::STANDARD
            .encode(client_pub.to_encoded_point(false).as_bytes());

        let server_pub_bytes = base64::engine::general_purpose::STANDARD
            .decode(server_pub_raw_b64)
            .unwrap();
        let server_point = EncodedPoint::from_bytes(&server_pub_bytes).unwrap();
        let server_pub = PublicKey::from_encoded_point(&server_point).unwrap();

        let client_shared =
            p256::ecdh::diffie_hellman(client_secret.to_nonzero_scalar(), server_pub.as_affine());
        let client_shared_bytes = client_shared.raw_secret_bytes();

        let channel_payload = serde_json::json!({
            "name": "encrypted-e2e-channel",
            "kind": "chat",
            "credential": "sk-secret-from-browser-e2e",
            "endpoints": [{ "protocol": "chat", "models": [{"name": "gpt-4o"}] }]
        });
        let plaintext = serde_json::to_vec(&channel_payload).unwrap();
        let iv_bytes = [42u8; 12];
        let cipher = Aes256Gcm::new_from_slice(client_shared_bytes.as_ref()).unwrap();
        let ciphertext_bytes = cipher
            .encrypt(&Nonce::from(iv_bytes), plaintext.as_slice())
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
}
