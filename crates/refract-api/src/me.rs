//! 自助面 REST API（`/api/me/...`）。
//!
//! 所有处理器通过 [`require_me`] 取当前登录用户，强制 `user_id = self`，
//! 不接受客户端传入的 user_id。密钥与渠道写路径复用 [`crate::admin`] 抽出的
//! 内部实现，避免两套逻辑分叉。

use chrono::{DateTime, Utc};
use refract_core::{Channel, ChannelId, ErrorKind, GatewayError};
use refract_store::{LedgerKind, LogFilter, User, UserStatus};
use serde::Deserialize;
use xitca_web::handler::handler_service;
use xitca_web::handler::params::Params;
use xitca_web::handler::query::Query;
use xitca_web::handler::state::StateRef;
use xitca_web::http::{HeaderMap, HeaderValue, StatusCode, WebResponse};
use xitca_web::route::{get, post, put};
use xitca_web::{App, NestApp};

use crate::admin::{
    collect_enabled_model_names, create_channel_impl, create_key_impl, delete_channel_impl,
    delete_key_impl, get_channel_impl, list_channels_impl, list_keys_impl, reset_key_usage_impl,
    toggle_channel_impl, toggle_key_impl, update_channel_impl, update_key_impl,
};
use crate::auth::{self, AuthUser, require_me};
use crate::error::{AppError, json_response, store_to_gateway};
use crate::extract::AdminJson;
use crate::state::AppState;

#[derive(Debug, serde::Serialize)]
struct Envelope<T> {
    data: T,
}

fn ok<T: serde::Serialize>(value: T) -> Result<WebResponse, AppError> {
    Ok(json_response(StatusCode::OK, &Envelope { data: value }))
}

fn reject(err: refract_store::StoreError) -> AppError {
    AppError::Admin(store_to_gateway(err))
}

fn json_with_cookie<T: serde::Serialize>(
    value: T,
    cookie: String,
) -> Result<WebResponse, AppError> {
    let mut response = ok(value)?;
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        response
            .headers_mut()
            .insert(xitca_web::http::header::SET_COOKIE, value);
    }
    Ok(response)
}

fn profile_json(user: &User) -> serde_json::Value {
    serde_json::json!({
        "id": user.id,
        "email": user.email,
        "display_name": user.display_name,
        "role": user.role,
        "status": user.status,
        "created_at": user.created_at,
    })
}

async fn current_user(
    state: &AppState,
    headers: &HeaderMap,
    peer: std::net::SocketAddr,
) -> Result<(AuthUser, i64), AppError> {
    let auth = require_me(state, headers, Some(peer)).await?;
    let user_id = auth.effective_user_id(state);
    Ok((auth, user_id))
}

fn require_active(auth: &AuthUser) -> Result<(), AppError> {
    if auth.status != UserStatus::Active {
        return Err(AppError::Admin(GatewayError::new(
            ErrorKind::PermissionDenied,
            "account is not active; verify your email before creating keys or channels",
        )));
    }
    Ok(())
}

async fn load_user(state: &AppState, user_id: i64) -> Result<User, AppError> {
    state
        .user_repo()
        .find_by_id(user_id)
        .await
        .map_err(reject)?
        .ok_or_else(|| AppError::Admin(GatewayError::not_found("user not found")))
}

/// 装配自助面路由。路径相对 `/api/me`。
pub fn nest() -> NestApp<AppState> {
    App::new()
        .at(
            "/profile",
            get(handler_service(get_profile)).put(handler_service(put_profile)),
        )
        .at("/password", post(handler_service(change_password)))
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
        .at("/logs", get(handler_service(query_logs)))
        .at("/logs/{id}", get(handler_service(get_log)))
        .at("/stats", get(handler_service(stats_summary)))
        .at("/stats/models", get(handler_service(stats_by_model)))
        .at("/stats/timeseries", get(handler_service(stats_timeseries)))
        .at(
            "/channels",
            get(handler_service(list_channels)).post(handler_service(create_channel)),
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
        .at("/wallet", get(handler_service(get_wallet)))
        .at("/wallet/ledger/export", get(handler_service(export_ledger)))
        .at("/wallet/ledger", get(handler_service(get_ledger)))
        .at("/models", get(handler_service(list_models)))
}

async fn get_profile(
    StateRef(state): StateRef<'_, AppState>,
    headers: &HeaderMap,
    peer: std::net::SocketAddr,
) -> Result<WebResponse, AppError> {
    let (_, user_id) = current_user(state, headers, peer).await?;
    ok(profile_json(&load_user(state, user_id).await?))
}

#[derive(Debug, Deserialize)]
struct ProfileBody {
    display_name: String,
}

async fn put_profile(
    StateRef(state): StateRef<'_, AppState>,
    AdminJson(body): AdminJson<ProfileBody>,
    headers: &HeaderMap,
    peer: std::net::SocketAddr,
) -> Result<WebResponse, AppError> {
    let (_, user_id) = current_user(state, headers, peer).await?;
    state
        .user_repo()
        .set_display_name(user_id, &body.display_name)
        .await
        .map_err(reject)?;
    state.invalidate_user_cache(user_id);
    ok(profile_json(&load_user(state, user_id).await?))
}

#[derive(Debug, Deserialize)]
struct PasswordBody {
    old_password: String,
    new_password: String,
}

async fn change_password(
    StateRef(state): StateRef<'_, AppState>,
    AdminJson(body): AdminJson<PasswordBody>,
    headers: &HeaderMap,
    peer: std::net::SocketAddr,
) -> Result<WebResponse, AppError> {
    let (_, user_id) = current_user(state, headers, peer).await?;
    if !crate::accounts::valid_password(&body.new_password) {
        return Err(AppError::Admin(GatewayError::invalid_request(
            "password must be at least 10 characters and contain both letters and digits",
        )));
    }
    let user = load_user(state, user_id).await?;
    let ok_old = crate::mail::verify_password(&body.old_password, &user.password_hash)
        .map_err(|e| AppError::Admin(GatewayError::internal(e.to_string())))?;
    if !ok_old {
        return Err(AppError::Admin(GatewayError::new(
            ErrorKind::Unauthenticated,
            "old password is incorrect",
        )));
    }
    let hash = crate::mail::hash_password(&body.new_password)
        .map_err(|e| AppError::Admin(GatewayError::internal(e.to_string())))?;
    state
        .user_repo()
        .set_password_hash(user_id, &hash)
        .await
        .map_err(reject)?;
    state.invalidate_user_cache(user_id);
    let ticket = auth::create_user_session_ticket(
        state.session_secret(),
        user_id,
        auth::SESSION_MAX_AGE_SECS,
    );
    let cookie = auth::session_cookie(
        &ticket,
        auth::SESSION_MAX_AGE_SECS,
        auth::request_is_https(headers),
    );
    json_with_cookie(serde_json::json!({ "ok": true }), cookie)
}

async fn list_keys(
    StateRef(state): StateRef<'_, AppState>,
    headers: &HeaderMap,
    peer: std::net::SocketAddr,
) -> Result<WebResponse, AppError> {
    let (_, user_id) = current_user(state, headers, peer).await?;
    list_keys_impl(state, Some(user_id)).await
}

async fn create_key(
    StateRef(state): StateRef<'_, AppState>,
    AdminJson(spec): AdminJson<refract_store::NewApiKey>,
    headers: &HeaderMap,
    peer: std::net::SocketAddr,
) -> Result<WebResponse, AppError> {
    let (auth, user_id) = current_user(state, headers, peer).await?;
    require_active(&auth)?;
    create_key_impl(state, spec, Some(user_id)).await
}

async fn update_key(
    StateRef(state): StateRef<'_, AppState>,
    Params(id): Params<i64>,
    AdminJson(spec): AdminJson<refract_store::NewApiKey>,
    headers: &HeaderMap,
    peer: std::net::SocketAddr,
) -> Result<WebResponse, AppError> {
    let (_, user_id) = current_user(state, headers, peer).await?;
    update_key_impl(state, id, spec, Some(user_id)).await
}

#[derive(Debug, Deserialize)]
struct EnabledBody {
    enabled: bool,
}

async fn toggle_key(
    StateRef(state): StateRef<'_, AppState>,
    Params(id): Params<i64>,
    AdminJson(body): AdminJson<EnabledBody>,
    headers: &HeaderMap,
    peer: std::net::SocketAddr,
) -> Result<WebResponse, AppError> {
    let (_, user_id) = current_user(state, headers, peer).await?;
    toggle_key_impl(state, id, body.enabled, Some(user_id)).await
}

async fn reset_key_usage(
    StateRef(state): StateRef<'_, AppState>,
    Params(id): Params<i64>,
    headers: &HeaderMap,
    peer: std::net::SocketAddr,
) -> Result<WebResponse, AppError> {
    let (_, user_id) = current_user(state, headers, peer).await?;
    reset_key_usage_impl(state, id, Some(user_id)).await
}

async fn delete_key(
    StateRef(state): StateRef<'_, AppState>,
    Params(id): Params<i64>,
    headers: &HeaderMap,
    peer: std::net::SocketAddr,
) -> Result<WebResponse, AppError> {
    let (_, user_id) = current_user(state, headers, peer).await?;
    delete_key_impl(state, id, Some(user_id)).await
}

async fn query_logs(
    StateRef(state): StateRef<'_, AppState>,
    Query(mut filter): Query<LogFilter>,
    headers: &HeaderMap,
    peer: std::net::SocketAddr,
) -> Result<WebResponse, AppError> {
    let (_, user_id) = current_user(state, headers, peer).await?;
    filter.user_id = Some(user_id);
    let items = state
        .log_repo()
        .query(refract_core::DEFAULT_OWNER_ID, &filter)
        .await
        .map_err(reject)?;
    ok(items)
}

async fn get_log(
    StateRef(state): StateRef<'_, AppState>,
    Params(id): Params<i64>,
    headers: &HeaderMap,
    peer: std::net::SocketAddr,
) -> Result<WebResponse, AppError> {
    let (_, user_id) = current_user(state, headers, peer).await?;
    let log = state
        .log_repo()
        .get(refract_core::DEFAULT_OWNER_ID, id)
        .await
        .map_err(reject)?;
    if log.user_id != Some(user_id) {
        return Err(AppError::Admin(GatewayError::not_found(format!(
            "log `{id}` not found"
        ))));
    }
    ok(log)
}

#[derive(Debug, Deserialize)]
struct StatsQuery {
    #[serde(default = "default_hours")]
    hours: u32,
}

fn default_hours() -> u32 {
    24
}

#[derive(Debug, Deserialize)]
struct TimeseriesQuery {
    #[serde(default = "default_hours")]
    hours: u32,
    #[serde(default = "default_bucket")]
    bucket: String,
}

fn default_bucket() -> String {
    "hour".into()
}

async fn stats_summary(
    StateRef(state): StateRef<'_, AppState>,
    Query(query): Query<StatsQuery>,
    headers: &HeaderMap,
    peer: std::net::SocketAddr,
) -> Result<WebResponse, AppError> {
    let (_, user_id) = current_user(state, headers, peer).await?;
    ok(state
        .log_repo()
        .summary_for_user(refract_core::DEFAULT_OWNER_ID, Some(user_id), query.hours)
        .await
        .map_err(reject)?)
}

async fn stats_by_model(
    StateRef(state): StateRef<'_, AppState>,
    Query(query): Query<StatsQuery>,
    headers: &HeaderMap,
    peer: std::net::SocketAddr,
) -> Result<WebResponse, AppError> {
    let (_, user_id) = current_user(state, headers, peer).await?;
    ok(state
        .log_repo()
        .by_model_for_user(
            refract_core::DEFAULT_OWNER_ID,
            Some(user_id),
            query.hours,
            50,
        )
        .await
        .map_err(reject)?)
}

async fn stats_timeseries(
    StateRef(state): StateRef<'_, AppState>,
    Query(query): Query<TimeseriesQuery>,
    headers: &HeaderMap,
    peer: std::net::SocketAddr,
) -> Result<WebResponse, AppError> {
    let (_, user_id) = current_user(state, headers, peer).await?;
    ok(state
        .log_repo()
        .timeseries_for_user(
            refract_core::DEFAULT_OWNER_ID,
            Some(user_id),
            query.hours,
            query.bucket == "day",
        )
        .await
        .map_err(reject)?)
}

async fn list_channels(
    StateRef(state): StateRef<'_, AppState>,
    headers: &HeaderMap,
    peer: std::net::SocketAddr,
) -> Result<WebResponse, AppError> {
    let (_, user_id) = current_user(state, headers, peer).await?;
    list_channels_impl(state, Some(user_id)).await
}

async fn create_channel(
    StateRef(state): StateRef<'_, AppState>,
    AdminJson(channel): AdminJson<Channel>,
    headers: &HeaderMap,
    peer: std::net::SocketAddr,
) -> Result<WebResponse, AppError> {
    let (auth, user_id) = current_user(state, headers, peer).await?;
    require_active(&auth)?;
    create_channel_impl(state, channel, Some(user_id)).await
}

async fn get_channel(
    StateRef(state): StateRef<'_, AppState>,
    Params(id): Params<ChannelId>,
    headers: &HeaderMap,
    peer: std::net::SocketAddr,
) -> Result<WebResponse, AppError> {
    let (_, user_id) = current_user(state, headers, peer).await?;
    get_channel_impl(state, id, Some(user_id)).await
}

async fn update_channel(
    StateRef(state): StateRef<'_, AppState>,
    Params(id): Params<ChannelId>,
    AdminJson(channel): AdminJson<Channel>,
    headers: &HeaderMap,
    peer: std::net::SocketAddr,
) -> Result<WebResponse, AppError> {
    let (_, user_id) = current_user(state, headers, peer).await?;
    update_channel_impl(state, id, channel, Some(user_id)).await
}

async fn delete_channel(
    StateRef(state): StateRef<'_, AppState>,
    Params(id): Params<ChannelId>,
    headers: &HeaderMap,
    peer: std::net::SocketAddr,
) -> Result<WebResponse, AppError> {
    let (_, user_id) = current_user(state, headers, peer).await?;
    delete_channel_impl(state, id, Some(user_id)).await
}

async fn toggle_channel(
    StateRef(state): StateRef<'_, AppState>,
    Params(id): Params<ChannelId>,
    AdminJson(body): AdminJson<EnabledBody>,
    headers: &HeaderMap,
    peer: std::net::SocketAddr,
) -> Result<WebResponse, AppError> {
    let (_, user_id) = current_user(state, headers, peer).await?;
    toggle_channel_impl(state, id, body.enabled, Some(user_id)).await
}

/// 读取钱包。
pub(crate) async fn wallet_response(
    state: &AppState,
    user_id: i64,
) -> Result<WebResponse, AppError> {
    let wallet = state.wallet_repo().wallet(user_id).await.map_err(reject)?;
    ok(wallet)
}

#[derive(Debug, Deserialize)]
pub(crate) struct LedgerQuery {
    pub limit: Option<u32>,
    pub offset: Option<u32>,
    pub kind: Option<String>,
    pub since: Option<String>,
    pub until: Option<String>,
    pub format: Option<String>,
}

fn parse_kind(raw: Option<&str>) -> Result<Option<LedgerKind>, AppError> {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(s) => s
            .parse()
            .map(Some)
            .map_err(|e| AppError::Admin(GatewayError::invalid_request(e))),
    }
}

fn parse_ts(raw: Option<&str>) -> Result<Option<DateTime<Utc>>, AppError> {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(s) => DateTime::parse_from_rfc3339(s)
            .map(|dt| Some(dt.with_timezone(&Utc)))
            .or_else(|_| {
                chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
                    .map(|n| Some(n.and_utc()))
            })
            .map_err(|_| AppError::Admin(GatewayError::invalid_request("invalid timestamp"))),
    }
}

/// 分页账本。
pub(crate) async fn ledger_response(
    state: &AppState,
    user_id: i64,
    query: &LedgerQuery,
) -> Result<WebResponse, AppError> {
    let kind = parse_kind(query.kind.as_deref())?;
    let since = parse_ts(query.since.as_deref())?;
    let until = parse_ts(query.until.as_deref())?;
    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    let offset = query.offset.unwrap_or(0);
    let items = state
        .wallet_repo()
        .ledger(user_id, limit, offset, kind, since, until)
        .await
        .map_err(reject)?;
    ok(items)
}

fn csv_field(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_owned()
    }
}

/// 导出账本。`format` 默认 csv。
pub(crate) async fn ledger_export_response(
    state: &AppState,
    user_id: i64,
    query: &LedgerQuery,
) -> Result<WebResponse, AppError> {
    let kind = parse_kind(query.kind.as_deref())?;
    let since = parse_ts(query.since.as_deref())?;
    let until = parse_ts(query.until.as_deref())?;
    let items = state
        .wallet_repo()
        .ledger_all_for_export(user_id, kind, since, until)
        .await
        .map_err(reject)?;
    let format = query
        .format
        .as_deref()
        .unwrap_or("csv")
        .trim()
        .to_ascii_lowercase();
    match format.as_str() {
        "ndjson" => {
            let mut body = String::with_capacity(items.len() * 128);
            for item in &items {
                if let Ok(line) = serde_json::to_string(item) {
                    body.push_str(&line);
                    body.push('\n');
                }
            }
            Ok(attachment(
                body.into_bytes(),
                "refract-ledger.ndjson",
                "application/x-ndjson; charset=utf-8",
            ))
        }
        "csv" => {
            let mut body = String::from("created_at,kind,delta,balance_after,ref_id,note\n");
            for item in &items {
                body.push_str(&csv_field(&item.created_at.to_rfc3339()));
                body.push(',');
                body.push_str(item.kind.as_str());
                body.push(',');
                body.push_str(&item.delta.to_string());
                body.push(',');
                body.push_str(&item.balance_after.to_string());
                body.push(',');
                body.push_str(&csv_field(item.ref_id.as_deref().unwrap_or("")));
                body.push(',');
                body.push_str(&csv_field(&item.note));
                body.push('\n');
            }
            Ok(attachment(
                body.into_bytes(),
                "refract-ledger.csv",
                "text/csv; charset=utf-8",
            ))
        }
        other => Err(AppError::Admin(GatewayError::invalid_request(format!(
            "unsupported export format `{other}`"
        )))),
    }
}

fn attachment(bytes: Vec<u8>, filename: &str, content_type: &'static str) -> WebResponse {
    use xitca_web::body::ResponseBody;
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

async fn get_wallet(
    StateRef(state): StateRef<'_, AppState>,
    headers: &HeaderMap,
    peer: std::net::SocketAddr,
) -> Result<WebResponse, AppError> {
    let (_, user_id) = current_user(state, headers, peer).await?;
    wallet_response(state, user_id).await
}

async fn get_ledger(
    StateRef(state): StateRef<'_, AppState>,
    Query(query): Query<LedgerQuery>,
    headers: &HeaderMap,
    peer: std::net::SocketAddr,
) -> Result<WebResponse, AppError> {
    let (_, user_id) = current_user(state, headers, peer).await?;
    ledger_response(state, user_id, &query).await
}

async fn export_ledger(
    StateRef(state): StateRef<'_, AppState>,
    Query(query): Query<LedgerQuery>,
    headers: &HeaderMap,
    peer: std::net::SocketAddr,
) -> Result<WebResponse, AppError> {
    let (_, user_id) = current_user(state, headers, peer).await?;
    ledger_export_response(state, user_id, &query).await
}

async fn list_models(
    StateRef(state): StateRef<'_, AppState>,
    headers: &HeaderMap,
    peer: std::net::SocketAddr,
) -> Result<WebResponse, AppError> {
    let (_, user_id) = current_user(state, headers, peer).await?;
    let channels = state.channels_for(user_id);
    ok(collect_enabled_model_names(channels.iter()))
}

#[cfg(test)]
#[path = "me_tests.rs"]
mod tests;
