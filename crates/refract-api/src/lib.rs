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
use std::path::{Path, PathBuf};
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

/// TLS 身份。同一套 PEM 用于 TCP 上的 HTTPS（HTTP/1.1 + HTTP/2）和 UDP 上的 HTTP/3。
#[derive(Debug, Clone)]
pub struct TlsListen {
    /// TCP/UDP 绑定地址，通常与明文 `listen` 相同。
    pub addr: SocketAddr,
    /// PEM 证书链路径。
    pub cert_pem: PathBuf,
    /// PEM 私钥路径。
    pub key_pem: PathBuf,
}
impl TlsListen {
    /// 提前读证书、构造 TLS 配置。在 bind 前把「文件缺失 / PEM 损坏 /
    /// 密钥与证书不匹配」拦在启动第一屏，而不是在 `listen` 失败时才报错。
    pub fn validate(&self) -> std::io::Result<()> {
        let _ = load_rustls_config(&self.cert_pem, &self.key_pem)?;
        let _ = load_quic_config(&self.cert_pem, &self.key_pem)?;
        Ok(())
    }
}

/// 监听并启动 xitca-server。调用方负责 `wait` 与优雅关闭。
///
/// 无 TLS 时 TCP 提供 HTTP/1.1 与 h2c（prior knowledge）。`tls` 为 `Some` 时
/// 释放明文 TCP，改为同一地址上的 HTTPS（ALPN：h2、http/1.1）加 HTTP/3。
pub fn start_server(
    state: AppState,
    listener: std::net::TcpListener,
    tls: Option<TlsListen>,
) -> std::io::Result<(ServerStop, impl FnOnce() -> std::io::Result<()>)> {
    let mut server = assembled_app!(state).serve().disable_signal();
    if let Some(tls) = tls {
        let addr = tls.addr;
        drop(listener);
        let rustls_config = load_rustls_config(&tls.cert_pem, &tls.key_pem)?;
        let quic_config = load_quic_config(&tls.cert_pem, &tls.key_pem)?;
        server = server
            .bind_rustls(addr, rustls_config)?
            .bind_h3(addr, quic_config)?;
    } else {
        server = server.h2c_prior_knowledge().listen(listener)?;
    }
    let mut server = server.run();
    let handle = server.handle()?;
    Ok((ServerStop(handle), move || server.wait()))
}

fn load_pem(
    cert_path: &Path,
    key_path: &Path,
) -> std::io::Result<(
    Vec<quinn::rustls::pki_types::CertificateDer<'static>>,
    quinn::rustls::pki_types::PrivateKeyDer<'static>,
)> {
    let cert_pem = std::fs::read(cert_path).map_err(|error| {
        std::io::Error::new(
            error.kind(),
            format!(
                "failed to read TLS certificate {}: {error}",
                cert_path.display()
            ),
        )
    })?;
    let key_pem = std::fs::read(key_path).map_err(|error| {
        std::io::Error::new(
            error.kind(),
            format!(
                "failed to read TLS private key {}: {error}",
                key_path.display()
            ),
        )
    })?;

    let certs = rustls_pemfile::certs(&mut &*cert_pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid TLS certificate PEM: {error}"),
            )
        })?;
    if certs.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("no certificates in {}", cert_path.display()),
        ));
    }

    let key = rustls_pemfile::private_key(&mut &*key_pem)
        .map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid TLS private key PEM: {error}"),
            )
        })?
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("no private key in {}", key_path.display()),
            )
        })?;
    Ok((certs, key))
}

fn rustls_server_config(
    certs: Vec<quinn::rustls::pki_types::CertificateDer<'static>>,
    key: quinn::rustls::pki_types::PrivateKeyDer<'static>,
) -> std::io::Result<quinn::rustls::ServerConfig> {
    quinn::rustls::ServerConfig::builder_with_provider(std::sync::Arc::new(
        quinn::rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid TLS protocol versions: {error}"),
        )
    })?
    .with_no_client_auth()
    .with_single_cert(certs, key)
    .map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid TLS certificate/key pair: {error}"),
        )
    })
}

fn load_rustls_config(
    cert_path: &Path,
    key_path: &Path,
) -> std::io::Result<quinn::rustls::ServerConfig> {
    let (certs, key) = load_pem(cert_path, key_path)?;
    rustls_server_config(certs, key)
}

fn load_quic_config(cert_path: &Path, key_path: &Path) -> std::io::Result<quinn::ServerConfig> {
    let (certs, key) = load_pem(cert_path, key_path)?;
    let mut tls = rustls_server_config(certs, key)?;
    tls.alpn_protocols = vec![b"h3".to_vec()];
    let tls = quinn::crypto::rustls::QuicServerConfig::try_from(tls).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid QUIC TLS config: {error}"),
        )
    })?;
    Ok(quinn::ServerConfig::with_crypto(std::sync::Arc::new(tls)))
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

    let alt_svc_port = ctx.state().alt_svc_port();
    if alt_svc_port != 0
        && let Ok(value) = HeaderValue::from_str(&format!("h3=\":{alt_svc_port}\"; ma=86400"))
    {
        response.headers_mut().insert("alt-svc", value);
    }
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

    fn write_self_signed(dir: &std::path::Path) -> (PathBuf, PathBuf, rcgen::Certificate) {
        let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
        let cert_path = dir.join("cert.pem");
        let key_path = dir.join("key.pem");
        std::fs::write(&cert_path, certified.cert.pem()).unwrap();
        std::fs::write(&key_path, certified.signing_key.serialize_pem()).unwrap();
        (cert_path, key_path, certified.cert)
    }

    #[test]
    fn load_quic_config_rejects_missing_files() {
        let error = super::load_quic_config(
            Path::new("/no/such/cert.pem"),
            Path::new("/no/such/key.pem"),
        )
        .expect_err("missing cert");
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn load_quic_config_rejects_empty_pem() {
        let dir = tempfile::tempdir().unwrap();
        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("key.pem");
        std::fs::write(&cert_path, "").unwrap();
        std::fs::write(&key_path, "").unwrap();
        let error = super::load_quic_config(&cert_path, &key_path).expect_err("empty pem");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn load_quic_config_accepts_self_signed_pem() {
        let dir = tempfile::tempdir().unwrap();
        let (cert_path, key_path, _) = write_self_signed(dir.path());
        super::load_quic_config(&cert_path, &key_path).expect("valid pem");
    }

    async fn spawn_plain() -> (SocketAddr, super::ServerStop, std::thread::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let std_listener = listener.into_std().unwrap();
        std_listener.set_nonblocking(true).unwrap();
        let (handle, wait) =
            super::start_server(test_state().await, std_listener, None).expect("listen");
        let join = std::thread::spawn(move || {
            let _ = wait();
        });
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        (addr, handle, join)
    }

    #[tokio::test]
    async fn h2c_curl_prior_knowledge_health_live() {
        let (addr, handle, join) = spawn_plain().await;
        let url = format!("http://{addr}/health/live");
        let output = std::process::Command::new("curl")
            .args(["--http2-prior-knowledge", "-sS", "-D", "-", "-o", "-", &url])
            .output()
            .expect("curl");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "curl h2c failed: status={} stderr={stderr} stdout={stdout}",
            output.status
        );
        assert!(
            stdout.contains("HTTP/2 200") || stdout.contains("HTTP/2.0 200"),
            "expected HTTP/2 200, got {stdout}"
        );
        assert!(stdout.contains("\"status\""), "body: {stdout}");
        handle.stop(true);
        let _ = join.join();
    }

    #[tokio::test]
    async fn h2c_server_sends_settings_after_preface() {
        let (addr, handle, join) = spawn_plain().await;
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        stream
            .write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n")
            .await
            .unwrap();
        stream
            .write_all(&[0, 0, 0, 4, 0, 0, 0, 0, 0])
            .await
            .unwrap();
        let mut buf = [0u8; 256];
        let n = tokio::time::timeout(std::time::Duration::from_secs(2), stream.read(&mut buf))
            .await
            .expect("server should answer h2c preface")
            .unwrap();
        assert!(n >= 9, "short h2c response: {n} bytes {:x?}", &buf[..n]);
        let len = u32::from_be_bytes([0, buf[0], buf[1], buf[2]]) as usize;
        let kind = buf[3];
        assert_eq!(
            kind,
            4,
            "first frame should be SETTINGS, got kind={kind} len={len} {:x?}",
            &buf[..n]
        );
        handle.stop(true);
        let _ = join.join();
    }

    #[tokio::test]
    async fn h2c_does_not_break_http1() {
        let (addr, handle, join) = spawn_plain().await;
        let h1 = reqwest::get(format!("http://{addr}/health/live"))
            .await
            .expect("http/1.1");
        assert_eq!(h1.status(), reqwest::StatusCode::OK);
        assert_eq!(h1.version(), reqwest::Version::HTTP_11);
        assert!(h1.headers().get("alt-svc").is_none());
        handle.stop(true);
        let _ = join.join();
    }

    #[tokio::test]
    async fn tls_serves_h1_h2_and_h3() {
        let dir = tempfile::tempdir().unwrap();
        let (cert_path, key_path, cert) = write_self_signed(dir.path());
        let pem = std::fs::read(&cert_path).unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let std_listener = listener.into_std().unwrap();
        std_listener.set_nonblocking(true).unwrap();

        let (handle, wait) = super::start_server(
            {
                let state = test_state().await;
                state.set_alt_svc_port(addr.port());
                state
            },
            std_listener,
            Some(TlsListen {
                addr,
                cert_pem: cert_path,
                key_pem: key_path,
            }),
        )
        .expect("listen");
        let join = std::thread::spawn(move || {
            let _ = wait();
        });
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;

        let https = format!("https://localhost:{}/health/live", addr.port());
        let ca = reqwest::Certificate::from_pem(&pem).unwrap();

        let h2 = reqwest::Client::builder()
            .add_root_certificate(ca.clone())
            .https_only(true)
            .build()
            .unwrap()
            .get(&https)
            .send()
            .await
            .expect("https h2");
        assert_eq!(h2.status(), reqwest::StatusCode::OK);
        assert_eq!(h2.version(), reqwest::Version::HTTP_2);
        assert_eq!(
            h2.headers().get("alt-svc").and_then(|v| v.to_str().ok()),
            Some(format!("h3=\":{}\"; ma=86400", addr.port()).as_str()),
        );

        let h1 = reqwest::Client::builder()
            .add_root_certificate(ca)
            .http1_only()
            .https_only(true)
            .build()
            .unwrap()
            .get(&https)
            .send()
            .await
            .expect("https h1");
        assert_eq!(h1.status(), reqwest::StatusCode::OK);
        assert_eq!(h1.version(), reqwest::Version::HTTP_11);

        let status = h3_get_live(addr, cert.der().clone()).await;
        assert_eq!(status, http::StatusCode::OK);

        handle.stop(true);
        let _ = join.join();
    }

    async fn h3_get_live(
        addr: SocketAddr,
        root: quinn::rustls::pki_types::CertificateDer<'static>,
    ) -> http::StatusCode {
        let mut roots = quinn::rustls::RootCertStore::empty();
        roots.add(root).expect("trust self-signed cert");
        let mut tls = quinn::rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
            quinn::rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
        tls.alpn_protocols = vec![b"h3".to_vec()];
        let mut endpoint = quinn::Endpoint::client(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        endpoint.set_default_client_config(quinn::ClientConfig::new(std::sync::Arc::new(
            quinn::crypto::rustls::QuicClientConfig::try_from(tls).unwrap(),
        )));

        let conn = endpoint
            .connect(addr, "localhost")
            .unwrap()
            .await
            .expect("QUIC handshake");
        let (mut driver, mut send_request) = h3::client::new(h3_quinn::Connection::new(conn))
            .await
            .expect("h3 client");

        let request = async {
            let request = http::Request::builder()
                .uri("https://localhost/health/live")
                .body(())
                .unwrap();
            let mut stream = send_request.send_request(request).await.unwrap();
            stream.finish().await.unwrap();
            stream.recv_response().await.unwrap().status()
        };
        tokio::select! {
            error = driver.wait_idle() => panic!("h3 driver closed: {error}"),
            status = request => status,
        }
    }

    fn sse_stream() -> &'static str {
        "data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n"
    }

    async fn state_with_sse_channel(upstream: &str) -> AppState {
        let db = refract_store::Database::open_in_memory().await.unwrap();
        let channel = refract_core::Channel {
            id: 0,
            owner_id: refract_core::DEFAULT_OWNER_ID,
            name: "sse-upstream".into(),
            kind: refract_core::ChannelKind::Single(refract_core::Protocol::Chat),
            enabled: true,
            priority: 0,
            weight: 1,
            credential: refract_core::Credential::new("k"),
            credentials: Vec::new(),
            key_strategy: Default::default(),
            address: refract_core::UpstreamAddress {
                unofficial: true,
                full_address: false,
                base_url: Some(upstream.to_owned()),
                version_prefix: None,
                path: None,
            },
            endpoints: vec![refract_core::ChannelEndpoint {
                models: vec![refract_core::ModelEntry::plain("gpt-4o")],
                ..refract_core::ChannelEndpoint::new(refract_core::Protocol::Chat)
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
        };
        refract_store::ChannelRepo::new(db.clone())
            .create(&channel)
            .await
            .unwrap();
        let client = refract_upstream::UpstreamClient::new(Default::default()).unwrap();
        AppState::bootstrap(db, client, false).await.unwrap()
    }

    #[tokio::test]
    async fn h2c_post_chat_streams_sse() {
        let upstream = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/chat/completions"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse_stream()),
            )
            .mount(&upstream)
            .await;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let std_listener = listener.into_std().unwrap();
        std_listener.set_nonblocking(true).unwrap();
        let (handle, wait) = super::start_server(
            state_with_sse_channel(&upstream.uri()).await,
            std_listener,
            None,
        )
        .expect("listen");
        let join = std::thread::spawn(move || {
            let _ = wait();
        });
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;

        let body = serde_json::json!({
            "model": "gpt-4o",
            "stream": true,
            "messages": [{ "role": "user", "content": "hi" }]
        });
        let output = std::process::Command::new("curl")
            .args([
                "--http2-prior-knowledge",
                "-sS",
                "-D",
                "-",
                "--header",
                "content-type: application/json",
                "--data",
                &serde_json::to_string(&body).unwrap(),
                &format!("http://{addr}/v1/chat/completions"),
            ])
            .output()
            .expect("curl h2c");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "h2c POST failed: status={} stderr={stderr} stdout={stdout}",
            output.status
        );
        assert!(
            stdout.contains("HTTP/2 200") || stdout.contains("HTTP/2.0 200"),
            "expected h2 response, got {stdout}"
        );
        assert!(
            stdout.contains("data:"),
            "expected SSE body over h2, got {stdout}"
        );
        assert!(
            stdout.contains("[DONE]"),
            "SSE stream must terminate over h2, got {stdout}"
        );
        handle.stop(true);
        let _ = join.join();
    }

    #[tokio::test]
    async fn h3_post_chat_streams_sse() {
        let upstream = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/chat/completions"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse_stream()),
            )
            .mount(&upstream)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let (cert_path, key_path, cert) = write_self_signed(dir.path());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let std_listener = listener.into_std().unwrap();
        std_listener.set_nonblocking(true).unwrap();

        let (handle, wait) = super::start_server(
            state_with_sse_channel(&upstream.uri()).await,
            std_listener,
            Some(TlsListen {
                addr,
                cert_pem: cert_path,
                key_pem: key_path,
            }),
        )
        .expect("listen");
        let join = std::thread::spawn(move || {
            let _ = wait();
        });
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;

        let (status, body) = h3_post_sse(addr, cert.der().clone()).await;
        assert_eq!(status, http::StatusCode::OK);
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("data:"), "h3 SSE body: {text}");
        assert!(text.contains("[DONE]"), "h3 SSE must terminate: {text}");

        handle.stop(true);
        let _ = join.join();
    }

    async fn h3_post_sse(
        addr: SocketAddr,
        root: quinn::rustls::pki_types::CertificateDer<'static>,
    ) -> (http::StatusCode, bytes::Bytes) {
        let mut roots = quinn::rustls::RootCertStore::empty();
        roots.add(root).expect("trust self-signed cert");
        let mut tls = quinn::rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
            quinn::rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
        tls.alpn_protocols = vec![b"h3".to_vec()];
        let mut endpoint = quinn::Endpoint::client(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        endpoint.set_default_client_config(quinn::ClientConfig::new(std::sync::Arc::new(
            quinn::crypto::rustls::QuicClientConfig::try_from(tls).unwrap(),
        )));

        let conn = endpoint
            .connect(addr, "localhost")
            .unwrap()
            .await
            .expect("QUIC handshake");
        let (mut driver, mut send_request) = h3::client::new(h3_quinn::Connection::new(conn))
            .await
            .expect("h3 client");

        let request = async {
            let payload = serde_json::to_vec(&serde_json::json!({
                "model": "gpt-4o",
                "stream": true,
                "messages": [{ "role": "user", "content": "hi" }]
            }))
            .unwrap();
            let request = http::Request::builder()
                .method(http::Method::POST)
                .uri("https://localhost/v1/chat/completions")
                .header("content-type", "application/json")
                .body(())
                .unwrap();
            let mut stream = send_request.send_request(request).await.unwrap();
            stream.send_data(payload.into()).await.unwrap();
            stream.finish().await.unwrap();
            let response = stream.recv_response().await.unwrap();
            let status = response.status();
            let mut body = bytes::BytesMut::new();
            while let Some(chunk) = stream.recv_data().await.unwrap() {
                body.extend_from_slice(bytes::Buf::chunk(&chunk));
            }
            (status, body.freeze())
        };
        tokio::select! {
            error = driver.wait_idle() => panic!("h3 driver closed: {error}"),
            result = request => result,
        }
    }
}
