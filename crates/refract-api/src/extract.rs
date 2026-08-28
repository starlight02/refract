//! xitca-web 官方扩展点。
//!
//! 内置 `Json<T>` / `Multipart` / `CookieJar` 对不上网关合同（加密信封、
//! 原样透传、HMAC 会话）。官方接法是自己实现 [`FromRequest`]，不是再写
//! 一套游离在 handler 里的读体函数。

use serde::de::DeserializeOwned;
use xitca_web::WebContext;
use xitca_web::bytes::Bytes;
use xitca_web::error::{BodyOverFlow, Error};
use xitca_web::handler::FromRequest;
use xitca_web::handler::body::Limit;
use xitca_web::http::WebResponse;
use xitca_web::service::Service;

use crate::auth::{require_admin_user, require_me};
use crate::error::AppError;
use crate::state::AppState;

/// 管理 API JSON 体上限。与历史 `ADMIN_BODY_LIMIT` 相同。
pub const ADMIN_JSON_LIMIT: usize = 2 * 1024 * 1024;

/// 管理面 JSON。先走官方限长读体，再解密信封。
pub struct AdminJson<T>(pub T);

/// 把已读字节解成管理 JSON（明文或加密信封）。
pub fn decode_admin_json<T: DeserializeOwned>(
    state: &AppState,
    bytes: &[u8],
) -> Result<T, AppError> {
    if let Ok(envelope) = serde_json::from_slice::<crate::crypto::EncryptedEnvelope>(bytes)
        && envelope.__encrypted
    {
        let decrypted = state
            .transport_crypto()
            .decrypt_envelope(&envelope)
            .map_err(|error| {
                AppError::Admin(refract_core::GatewayError::new(
                    refract_core::ErrorKind::InvalidRequest,
                    format!("failed to decrypt transport payload: {error}"),
                ))
            })?;
        return serde_json::from_slice::<T>(&decrypted).map_err(|error| {
            AppError::Admin(refract_core::GatewayError::new(
                refract_core::ErrorKind::InvalidRequest,
                format!("failed to parse decrypted JSON payload: {error}"),
            ))
        });
    }
    serde_json::from_slice::<T>(bytes).map_err(|error| {
        AppError::Admin(refract_core::GatewayError::new(
            refract_core::ErrorKind::InvalidRequest,
            format!("invalid JSON payload: {error}"),
        ))
    })
}

impl<'a, 'r, T> FromRequest<'a, WebContext<'r, AppState>> for AdminJson<T>
where
    T: DeserializeOwned + 'static,
{
    type Type<'b> = AdminJson<T>;
    type Error = Error;

    async fn from_request(ctx: &'a WebContext<'r, AppState>) -> Result<Self, Self::Error> {
        let bytes = match <(Bytes, Limit<ADMIN_JSON_LIMIT>)>::from_request(ctx).await {
            Ok((bytes, _)) => bytes,
            Err(error) => {
                if error.upcast().downcast_ref::<BodyOverFlow>().is_some() {
                    return Err(AppError::PayloadTooLarge.into());
                }
                return Err(error);
            }
        };
        decode_admin_json(ctx.state(), &bytes)
            .map(AdminJson)
            .map_err(Into::into)
    }
}

/// `/api/auth/*` 中不需要登录的端点。
fn is_public_auth(path: &str) -> bool {
    matches!(
        path,
        "/api/auth/login"
            | "/api/auth/logout"
            | "/api/auth/session"
            | "/api/auth/register"
            | "/api/auth/verify-email"
            | "/api/auth/resend-verification"
            | "/api/auth/password-reset/request"
            | "/api/auth/password-reset/confirm"
            | "/api/auth/dev-codes"
    )
}

/// 管理面/自助面鉴权中间件。
///
/// 分流规则：
/// - `/api/auth/*`：公开（登录/注册/找回）或登录态可读（session）。
/// - `/api/admin/*`：必须 admin 角色（`/api/admin/crypto/public-key` 除外——
///   它只暴露服务器公钥，加密写操作前就需要它）。
/// - `/api/me/*`：必须登录（user/admin 均可）。
/// - 其余 `/api/*`：旧路径统一 410 Gone，不再做任何鉴权——防止旧探测
///   流量绕过认证拿到数据。
pub async fn require_auth_mw<S, E>(
    service: &S,
    ctx: WebContext<'_, AppState>,
) -> Result<WebResponse, Error>
where
    S: for<'r> Service<WebContext<'r, AppState>, Response = WebResponse, Error = E>,
    E: Into<Error>,
{
    let path = ctx.req().uri().path();
    if !path.starts_with("/api/") {
        return service.call(ctx).await.map_err(Into::into);
    }
    let peer = crate::peer_addr(*ctx.req().body().socket_addr());
    if path == "/api/admin/crypto/public-key" {
        // 公开：ECDH 公钥本身不是秘密，加密写操作前必须能拿到。
        return service.call(ctx).await.map_err(Into::into);
    }
    if path.starts_with("/api/auth/") {
        if is_public_auth(path) {
            return service.call(ctx).await.map_err(Into::into);
        }
        require_me(ctx.state(), ctx.req().headers(), peer).await?;
    } else if path.starts_with("/api/admin/") {
        require_admin_user(ctx.state(), ctx.req().headers(), peer).await?;
    } else if path.starts_with("/api/me/") {
        require_me(ctx.state(), ctx.req().headers(), peer).await?;
    }
    // 其余旧路径（如 /api/channels）不进鉴权、直达 410 兜底路由。
    service.call(ctx).await.map_err(Into::into)
}
