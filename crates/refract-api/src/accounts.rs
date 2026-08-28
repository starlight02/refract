//! 账号认证（`/api/auth/*`）：注册、邮箱验证、登录、登出、密码重置。
//!
//! 两套凭据并存（见 [`crate::auth`] 顶部注释）：
//! - 网关 API key（`/v1/*`）——本模块只负责自助面的登录/注册/找回。
//! - 管理令牌 / 会话 Cookie（`/api/*`）——本模块负责签发与撤销会话。
//!
//! 设计要点：
//! - 邮箱验证 6 位码，SHA-256(user_id:code) 落库，10 分钟过期，最多 5 次尝试。
//! - SMTP 未配置时验证码走 `tracing::info!`（dev mode）；生产部署应配置 `mail.smtp_url`。
//! - 注册/找回对「邮箱不存在」与「成功」返回相同响应，防枚举。
//! - 会话 ticket 带 `iat`；改密码/禁用账号时 `session_revoked_at` 使旧会话立即失效。

use refract_core::{ErrorKind, GatewayError};
use refract_store::{CodePurpose, CodeVerifyOutcome, User, UserRole, UserStatus};
use serde::Deserialize;
use xitca_web::handler::handler_service;
use xitca_web::handler::state::StateRef;
use xitca_web::http::{HeaderMap, HeaderValue, StatusCode, WebResponse};
use xitca_web::route::{get, post};
use xitca_web::{App, NestApp};

use crate::auth::{self};
use crate::error::{AppError, json_response, store_to_gateway};
use crate::extract::AdminJson;
use crate::state::AppState;

/// 统一成功包裹（与 admin.rs 同一形状）。
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

/// 校验邮箱格式（极简版：非空、含且仅含一个 @、两端非空）。
pub(crate) fn valid_email(email: &str) -> bool {
    let mut parts = email.split('@');
    matches!(
        (parts.next(), parts.next(), parts.next()),
        (Some(local), Some(domain), None) if !local.is_empty() && !domain.is_empty() && domain.contains('.')
    )
}

/// 校验密码强度：≥10 位，含字母与数字。
pub(crate) fn valid_password(password: &str) -> bool {
    password.len() >= 10
        && password.chars().any(|c| c.is_ascii_alphabetic())
        && password.chars().any(|c| c.is_ascii_digit())
}

/// 把用户序列化为会话/注册响应用的 JSON。
pub(crate) fn user_json(user: &User) -> serde_json::Value {
    serde_json::json!({
        "id": user.id,
        "email": user.email,
        "display_name": user.display_name,
        "role": user.role,
        "status": user.status,
    })
}

/// 发验证码。dev mode 或未配置 SMTP 时走 log；其余由 mailer 发送。
async fn send_code(
    state: &AppState,
    user_id: i64,
    email: &str,
    purpose: CodePurpose,
) -> Result<(), AppError> {
    let code = {
        use rand::RngExt as _;
        format!("{:06}", rand::rng().random_range(0..1_000_000u32))
    };
    state
        .verification_repo()
        .issue(user_id, purpose, &code, 10)
        .await
        .map_err(reject)?;
    if let Err(error) = state.mailer().send_code(email, purpose, &code).await {
        tracing::error!(error = %error, email, ?purpose, "failed to send verification code");
        return Err(AppError::Admin(GatewayError::internal(
            "failed to deliver verification email",
        )));
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct RegisterBody {
    email: String,
    password: String,
    display_name: Option<String>,
}

async fn register(
    StateRef(state): StateRef<'_, AppState>,
    AdminJson(body): AdminJson<RegisterBody>,
    peer: std::net::SocketAddr,
) -> Result<WebResponse, AppError> {
    let ip = peer.ip();
    if let Some(wait_secs) = state.auth_rate_limiter().check_register(ip) {
        let err = GatewayError::new(
            ErrorKind::PermissionDenied,
            "too many registration attempts",
        )
        .with_retry_after(std::time::Duration::from_secs(wait_secs));
        return Err(AppError::Admin(err));
    }
    state.auth_rate_limiter().record_register_attempt(ip);

    let email = body.email.trim().to_ascii_lowercase();
    if !valid_email(&email) {
        return Err(AppError::Admin(GatewayError::invalid_request(
            "invalid email address",
        )));
    }
    if !valid_password(&body.password) {
        return Err(AppError::Admin(GatewayError::invalid_request(
            "password must be at least 10 characters and contain both letters and digits",
        )));
    }

    let password_hash = crate::mail::hash_password(&body.password)
        .map_err(|e| AppError::Admin(GatewayError::internal(e.to_string())))?;

    let user_repo = state.user_repo();
    let display_name = body.display_name.unwrap_or_default();
    match user_repo
        .create(&email, &password_hash, &display_name, UserRole::User)
        .await
    {
        Ok(user) => {
            state.auth_rate_limiter().record_register_success(ip);
            send_code(state, user.id, &email, CodePurpose::VerifyEmail).await?;
            ok(serde_json::json!({
                "user_id": user.id,
                "verification_required": true,
            }))
        }
        Err(_) => {
            // 邮箱已存在：返回相同形状，不发第二封（防枚举）。
            ok(serde_json::json!({
                "user_id": null,
                "verification_required": true,
            }))
        }
    }
}

#[derive(Debug, Deserialize)]
struct VerifyEmailBody {
    email: String,
    code: String,
}

async fn verify_email(
    StateRef(state): StateRef<'_, AppState>,
    AdminJson(body): AdminJson<VerifyEmailBody>,
) -> Result<WebResponse, AppError> {
    let email = body.email.trim().to_ascii_lowercase();
    let user = state
        .user_repo()
        .find_by_email(&email)
        .await
        .map_err(reject)?
        .ok_or_else(|| AppError::Admin(GatewayError::invalid_request("invalid or expired code")))?;

    let outcome = state
        .verification_repo()
        .verify(
            user.id,
            CodePurpose::VerifyEmail,
            &body.code,
            chrono::Utc::now(),
        )
        .await
        .map_err(reject)?;

    match outcome {
        CodeVerifyOutcome::Ok => {
            state
                .user_repo()
                .mark_email_verified(user.id)
                .await
                .map_err(reject)?;
            state.invalidate_user_cache(user.id);
            ok(serde_json::json!({ "verified": true }))
        }
        CodeVerifyOutcome::Locked => Err(AppError::Admin(GatewayError::new(
            ErrorKind::PermissionDenied,
            "too many attempts; request a new code",
        ))),
        CodeVerifyOutcome::Expired | CodeVerifyOutcome::Invalid => Err(AppError::Admin(
            GatewayError::invalid_request("invalid or expired code"),
        )),
    }
}

#[derive(Debug, Deserialize)]
struct ResendBody {
    email: String,
}

async fn resend_verification(
    StateRef(state): StateRef<'_, AppState>,
    AdminJson(body): AdminJson<ResendBody>,
) -> Result<WebResponse, AppError> {
    let email = body.email.trim().to_ascii_lowercase();
    if let Some(user) = state
        .user_repo()
        .find_by_email(&email)
        .await
        .map_err(reject)?
        && user.status != UserStatus::Active
    {
        if let Some(latest) = state
            .verification_repo()
            .latest_created_at(user.id, CodePurpose::VerifyEmail)
            .await
            .map_err(reject)?
        {
            let elapsed = chrono::Utc::now() - latest;
            if elapsed < chrono::Duration::seconds(60) {
                return Err(AppError::Admin(
                    GatewayError::new(ErrorKind::PermissionDenied, "code was sent recently")
                        .with_retry_after(std::time::Duration::from_secs(
                            (60 - elapsed.num_seconds()).max(1) as u64,
                        )),
                ));
            }
        }
        send_code(state, user.id, &email, CodePurpose::VerifyEmail).await?;
    }
    // 对不存在的邮箱返回相同响应（防枚举）。
    ok(serde_json::json!({ "sent": true }))
}

#[derive(Debug, Deserialize)]
struct LoginBody {
    email: Option<String>,
    password: Option<String>,
    token: Option<String>,
}

async fn auth_login(
    StateRef(state): StateRef<'_, AppState>,
    AdminJson(body): AdminJson<LoginBody>,
    headers: &HeaderMap,
    peer: std::net::SocketAddr,
) -> Result<WebResponse, AppError> {
    let client_ip = peer.ip();

    // 通道 1：管理令牌（紧急恢复）。
    if let Some(token) = body
        .token
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        if let Some(wait_secs) = state.admin_guard().check_locked(client_ip) {
            let err = GatewayError::new(
                ErrorKind::PermissionDenied,
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
            return Err(AppError::Admin(GatewayError::new(
                ErrorKind::PermissionDenied,
                "admin token login is not configured",
            )));
        };
        let token_hash = refract_store::ApiKeyRepo::hash(token);
        if !auth::constant_time_eq(token_hash.as_bytes(), expected_hash.as_bytes()) {
            let locked = state.admin_guard().record_failure(client_ip);
            if locked && let Some(wait_secs) = state.admin_guard().check_locked(client_ip) {
                let err = GatewayError::new(
                    ErrorKind::PermissionDenied,
                    format!("too many failed login attempts, locked for {wait_secs}s"),
                )
                .with_retry_after(std::time::Duration::from_secs(wait_secs));
                return Err(AppError::Admin(err));
            }
            return Err(AppError::Admin(GatewayError::new(
                ErrorKind::Unauthenticated,
                "invalid admin token",
            )));
        }
        state.admin_guard().record_success(client_ip);
        let admin = state
            .user_repo()
            .find_by_id(state.bootstrap_admin_id())
            .await
            .map_err(reject)?
            .ok_or_else(|| AppError::Admin(GatewayError::internal("bootstrap admin missing")))?;
        let ticket = auth::create_user_session_ticket(
            state.session_secret(),
            admin.id,
            auth::SESSION_MAX_AGE_SECS,
        );
        let cookie = auth::session_cookie(
            &ticket,
            auth::SESSION_MAX_AGE_SECS,
            auth::request_is_https(headers),
        );
        return json_with_cookie(
            serde_json::json!({
                "authenticated": true,
                "user": user_json(&admin),
            }),
            cookie,
        );
    }

    // 通道 2：邮箱 + 密码。
    let email = body
        .email
        .as_deref()
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    let password = body.password.as_deref().unwrap_or_default();
    if email.is_empty() || password.is_empty() {
        return Err(AppError::Admin(GatewayError::invalid_request(
            "email and password are required",
        )));
    }

    if let Some(wait_secs) = state.login_guard().check_locked(client_ip, &email) {
        let err = GatewayError::new(
            ErrorKind::PermissionDenied,
            format!("too many failed login attempts, locked for {wait_secs}s"),
        )
        .with_retry_after(std::time::Duration::from_secs(wait_secs));
        return Err(AppError::Admin(err));
    }

    let user = state
        .user_repo()
        .find_by_email(&email)
        .await
        .map_err(reject)?;
    let verified = match user.as_ref() {
        Some(user) => crate::mail::verify_password(password, &user.password_hash)
            .map_err(|e| AppError::Admin(GatewayError::internal(e.to_string())))?,
        None => {
            // 对不存在的账号也做一次哈希，抹平时间侧信道。
            let _ = crate::mail::verify_password(password, crate::mail::DUMMY_PASSWORD_HASH);
            false
        }
    };

    let Some(user) = user else {
        let _ = state.login_guard().record_failure(client_ip, &email);
        return Err(AppError::Admin(GatewayError::new(
            ErrorKind::Unauthenticated,
            "invalid email or password",
        )));
    };
    if !verified {
        let _ = state.login_guard().record_failure(client_ip, &email);
        return Err(AppError::Admin(GatewayError::new(
            ErrorKind::Unauthenticated,
            "invalid email or password",
        )));
    }
    if user.status == UserStatus::Disabled {
        return Err(AppError::Admin(GatewayError::new(
            ErrorKind::PermissionDenied,
            "account is disabled",
        )));
    }

    state.login_guard().record_success(client_ip, &email);

    // 未验证账号：宽限期内可登录但受限；超期直接禁用并拒绝。
    let mut restricted = false;
    if user.status == UserStatus::PendingVerification {
        let grace = chrono::Duration::hours(state.unverified_grace_hours());
        if chrono::Utc::now() - user.created_at > grace {
            state
                .user_repo()
                .set_status(user.id, UserStatus::Disabled)
                .await
                .map_err(reject)?;
            state.invalidate_user_cache(user.id);
            return Err(AppError::Admin(GatewayError::new(
                ErrorKind::PermissionDenied,
                "email verification expired; account disabled",
            )));
        }
        restricted = true;
    }

    let ticket = auth::create_user_session_ticket(
        state.session_secret(),
        user.id,
        auth::SESSION_MAX_AGE_SECS,
    );
    let cookie = auth::session_cookie(
        &ticket,
        auth::SESSION_MAX_AGE_SECS,
        auth::request_is_https(headers),
    );
    json_with_cookie(
        serde_json::json!({
            "authenticated": true,
            "restricted": restricted,
            "user": user_json(&user),
        }),
        cookie,
    )
}

pub(crate) async fn auth_logout(headers: &HeaderMap) -> Result<WebResponse, AppError> {
    let cookie = auth::session_cookie("", 0, auth::request_is_https(headers));
    json_with_cookie(serde_json::json!({ "authenticated": false }), cookie)
}

pub(crate) async fn auth_session(
    StateRef(state): StateRef<'_, AppState>,
    headers: &HeaderMap,
) -> Result<WebResponse, AppError> {
    let configured = state
        .settings_repo()
        .get::<String>(refract_store::settings_repo::KEY_ADMIN_TOKEN_HASH)
        .await
        .map_err(reject)?
        .is_some_and(|hash| !hash.trim().is_empty());

    match auth::require_me(state, headers, None).await {
        Ok(user) => {
            let user_json = match user.user_id {
                Some(id) => state
                    .user_repo()
                    .find_by_id(id)
                    .await
                    .map_err(reject)?
                    .map(|u| user_json(&u)),
                None => state
                    .user_repo()
                    .find_by_id(state.bootstrap_admin_id())
                    .await
                    .map_err(reject)?
                    .map(|u| user_json(&u)),
            };
            ok(serde_json::json!({
                "authenticated": true,
                "configured": configured,
                "user": user_json,
            }))
        }
        Err(_) => ok(serde_json::json!({
            "authenticated": false,
            "configured": configured,
            "user": null,
        })),
    }
}

#[derive(Debug, Deserialize)]
struct PasswordResetRequestBody {
    email: String,
}

async fn password_reset_request(
    StateRef(state): StateRef<'_, AppState>,
    AdminJson(body): AdminJson<PasswordResetRequestBody>,
) -> Result<WebResponse, AppError> {
    let email = body.email.trim().to_ascii_lowercase();
    if let Some(user) = state
        .user_repo()
        .find_by_email(&email)
        .await
        .map_err(reject)?
    {
        send_code(state, user.id, &email, CodePurpose::ResetPassword).await?;
    }
    ok(serde_json::json!({ "sent": true }))
}

#[derive(Debug, Deserialize)]
struct PasswordResetConfirmBody {
    email: String,
    code: String,
    new_password: String,
}

async fn password_reset_confirm(
    StateRef(state): StateRef<'_, AppState>,
    AdminJson(body): AdminJson<PasswordResetConfirmBody>,
) -> Result<WebResponse, AppError> {
    let email = body.email.trim().to_ascii_lowercase();
    if !valid_password(&body.new_password) {
        return Err(AppError::Admin(GatewayError::invalid_request(
            "password must be at least 10 characters and contain both letters and digits",
        )));
    }
    let user = state
        .user_repo()
        .find_by_email(&email)
        .await
        .map_err(reject)?
        .ok_or_else(|| AppError::Admin(GatewayError::invalid_request("invalid or expired code")))?;
    let outcome = state
        .verification_repo()
        .verify(
            user.id,
            CodePurpose::ResetPassword,
            &body.code,
            chrono::Utc::now(),
        )
        .await
        .map_err(reject)?;
    match outcome {
        CodeVerifyOutcome::Ok => {
            let hash = crate::mail::hash_password(&body.new_password)
                .map_err(|e| AppError::Admin(GatewayError::internal(e.to_string())))?;
            state
                .user_repo()
                .set_password_hash(user.id, &hash)
                .await
                .map_err(reject)?;
            state.invalidate_user_cache(user.id);
            ok(serde_json::json!({ "reset": true }))
        }
        CodeVerifyOutcome::Locked => Err(AppError::Admin(GatewayError::new(
            ErrorKind::PermissionDenied,
            "too many attempts; request a new code",
        ))),
        CodeVerifyOutcome::Expired | CodeVerifyOutcome::Invalid => Err(AppError::Admin(
            GatewayError::invalid_request("invalid or expired code"),
        )),
    }
}

/// 开发模式专用：按邮箱取最新验证码。仅 `REFRACT_DEV_MODE=1` 时挂载。
async fn dev_codes(
    StateRef(state): StateRef<'_, AppState>,
    query: xitca_web::handler::query::Query<std::collections::HashMap<String, String>>,
) -> Result<WebResponse, AppError> {
    if !state.dev_mode() {
        return Err(AppError::Admin(GatewayError::not_found("not found")));
    }
    let email = query
        .0
        .get("email")
        .map(|s| s.trim().to_ascii_lowercase())
        .unwrap_or_default();
    ok(serde_json::json!({
        "code": state.mailer().last_code(&email),
    }))
}

/// 装配认证路由。`dev_codes` 由处理器内部按 dev_mode 判断——
/// 路由始终挂载，非 dev 模式返回 404，避免把状态检查漏到装配层。
pub fn nest() -> NestApp<AppState> {
    App::new()
        .at("/register", post(handler_service(register)))
        .at("/verify-email", post(handler_service(verify_email)))
        .at(
            "/resend-verification",
            post(handler_service(resend_verification)),
        )
        .at("/login", post(handler_service(auth_login)))
        .at("/logout", post(handler_service(auth_logout)))
        .at("/session", get(handler_service(auth_session)))
        .at(
            "/password-reset/request",
            post(handler_service(password_reset_request)),
        )
        .at(
            "/password-reset/confirm",
            post(handler_service(password_reset_confirm)),
        )
        .at("/dev-codes", get(handler_service(dev_codes)))
}

#[cfg(test)]
#[path = "accounts_tests.rs"]
mod tests;
