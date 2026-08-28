//! 管理员用户管理（`/api/admin/users...`）。
//!
//! 不做 DELETE 用户：账务与日志审计需要保留，disabled 即终态。

use refract_core::GatewayError;
use refract_store::{LedgerKind, User, UserRole, UserStatus};
use serde::Deserialize;
use xitca_web::handler::handler_service;
use xitca_web::handler::params::Params;
use xitca_web::handler::query::Query;
use xitca_web::handler::state::StateRef;
use xitca_web::http::{HeaderMap, StatusCode, WebResponse};
use xitca_web::route::{get, post};
use xitca_web::{App, NestApp};

use crate::auth::require_admin_user;
use crate::error::{AppError, json_response, store_to_gateway};
use crate::extract::AdminJson;
use crate::me::{LedgerQuery, ledger_response, wallet_response};
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

fn user_with_balance(user: &User, balance: f64) -> serde_json::Value {
    serde_json::json!({
        "id": user.id,
        "email": user.email,
        "display_name": user.display_name,
        "role": user.role,
        "status": user.status,
        "created_at": user.created_at,
        "balance": balance,
    })
}

async fn load_user(state: &AppState, id: i64) -> Result<User, AppError> {
    state
        .user_repo()
        .find_by_id(id)
        .await
        .map_err(reject)?
        .ok_or_else(|| AppError::Admin(GatewayError::not_found(format!("user `{id}` not found"))))
}

fn admin_ref(admin_id: i64) -> String {
    format!("admin:{admin_id}:{}", uuid::Uuid::new_v4())
}

/// 装配管理员用户路由。路径相对 `/api/admin`。
pub fn nest() -> NestApp<AppState> {
    App::new()
        .at(
            "/users",
            get(handler_service(list_users)).post(handler_service(create_user)),
        )
        .at(
            "/users/{id}",
            get(handler_service(get_user)).put(handler_service(update_user)),
        )
        .at("/users/{id}/disable", post(handler_service(disable_user)))
        .at("/users/{id}/enable", post(handler_service(enable_user)))
        .at(
            "/users/{id}/wallet/topup",
            post(handler_service(wallet_topup)),
        )
        .at(
            "/users/{id}/wallet/adjust",
            post(handler_service(wallet_adjust)),
        )
        .at(
            "/users/{id}/wallet/refund",
            post(handler_service(wallet_refund)),
        )
        .at(
            "/users/{id}/wallet/ledger",
            get(handler_service(user_ledger)),
        )
        .at("/users/{id}/wallet", get(handler_service(user_wallet)))
}

#[derive(Debug, Deserialize)]
struct UserListQuery {
    status: Option<UserStatus>,
    email: Option<String>,
    limit: Option<u32>,
    offset: Option<u32>,
}

async fn list_users(
    StateRef(state): StateRef<'_, AppState>,
    Query(query): Query<UserListQuery>,
    headers: &HeaderMap,
    peer: std::net::SocketAddr,
) -> Result<WebResponse, AppError> {
    let _ = require_admin_user(state, headers, Some(peer)).await?;
    let users = state
        .user_repo()
        .list_filtered(
            query.status,
            query.email.as_deref(),
            query.limit.unwrap_or(50),
            query.offset.unwrap_or(0),
        )
        .await
        .map_err(reject)?;
    let mut out = Vec::with_capacity(users.len());
    for user in &users {
        let balance = state.wallet_repo().balance(user.id).await.map_err(reject)?;
        out.push(user_with_balance(user, balance));
    }
    ok(out)
}

async fn get_user(
    StateRef(state): StateRef<'_, AppState>,
    Params(id): Params<i64>,
    headers: &HeaderMap,
    peer: std::net::SocketAddr,
) -> Result<WebResponse, AppError> {
    let _ = require_admin_user(state, headers, Some(peer)).await?;
    let user = load_user(state, id).await?;
    let balance = state.wallet_repo().balance(id).await.map_err(reject)?;
    ok(user_with_balance(&user, balance))
}

#[derive(Debug, Deserialize)]
struct CreateUserBody {
    email: String,
    password: String,
    display_name: Option<String>,
    role: Option<UserRole>,
    initial_balance: Option<f64>,
}

async fn create_user(
    StateRef(state): StateRef<'_, AppState>,
    AdminJson(body): AdminJson<CreateUserBody>,
    headers: &HeaderMap,
    peer: std::net::SocketAddr,
) -> Result<WebResponse, AppError> {
    let admin = require_admin_user(state, headers, Some(peer)).await?;
    let email = body.email.trim().to_ascii_lowercase();
    if !crate::accounts::valid_email(&email) {
        return Err(AppError::Admin(GatewayError::invalid_request(
            "invalid email address",
        )));
    }
    if !crate::accounts::valid_password(&body.password) {
        return Err(AppError::Admin(GatewayError::invalid_request(
            "password must be at least 10 characters and contain both letters and digits",
        )));
    }
    let hash = crate::mail::hash_password(&body.password)
        .map_err(|e| AppError::Admin(GatewayError::internal(e.to_string())))?;
    let display_name = body.display_name.unwrap_or_default();
    let role = body.role.unwrap_or(UserRole::User);
    let user = match state
        .user_repo()
        .create(&email, &hash, &display_name, role)
        .await
    {
        Ok(user) => user,
        Err(refract_store::StoreError::Conflict { .. }) => {
            return Err(AppError::Admin(GatewayError::invalid_request(
                "email already registered",
            )));
        }
        Err(err) => return Err(reject(err)),
    };
    state
        .user_repo()
        .mark_email_verified(user.id)
        .await
        .map_err(reject)?;
    if let Some(amount) = body.initial_balance
        && amount > 0.0
        && amount.is_finite()
    {
        let admin_id = admin.effective_user_id(state);
        state
            .wallet_repo()
            .apply(
                user.id,
                amount,
                LedgerKind::Topup,
                Some(&admin_ref(admin_id)),
                "initial balance",
            )
            .await
            .map_err(reject)?;
        state.invalidate_balance_cache(user.id);
    }
    let user = load_user(state, user.id).await?;
    let balance = state.wallet_repo().balance(user.id).await.map_err(reject)?;
    ok(user_with_balance(&user, balance))
}

#[derive(Debug, Deserialize)]
struct UpdateUserBody {
    display_name: Option<String>,
    role: Option<UserRole>,
    status: Option<UserStatus>,
}

async fn update_user(
    StateRef(state): StateRef<'_, AppState>,
    Params(id): Params<i64>,
    AdminJson(body): AdminJson<UpdateUserBody>,
    headers: &HeaderMap,
    peer: std::net::SocketAddr,
) -> Result<WebResponse, AppError> {
    let _ = require_admin_user(state, headers, Some(peer)).await?;
    let user = load_user(state, id).await?;
    if let Some(name) = body.display_name.as_deref() {
        state
            .user_repo()
            .set_display_name(id, name)
            .await
            .map_err(reject)?;
    }
    if let Some(role) = body.role
        && role != user.role
    {
        if user.role == UserRole::Admin
            && role != UserRole::Admin
            && state.user_repo().count_admins().await.map_err(reject)? <= 1
        {
            return Err(AppError::Admin(GatewayError::invalid_request(
                "cannot demote the last admin",
            )));
        }
        state.user_repo().set_role(id, role).await.map_err(reject)?;
        state.invalidate_user_cache(id);
    }
    if let Some(status) = body.status
        && status != user.status
    {
        state
            .user_repo()
            .set_status(id, status)
            .await
            .map_err(reject)?;
        if status == UserStatus::Disabled {
            state
                .user_repo()
                .revoke_sessions(id)
                .await
                .map_err(reject)?;
        }
        state.invalidate_user_cache(id);
    }
    let user = load_user(state, id).await?;
    let balance = state.wallet_repo().balance(id).await.map_err(reject)?;
    ok(user_with_balance(&user, balance))
}

async fn disable_user(
    StateRef(state): StateRef<'_, AppState>,
    Params(id): Params<i64>,
    headers: &HeaderMap,
    peer: std::net::SocketAddr,
) -> Result<WebResponse, AppError> {
    let _ = require_admin_user(state, headers, Some(peer)).await?;
    let _ = load_user(state, id).await?;
    state
        .user_repo()
        .set_status(id, UserStatus::Disabled)
        .await
        .map_err(reject)?;
    state
        .user_repo()
        .revoke_sessions(id)
        .await
        .map_err(reject)?;
    state.invalidate_user_cache(id);
    ok(serde_json::json!({ "id": id, "status": UserStatus::Disabled }))
}

async fn enable_user(
    StateRef(state): StateRef<'_, AppState>,
    Params(id): Params<i64>,
    headers: &HeaderMap,
    peer: std::net::SocketAddr,
) -> Result<WebResponse, AppError> {
    let _ = require_admin_user(state, headers, Some(peer)).await?;
    let _ = load_user(state, id).await?;
    state
        .user_repo()
        .set_status(id, UserStatus::Active)
        .await
        .map_err(reject)?;
    state.invalidate_user_cache(id);
    ok(serde_json::json!({ "id": id, "status": UserStatus::Active }))
}

#[derive(Debug, Deserialize)]
struct AmountBody {
    amount: f64,
    #[serde(default)]
    note: String,
}

#[derive(Debug, Deserialize)]
struct AdjustBody {
    delta: f64,
    #[serde(default)]
    note: String,
}

async fn apply_wallet(
    state: &AppState,
    user_id: i64,
    delta: f64,
    kind: LedgerKind,
    admin_id: i64,
    note: &str,
) -> Result<WebResponse, AppError> {
    let _ = load_user(state, user_id).await?;
    state
        .wallet_repo()
        .apply(user_id, delta, kind, Some(&admin_ref(admin_id)), note)
        .await
        .map_err(reject)?;
    state.invalidate_balance_cache(user_id);
    let balance = state.wallet_repo().balance(user_id).await.map_err(reject)?;
    ok(serde_json::json!({ "balance": balance }))
}

async fn wallet_topup(
    StateRef(state): StateRef<'_, AppState>,
    Params(id): Params<i64>,
    AdminJson(body): AdminJson<AmountBody>,
    headers: &HeaderMap,
    peer: std::net::SocketAddr,
) -> Result<WebResponse, AppError> {
    let admin = require_admin_user(state, headers, Some(peer)).await?;
    if !(body.amount > 0.0 && body.amount.is_finite()) {
        return Err(AppError::Admin(GatewayError::invalid_request(
            "amount must be greater than 0",
        )));
    }
    apply_wallet(
        state,
        id,
        body.amount,
        LedgerKind::Topup,
        admin.effective_user_id(state),
        &body.note,
    )
    .await
}

async fn wallet_adjust(
    StateRef(state): StateRef<'_, AppState>,
    Params(id): Params<i64>,
    AdminJson(body): AdminJson<AdjustBody>,
    headers: &HeaderMap,
    peer: std::net::SocketAddr,
) -> Result<WebResponse, AppError> {
    let admin = require_admin_user(state, headers, Some(peer)).await?;
    if body.delta == 0.0 || !body.delta.is_finite() {
        return Err(AppError::Admin(GatewayError::invalid_request(
            "delta must be a non-zero finite number",
        )));
    }
    apply_wallet(
        state,
        id,
        body.delta,
        LedgerKind::Adjust,
        admin.effective_user_id(state),
        &body.note,
    )
    .await
}

async fn wallet_refund(
    StateRef(state): StateRef<'_, AppState>,
    Params(id): Params<i64>,
    AdminJson(body): AdminJson<AmountBody>,
    headers: &HeaderMap,
    peer: std::net::SocketAddr,
) -> Result<WebResponse, AppError> {
    let admin = require_admin_user(state, headers, Some(peer)).await?;
    if !(body.amount > 0.0 && body.amount.is_finite()) {
        return Err(AppError::Admin(GatewayError::invalid_request(
            "amount must be greater than 0",
        )));
    }
    apply_wallet(
        state,
        id,
        body.amount,
        LedgerKind::Refund,
        admin.effective_user_id(state),
        &body.note,
    )
    .await
}

async fn user_wallet(
    StateRef(state): StateRef<'_, AppState>,
    Params(id): Params<i64>,
    headers: &HeaderMap,
    peer: std::net::SocketAddr,
) -> Result<WebResponse, AppError> {
    let _ = require_admin_user(state, headers, Some(peer)).await?;
    let _ = load_user(state, id).await?;
    wallet_response(state, id).await
}

async fn user_ledger(
    StateRef(state): StateRef<'_, AppState>,
    Params(id): Params<i64>,
    Query(query): Query<LedgerQuery>,
    headers: &HeaderMap,
    peer: std::net::SocketAddr,
) -> Result<WebResponse, AppError> {
    let _ = require_admin_user(state, headers, Some(peer)).await?;
    let _ = load_user(state, id).await?;
    ledger_response(state, id, &query).await
}
