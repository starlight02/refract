//! HTTP 层。
//!
//! 两组路由，边界清晰：
//! - **网关路由**（`/v1/...`、`/v1beta/...`）：面向 LLM 客户端，形状必须与
//!   各家官方 API 完全一致，否则现成的 SDK 用不了。
//! - **管理路由**（`/api/...`）：面向本项目自己的前端，形状由我们决定。
//!
//! 两者分开的理由不只是整洁：网关路由的鉴权用「网关 API 密钥」，管理路由用
//! 「管理令牌」。混在一起意味着一个泄漏的推理密钥能改配置。

pub mod admin;
pub mod auth;
pub mod backup;
pub mod crypto;
pub mod error;
pub mod extract;
pub mod gateway;
pub mod metrics;
pub mod notify;
pub mod ops;
pub mod rate;
pub mod realtime;
pub mod state;
pub mod statics;

#[cfg(test)]
pub mod http_test;

pub use error::{ApiError, ErrorEnvelope};
pub use state::AppState;

use std::net::SocketAddr;
use xitca_web::error::{MatchError, MethodNotAllowed};
use xitca_web::handler::handler_service;
use xitca_web::http::{HeaderValue, Method, StatusCode, WebResponse};
use xitca_web::route::{get, post};
use xitca_web::service::{Service, fn_service};
use xitca_web::{App, WebContext};

use crate::error::{AppError, empty_response};

/// 未指定对端地址时视为缺失，鉴权回落到 `127.0.0.1`。
pub(crate) fn peer_addr(addr: SocketAddr) -> Option<SocketAddr> {
    if addr.ip().is_unspecified() {
        None
    } else {
        Some(addr)
    }
}

/// 装配全部路由。
///
/// 顺序有讲究：管理与网关路由前缀不重叠，静态资源必须放最后 —— 它是
/// 兜底的 SPA fallback，放前面会把 API 路径吃掉。
macro_rules! assembled_app {
    ($state:expr) => {
        App::new()
            .at("/health/live", get(handler_service(ops::live)))
            .at("/health/ready", get(handler_service(ops::ready)))
            .at("/metrics", get(handler_service(ops::metrics)))
            .at(
                "/v1/chat/completions",
                post(handler_service(gateway::chat_completions)),
            )
            .at("/v1/messages", post(handler_service(gateway::messages)))
            .at("/v1/responses", post(handler_service(gateway::responses)))
            .at("/v1/embeddings", post(handler_service(gateway::embeddings)))
            .at(
                "/v1/completions",
                post(handler_service(gateway::completions)),
            )
            .at(
                "/v1/images/generations",
                post(handler_service(gateway::images_generations)),
            )
            .at(
                "/v1/audio/speech",
                post(handler_service(gateway::audio_speech)),
            )
            .at(
                "/v1/moderations",
                post(handler_service(gateway::moderations)),
            )
            .at("/v1/rerank", post(handler_service(gateway::rerank)))
            .at(
                "/v1/messages/count_tokens",
                post(handler_service(gateway::count_tokens)),
            )
            .at(
                "/v1/audio/transcriptions",
                post(handler_service(gateway::audio_transcriptions)),
            )
            .at(
                "/v1/audio/translations",
                post(handler_service(gateway::audio_translations)),
            )
            .at(
                "/v1/images/edits",
                post(handler_service(gateway::image_edits)),
            )
            .at("/v1/models", get(handler_service(gateway::list_models)))
            .at("/v1/models/{*id}", get(handler_service(gateway::get_model)))
            .at(
                "/v1beta/models",
                get(handler_service(gateway::list_models_gemini)),
            )
            .at(
                "/v1beta/models/{*rest}",
                get(handler_service(gateway::get_model_gemini))
                    .post(handler_service(gateway::gemini_action)),
            )
            .at("/v1/realtime", get(fn_service(realtime::realtime)))
            .at("/api", admin::nest())
            .at("/", get(handler_service(statics::root)))
            .at("/{*path}", handler_service(statics::asset))
            .with_state($state)
            .enclosed_fn(crate::extract::require_admin_mw)
            .enclosed_fn(options_and_cors)
    };
}

/// 装配全部路由，供进程内测试 `finish().call()`。
pub fn build_app(
    state: AppState,
) -> App<impl Service<Error: std::fmt::Debug> + Send + Sync, impl Send + Sync> {
    assembled_app!(state)
}

#[cfg(test)]
/// 进程内测试：对装配好的应用发一次请求并收齐响应体。
pub(crate) async fn dispatch_test(
    state: AppState,
    request: xitca_web::http::Request<xitca_web::http::RequestExt<xitca_web::body::RequestBody>>,
) -> (
    StatusCode,
    xitca_web::http::HeaderMap,
    xitca_web::bytes::Bytes,
) {
    let service = assembled_app!(state)
        .finish()
        .call(())
        .await
        .unwrap_or_else(|error| panic!("app finish failed: {error:?}"));
    let response = service
        .call(request)
        .await
        .unwrap_or_else(|error| panic!("service call failed: {error:?}"));
    let status = response.status();
    let headers = response.headers().clone();
    let body = xitca_web::test::collect_body(response.into_body())
        .await
        .unwrap_or_else(|error| panic!("collect body failed: {error}"));
    (status, headers, xitca_web::bytes::Bytes::from(body))
}

/// 监听并启动 xitca-server。调用方负责 `wait` 与优雅关闭。
pub fn start_server(
    state: AppState,
    listener: std::net::TcpListener,
) -> std::io::Result<(ServerStop, impl FnOnce() -> std::io::Result<()>)> {
    let mut server = assembled_app!(state)
        .serve()
        .disable_signal()
        .listen(listener)?
        .run();
    let handle = server.handle()?;
    Ok((ServerStop(handle), move || server.wait()))
}

/// 已启动服务器的停止手柄。
#[derive(Clone)]
pub struct ServerStop(xitca_server::ServerHandle);

impl ServerStop {
    /// 请求停机。`graceful` 为真时排空连接。
    pub fn stop(&self, graceful: bool) {
        self.0.stop(graceful);
    }
}

/// 哪些路径面向浏览器跨源开放。
///
/// **只有网关面与运维探针**。管理面（`/api`）与内嵌前端是同源应用，根本
/// 不需要 CORS —— 对它们回 `allow-origin: *` 曾是一个真实漏洞：管理令牌
/// 未启用时，任意网页可以用一个简单 GET 跨源读取 `/api/export`，把全部
/// 上游密钥明文拖走。不发许可头，浏览器的同源策略就替我们挡住这类页面。
fn cors_eligible(path: &str) -> bool {
    path.starts_with("/v1/") || path.starts_with("/v1beta/") || path.starts_with("/health/")
}

/// 给网关面响应补上 CORS 头。
///
/// 网关面放开 `*` 是经过考虑的：网关鉴权走显式请求头而非 Cookie，跨源
/// 请求不会自动携带凭据；不放开的话，浏览器内运行的 LLM 客户端根本无法
/// 使用这个网关。代价是 `require_auth=false` 时本机浏览器里的任意页面
/// 也能调用网关 —— 文档中已注明，长期开放使用请启用网关鉴权。
/// 错误响应也必须带这些头 —— 否则浏览器只报「CORS 失败」而吞掉真实错误。
fn apply_cors(response: &mut WebResponse) {
    let headers = response.headers_mut();
    headers.insert("access-control-allow-origin", HeaderValue::from_static("*"));
    headers.insert(
        "access-control-allow-methods",
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    headers.insert(
        "access-control-allow-headers",
        HeaderValue::from_static(
            "authorization, content-type, x-api-key, anthropic-version, x-goog-api-key",
        ),
    );
    headers.insert("access-control-max-age", HeaderValue::from_static("86400"));
}

fn map_router_error(err: &xitca_web::error::Error, path: &str) -> Option<AppError> {
    let std_err = err.upcast();
    if std_err.downcast_ref::<MatchError>().is_some() {
        return Some(AppError::NotFound {
            path: path.to_owned(),
        });
    }
    if let Some(denied) = std_err.downcast_ref::<MethodNotAllowed>() {
        let allowed = denied
            .allowed_methods()
            .iter()
            .map(Method::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        return Some(AppError::MethodNotAllowed { allowed });
    }
    std_err.downcast_ref::<AppError>().cloned()
}

async fn options_and_cors<S, E>(
    service: &S,
    mut ctx: WebContext<'_, AppState>,
) -> Result<WebResponse, xitca_web::error::Error>
where
    S: for<'r> Service<WebContext<'r, AppState>, Response = WebResponse, Error = E>,
    E: Into<xitca_web::error::Error>,
{
    let path = ctx.req().uri().path().to_owned();
    let method = ctx.req().method().clone();

    if method == Method::OPTIONS {
        let mut response = empty_response(StatusCode::NO_CONTENT);
        if cors_eligible(&path) {
            apply_cors(&mut response);
        }
        tracing::info!(method = %method, path = %path, status = 204);
        return Ok(response);
    }

    let mut response = match service.call(ctx.reborrow()).await {
        Ok(response) => response,
        Err(error) => {
            let error = error.into();
            if let Some(app_error) = map_router_error(&error, &path) {
                app_error.to_response(&path)
            } else {
                Service::call(&error, ctx.reborrow()).await?
            }
        }
    };

    if cors_eligible(&path) {
        apply_cors(&mut response);
    }
    tracing::info!(method = %method, path = %path, status = response.status().as_u16());
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http_test::TestRequest;
    use xitca_web::http::Method;

    async fn test_state() -> AppState {
        let db = refract_store::Database::open_in_memory().await.unwrap();
        let client = refract_upstream::UpstreamClient::new(Default::default()).unwrap();
        AppState::bootstrap(db, client, false).await.unwrap()
    }

    #[tokio::test]
    async fn preflight_returns_no_content_with_cors_headers() {
        let response = TestRequest::get("/v1/chat/completions")
            .method(Method::OPTIONS)
            .header("origin", "https://client.example")
            .header("access-control-request-method", "POST")
            .send(test_state().await)
            .await;

        assert_eq!(response.status(), 204);
        assert_eq!(
            response.headers()["access-control-allow-origin"],
            "*",
            "preflight must allow any origin"
        );
        let allowed = response.headers()["access-control-allow-headers"]
            .to_str()
            .unwrap();
        for header in ["authorization", "x-api-key", "anthropic-version"] {
            assert!(allowed.contains(header), "{allowed}");
        }
    }

    #[tokio::test]
    async fn error_responses_also_carry_cors_headers() {
        // 浏览器只有在错误响应也带 CORS 头时才能读到真实错误信息。
        let response = TestRequest::post("/v1/chat/completions")
            .json(&serde_json::json!({ "model": "ghost", "messages": [] }))
            .send(test_state().await)
            .await;

        assert_eq!(response.status(), 404);
        assert_eq!(response.headers()["access-control-allow-origin"], "*");
    }

    #[tokio::test]
    async fn admin_surface_never_carries_cors_headers() {
        // 管理面是同源应用，发放跨源许可等于把 /api/export 里的密钥明文
        // 暴露给任意网页。GET + 无自定义头是「简单请求」，不经预检直达，
        // 所以防线只能是响应头缺失 —— 断言它确实缺失。
        let state = test_state().await;
        for path in ["/api/channels", "/api/export"] {
            let response = TestRequest::get(path)
                .header("origin", "https://evil.example")
                .send(state.clone())
                .await;
            assert!(
                !response
                    .headers()
                    .contains_key("access-control-allow-origin"),
                "{path} must not grant cross-origin access"
            );
        }

        // 预检同样不该为管理面放行。
        let response = TestRequest::get("/api/channels")
            .method(Method::OPTIONS)
            .header("origin", "https://evil.example")
            .header("access-control-request-method", "GET")
            .send(state)
            .await;
        assert!(
            !response
                .headers()
                .contains_key("access-control-allow-origin"),
            "admin preflight must not grant cross-origin access"
        );
    }

    #[tokio::test]
    async fn metrics_endpoint_never_carries_cors_headers() {
        // Prometheus 从服务端抓取，不需要 CORS；给浏览器脚本留一个
        // 跨源读 /metrics 的口子只会泄漏运行指标（渠道/模型标签、计数）。
        let response = TestRequest::get("/metrics")
            .header("origin", "https://evil.example")
            .send(test_state().await)
            .await;
        assert_eq!(response.status(), 200);
        assert!(
            !response
                .headers()
                .contains_key("access-control-allow-origin"),
            "/metrics must not grant cross-origin access"
        );
    }

    #[tokio::test]
    async fn unmatched_v1_path_uses_openai_error_envelope() {
        let response = TestRequest::get("/v1/chat/completion")
            .send(test_state().await)
            .await;
        assert_eq!(response.status(), 404);
        let body = response.json();
        assert_eq!(body["error"]["message"], "endpoint not found");
        assert!(
            body.get("code").is_none(),
            "must not use the admin envelope"
        );
    }

    #[tokio::test]
    async fn unmatched_v1beta_path_uses_gemini_error_envelope() {
        let response = TestRequest::get("/v1beta/not-a-real-endpoint")
            .send(test_state().await)
            .await;
        assert_eq!(response.status(), 404);
        let body = response.json();
        assert_eq!(body["error"]["message"], "endpoint not found");
        assert_eq!(body["error"]["status"], "NOT_FOUND");
    }

    #[tokio::test]
    async fn unmatched_admin_path_keeps_admin_envelope() {
        let response = TestRequest::get("/api/does-not-exist")
            .send(test_state().await)
            .await;
        let body = response.json();
        assert!(
            body.get("code").is_some(),
            "admin fallback must keep {{code,message}}, got {body}"
        );
        assert!(
            body.get("error")
                .and_then(|error| error.get("message"))
                .is_none()
        );
    }

    #[tokio::test]
    async fn unmatched_post_is_404_not_catch_all_405() {
        let state = test_state().await;

        let chat = TestRequest::post("/v1/chat/completion")
            .send(state.clone())
            .await;
        assert_eq!(chat.status(), 404);
        assert_eq!(chat.json()["error"]["message"], "endpoint not found");

        let gemini = TestRequest::post("/v1beta/not-a-real-endpoint")
            .send(state.clone())
            .await;
        assert_eq!(gemini.status(), 404);
        assert_eq!(gemini.json()["error"]["status"], "NOT_FOUND");

        let admin = TestRequest::post("/api/does-not-exist").send(state).await;
        assert!(admin.json().get("code").is_some());
        assert_eq!(admin.status(), 404);
    }
}
