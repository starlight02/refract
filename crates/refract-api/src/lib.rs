//! HTTP 层。
//!
//! 两组路由，边界清晰：
//! - **网关路由**（`/v1/...`、`/v1beta/...`）：面向 LLM 客户端，形状必须与
//!   各家官方 API 完全一致，否则现成的 SDK 用不了。
//! - **管理路由**（`/api/...`）：面向本项目自己的前端，形状由我们决定。
//!
//! 两者分开的理由不只是整洁：网关路由的鉴权用「网关 API 密钥」，管理路由用
//! 「管理令牌」。混在一起意味着一个泄漏的推理密钥能改配置。

// lint 配置统一在 workspace `Cargo.toml` 的 [workspace.lints] 里维护。

pub mod admin;
pub mod auth;
pub mod error;
pub mod gateway;
pub mod metrics;
pub mod ops;
pub mod state;
pub mod statics;

pub use error::{ApiError, ErrorEnvelope};
pub use state::AppState;

use warp::{Filter, Reply as _};

/// 装配全部路由。
///
/// 顺序有讲究：管理与网关路由前缀不重叠，静态资源必须放最后 —— 它是
/// 兜底的 SPA fallback，放前面会把 API 路径吃掉。
pub fn routes(
    state: AppState,
) -> impl Filter<Extract = (impl warp::Reply,), Error = std::convert::Infallible> + Clone {
    // CORS preflight：浏览器里的 LLM 客户端（网页版 SillyTavern、各种
    // playground）直接打网关端点前会先发 OPTIONS。放最前面统一应答；
    // 是否附带 CORS 头由下面按路径决定 —— 管理面的预检拿不到许可头，
    // 跨源调用管理 API 会被浏览器拦下，这正是想要的效果。
    let preflight = warp::options().map(|| {
        warp::reply::with_status(warp::reply(), warp::http::StatusCode::NO_CONTENT).into_response()
    });

    let api = preflight
        .or(ops::routes(state.clone()))
        .or(admin::routes(state.clone()))
        .or(gateway::routes(state.clone()))
        .or(statics::routes());

    warp::path::full()
        .and(api.recover(error::recover))
        .map(|path: warp::path::FullPath, reply| {
            let response = warp::Reply::into_response(reply);
            if cors_eligible(path.as_str()) {
                apply_cors(response)
            } else {
                response
            }
        })
        .with(warp::trace::request())
}

/// 哪些路径面向浏览器跨源开放。
///
/// **只有网关面与运维探针**。管理面（`/api`）与内嵌前端是同源应用，根本
/// 不需要 CORS —— 对它们回 `allow-origin: *` 曾是一个真实漏洞：管理令牌
/// 未启用时，任意网页可以用一个简单 GET 跨源读取 `/api/export`，把全部
/// 上游密钥明文拖走。不发许可头，浏览器的同源策略就替我们挡住这类页面。
fn cors_eligible(path: &str) -> bool {
    path.starts_with("/v1/")
        || path.starts_with("/v1beta/")
        || path.starts_with("/health/")
        || path == "/metrics"
}

/// 给网关面响应补上 CORS 头。
///
/// 网关面放开 `*` 是经过考虑的：网关鉴权走显式请求头而非 Cookie，跨源
/// 请求不会自动携带凭据；不放开的话，浏览器内运行的 LLM 客户端根本无法
/// 使用这个网关。代价是 `require_auth=false` 时本机浏览器里的任意页面
/// 也能调用网关 —— 文档中已注明，长期开放使用请启用网关鉴权。
/// 错误响应也必须带这些头 —— 否则浏览器只报「CORS 失败」而吞掉真实错误。
fn apply_cors(mut response: warp::reply::Response) -> warp::reply::Response {
    use warp::http::HeaderValue;
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
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_state() -> AppState {
        let db = refract_store::Database::open_in_memory().await.unwrap();
        let client = refract_upstream::UpstreamClient::new(Default::default()).unwrap();
        AppState::bootstrap(db, client, false).await.unwrap()
    }

    #[tokio::test]
    async fn preflight_returns_no_content_with_cors_headers() {
        let response = warp::test::request()
            .method("OPTIONS")
            .path("/v1/chat/completions")
            .header("origin", "https://client.example")
            .header("access-control-request-method", "POST")
            .reply(&routes(test_state().await))
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
        let response = warp::test::request()
            .method("POST")
            .path("/v1/chat/completions")
            .json(&serde_json::json!({ "model": "ghost", "messages": [] }))
            .reply(&routes(test_state().await))
            .await;

        assert_eq!(response.status(), 404);
        assert_eq!(response.headers()["access-control-allow-origin"], "*");
    }

    #[tokio::test]
    async fn admin_surface_never_carries_cors_headers() {
        // 管理面是同源应用，发放跨源许可等于把 /api/export 里的密钥明文
        // 暴露给任意网页。GET + 无自定义头是「简单请求」，不经预检直达，
        // 所以防线只能是响应头缺失 —— 断言它确实缺失。
        let routes = routes(test_state().await);
        for path in ["/api/channels", "/api/export"] {
            let response = warp::test::request()
                .method("GET")
                .path(path)
                .header("origin", "https://evil.example")
                .reply(&routes)
                .await;
            assert!(
                !response
                    .headers()
                    .contains_key("access-control-allow-origin"),
                "{path} must not grant cross-origin access"
            );
        }

        // 预检同样不该为管理面放行。
        let response = warp::test::request()
            .method("OPTIONS")
            .path("/api/channels")
            .header("origin", "https://evil.example")
            .header("access-control-request-method", "GET")
            .reply(&routes)
            .await;
        assert!(
            !response
                .headers()
                .contains_key("access-control-allow-origin"),
            "admin preflight must not grant cross-origin access"
        );
    }
}
