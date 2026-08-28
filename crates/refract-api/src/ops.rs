//! 无鉴权的服务存活与就绪探针，以及 Prometheus 指标端点。

use std::net::SocketAddr;

use serde_json::json;
use xitca_web::body::ResponseBody;
use xitca_web::handler::state::StateRef;
use xitca_web::http::{HeaderMap, HeaderValue, StatusCode, WebResponse, header};

use crate::auth::require_admin;
use crate::error::{AppError, json_response};
use crate::state::AppState;

/// `GET /health/live`
pub async fn live() -> Result<WebResponse, AppError> {
    Ok(json_response(StatusCode::OK, &json!({"status": "ok"})))
}

/// `GET /health/ready`
pub async fn ready(StateRef(state): StateRef<'_, AppState>) -> Result<WebResponse, AppError> {
    let (status, body) = match state.db().ping().await {
        Ok(()) => (
            StatusCode::OK,
            json!({"status": "ok", "checks": {"database": "ok"}}),
        ),
        Err(error) => {
            tracing::warn!(error = %error, "readiness database check failed");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                json!({"status": "unavailable", "checks": {"database": "failed"}}),
            )
        }
    };
    Ok(json_response(status, &body))
}

/// 旧版管理路径（`/api/*`，无 `admin`/`me`/`auth` 前缀）一律 410。
///
/// 多用户化把管理面迁到 `/api/admin/*`；旧路径若静默 404，外部脚本会误以为是
/// 配置损坏。410 明确告诉调用方「路径已死」，响应里给出迁移指引。
pub async fn legacy_gone() -> Result<WebResponse, AppError> {
    Ok(json_response(
        StatusCode::GONE,
        &json!({
            "error": {
                "type": "gone",
                "message": "this endpoint moved to /api/admin/* (self-service: /api/me/*); see the v0.6 upgrade notes",
            }
        }),
    ))
}

/// `GET /metrics`
pub async fn metrics(
    StateRef(state): StateRef<'_, AppState>,
    headers: &HeaderMap,
    addr: SocketAddr,
) -> Result<WebResponse, AppError> {
    require_admin(state, headers, crate::peer_addr(addr)).await?;
    if state.metrics().per_user_enabled() {
        match state.wallet_repo().all_wallets().await {
            Ok(wallets) => state.metrics().set_wallet_balances(
                wallets
                    .into_iter()
                    .map(|wallet| (wallet.user_id, wallet.balance)),
            ),
            Err(error) => {
                state.metrics().set_wallet_balances([]);
                tracing::warn!(%error, "failed to refresh per-user wallet metrics");
            }
        }
    }
    let body = state.metrics().render();
    let mut response = WebResponse::new(ResponseBody::bytes(body));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
    );
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http_test::TestRequest;
    use refract_store::Database;

    async fn state() -> AppState {
        let db = Database::open_in_memory().await.unwrap();
        let client = refract_upstream::UpstreamClient::new(Default::default()).unwrap();
        AppState::bootstrap(db, client, false).await.unwrap()
    }

    #[tokio::test]
    async fn metrics_render_in_prometheus_text_format() {
        let state = state().await;
        let response = TestRequest::get("/metrics").send(state).await;
        assert_eq!(response.status(), 200);
        assert!(
            response.headers()["content-type"]
                .to_str()
                .unwrap()
                .starts_with("text/plain")
        );
        let body = String::from_utf8_lossy(response.body());
        assert!(body.contains("refract_requests_total"));
        assert!(body.contains("refract_uptime_seconds"));
    }

    #[tokio::test]
    async fn liveness_is_independent_of_database_state() {
        let state = state().await;
        state.db().close().await;
        let response = TestRequest::get("/health/live").send(state).await;
        assert_eq!(response.status(), 200);
    }

    #[tokio::test]
    async fn readiness_tracks_database_availability() {
        let state = state().await;
        let ready = TestRequest::get("/health/ready").send(state.clone()).await;
        assert_eq!(ready.status(), 200);

        state.db().close().await;
        let unavailable = TestRequest::get("/health/ready").send(state).await;
        assert_eq!(unavailable.status(), 503);
    }
}
