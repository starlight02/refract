//! 无鉴权的服务存活与就绪探针，以及 Prometheus 指标端点。

use std::convert::Infallible;

use serde_json::json;
use warp::filters::BoxedFilter;
use warp::{Filter, Reply};

use crate::state::{AppState, with_state};

/// 装配 `/health/live`、`/health/ready` 与 `/metrics`。
pub fn routes(state: AppState) -> BoxedFilter<(warp::reply::Response,)> {
    let metrics = warp::path("metrics")
        .and(warp::path::end())
        .and(warp::get())
        .and(with_state(state.clone()))
        .map(|state: AppState| {
            // 与健康探针同样不鉴权：指标不含密钥明文，模型/协议名的暴露面
            // 与部署内网可达性一致；需要隔离时在反代层挡即可。
            warp::reply::with_header(
                state.metrics().render(),
                "content-type",
                "text/plain; version=0.0.4; charset=utf-8",
            )
            .into_response()
        });
    let probes = probe_routes(state);
    routes![metrics, probes]
}

/// 装配 `/health/live` 与 `/health/ready`。
fn probe_routes(state: AppState) -> BoxedFilter<(warp::reply::Response,)> {
    let live = warp::path!("health" / "live")
        .and(warp::path::end())
        .and(warp::get())
        .map(|| {
            warp::reply::with_status(
                warp::reply::json(&json!({"status": "ok"})),
                warp::http::StatusCode::OK,
            )
            .into_response()
        });

    let ready = warp::path!("health" / "ready")
        .and(warp::path::end())
        .and(warp::get())
        .and(with_state(state))
        .and_then(|state: AppState| async move {
            let (status, body) = match state.db().ping().await {
                Ok(()) => (
                    warp::http::StatusCode::OK,
                    json!({"status": "ok", "checks": {"database": "ok"}}),
                ),
                Err(error) => {
                    tracing::warn!(error = %error, "readiness database check failed");
                    (
                        warp::http::StatusCode::SERVICE_UNAVAILABLE,
                        json!({"status": "unavailable", "checks": {"database": "failed"}}),
                    )
                }
            };
            Ok::<_, Infallible>(
                warp::reply::with_status(warp::reply::json(&body), status).into_response(),
            )
        });

    routes![live, ready]
}

#[cfg(test)]
mod tests {
    use super::*;
    use refract_store::Database;

    async fn state() -> AppState {
        let db = Database::open_in_memory().await.unwrap();
        let client = refract_upstream::UpstreamClient::new(Default::default()).unwrap();
        AppState::bootstrap(db, client, false).await.unwrap()
    }

    #[tokio::test]
    async fn metrics_render_in_prometheus_text_format() {
        let state = state().await;
        let response = warp::test::request()
            .method("GET")
            .path("/metrics")
            .reply(&routes(state))
            .await;
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
        let response = warp::test::request()
            .method("GET")
            .path("/health/live")
            .reply(&routes(state))
            .await;
        assert_eq!(response.status(), 200);
    }

    #[tokio::test]
    async fn readiness_tracks_database_availability() {
        let state = state().await;
        let ready = warp::test::request()
            .method("GET")
            .path("/health/ready")
            .reply(&routes(state.clone()))
            .await;
        assert_eq!(ready.status(), 200);

        state.db().close().await;
        let unavailable = warp::test::request()
            .method("GET")
            .path("/health/ready")
            .reply(&routes(state))
            .await;
        assert_eq!(unavailable.status(), 503);
    }
}
