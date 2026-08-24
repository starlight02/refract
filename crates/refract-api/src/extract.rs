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

use crate::auth::require_admin;
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

fn is_public_admin(path: &str) -> bool {
    matches!(
        path,
        "/api/crypto/public-key" | "/api/auth/login" | "/api/auth/logout" | "/api/auth/session"
    )
}

/// 管理面鉴权中间件。只拦 `/api`，公开登录/会话路由放行。
pub async fn require_admin_mw<S, E>(
    service: &S,
    ctx: WebContext<'_, AppState>,
) -> Result<WebResponse, Error>
where
    S: for<'r> Service<WebContext<'r, AppState>, Response = WebResponse, Error = E>,
    E: Into<Error>,
{
    let path = ctx.req().uri().path();
    if path.starts_with("/api/") && !is_public_admin(path) {
        let peer = crate::peer_addr(*ctx.req().body().socket_addr());
        require_admin(ctx.state(), ctx.req().headers(), peer).await?;
    }
    service.call(ctx).await.map_err(Into::into)
}
