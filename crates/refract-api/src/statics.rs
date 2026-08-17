//! 内嵌前端静态资源。
//!
//! 前端产物编译进二进制，而不是运行时读磁盘。理由很实际：这个网关是个人
//! 自用工具，部署方式应该是「拷一个文件过去跑起来」。分发一个二进制 + 一个
//! `dist/` 目录意味着两者可能版本错配 —— 用户升级了程序忘了换前端，然后看到
//! 一个调不通新接口的旧界面。
//!
//! SPA fallback 的边界很关键：找不到的路径回 `index.html`，**但 API 前缀除外**。
//! 不排除的话，一个拼错的 `/api/chanels` 会返回 200 + HTML，前端 `response.json()`
//! 抛出语法错误，用户看到的是「Unexpected token <」而不是 404。
use warp::http::header::{CACHE_CONTROL, CONTENT_ENCODING, CONTENT_TYPE, ETAG, VARY};
use warp::http::{HeaderValue, StatusCode};
use warp::{Filter, Reply, filters::BoxedFilter};

/// 编译期内嵌的前端产物。
///
/// `web/dist` 由 `build.rs` 保证存在 —— rust-embed 对不存在的目录会直接
/// 编译失败，而后端必须能在前端还没构建时独立编译（CI 里就是分开跑的）。
#[derive(rust_embed::Embed)]
#[folder = "../../web/dist"]
struct Assets;

/// 不应被 SPA fallback 接管的路径前缀。
///
/// 这些前缀下的 404 必须保持 404：客户端 SDK 和前端都靠状态码判断，
/// 回一个 HTML 页面会把「路径写错了」伪装成「服务器返回了奇怪的数据」。
const API_PREFIXES: [&str; 4] = ["api/", "health/", "v1/", "v1beta/"];

/// 静态资源支持的内容编码格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentEncoding {
    /// 原始未压缩。
    Identity,
    /// Brotli 压缩。
    Brotli,
    /// Gzip 压缩。
    Gzip,
}

impl ContentEncoding {
    /// 对应的 `Content-Encoding` 头部值。
    pub fn header_value(self) -> Option<&'static str> {
        match self {
            Self::Identity => None,
            Self::Brotli => Some("br"),
            Self::Gzip => Some("gzip"),
        }
    }
}

/// 静态资源路由。
pub fn routes() -> BoxedFilter<(warp::reply::Response,)> {
    warp::get()
        .and(warp::path::tail())
        .and(warp::header::optional::<String>("accept-encoding"))
        .and(warp::header::optional::<String>("if-none-match"))
        .and_then(
            |tail: warp::path::Tail, accept_encoding: Option<String>, inm: Option<String>| async move {
                serve(tail.as_str(), accept_encoding.as_deref(), inm.as_deref())
                    .ok_or_else(warp::reject::not_found)
            },
        )
        .boxed()
}

/// 根据客户端 `Accept-Encoding` 协商选择最佳资源（优先 br，其次 gzip，最后未压缩）。
fn select_asset<'a>(
    candidate: &'a str,
    accept_encoding: Option<&str>,
) -> Option<(&'a str, rust_embed::EmbeddedFile, ContentEncoding)> {
    let ae = accept_encoding.unwrap_or("");
    let supports_br = ae.split(',').any(|part| part.trim().starts_with("br"));
    let supports_gzip = ae
        .split(',')
        .any(|part| part.trim().starts_with("gzip") || part.trim().starts_with('*'));

    if supports_br {
        let br_path = format!("{candidate}.br");
        if let Some(file) = Assets::get(&br_path) {
            return Some((candidate, file, ContentEncoding::Brotli));
        }
    }

    if supports_gzip {
        let gz_path = format!("{candidate}.gz");
        if let Some(file) = Assets::get(&gz_path) {
            return Some((candidate, file, ContentEncoding::Gzip));
        }
    }

    Assets::get(candidate).map(|file| (candidate, file, ContentEncoding::Identity))
}

/// 解析一个路径并渲染响应。
///
/// 返回 `None` 表示「应当交给别的过滤器或报 404」。
fn serve(
    path: &str,
    accept_encoding: Option<&str>,
    if_none_match: Option<&str>,
) -> Option<warp::reply::Response> {
    let trimmed = path.trim_start_matches('/');

    // API 前缀不走静态资源，也不走 fallback。裸前缀（`/api` 本身）同样
    // 排除：`starts_with("api/")` 对不带尾斜杠的路径不命中，拼错的客户端
    // 会拿到 200 + HTML 而不是 404。
    if API_PREFIXES
        .iter()
        .any(|p| trimmed.starts_with(p) || trimmed == p.trim_end_matches('/'))
    {
        return None;
    }

    let candidate = if trimmed.is_empty() {
        "index.html"
    } else {
        trimmed
    };

    if let Some((orig_path, file, encoding)) = select_asset(candidate, accept_encoding) {
        return Some(render(orig_path, file, encoding, if_none_match));
    }

    // 找不到的路径交给 SPA：前端路由（/channels、/logs）在服务端不存在，
    // 但必须能被直接访问和刷新。
    let (orig_path, index, encoding) = select_asset("index.html", accept_encoding)?;
    Some(render(orig_path, index, encoding, if_none_match))
}
/// 渲染一个资源为 HTTP 响应。
fn render(
    path: &str,
    file: rust_embed::EmbeddedFile,
    encoding: ContentEncoding,
    if_none_match: Option<&str>,
) -> warp::reply::Response {
    let etag = format!("\"{}\"", hex_digest(&file.metadata.sha256_hash()));

    if if_none_match.is_some_and(|v| v == etag) {
        // 命中缓存：回 304，不发 body。对日志页这种频繁刷新的场景省下整个 bundle。
        let mut response =
            warp::reply::with_status(warp::reply(), StatusCode::NOT_MODIFIED).into_response();
        if let Ok(value) = HeaderValue::from_str(&etag) {
            response.headers_mut().insert(ETAG, value);
        }
        if encoding != ContentEncoding::Identity {
            response
                .headers_mut()
                .insert(VARY, HeaderValue::from_static("Accept-Encoding"));
        }
        return response;
    }

    // 嵌入产物在 release 下是 &'static [u8]，直接零拷贝挂进响应；
    // 每次请求 to_vec() 等于把整个 bundle 复制一遍。
    let body = match file.data {
        std::borrow::Cow::Borrowed(bytes) => bytes::Bytes::from_static(bytes),
        std::borrow::Cow::Owned(vec) => bytes::Bytes::from(vec),
    };
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    let mut response = warp::reply::Response::new(body.into());

    let headers = response.headers_mut();
    if let Ok(value) = HeaderValue::from_str(mime.as_ref()) {
        headers.insert(CONTENT_TYPE, value);
    }
    if let Ok(value) = HeaderValue::from_str(&etag) {
        headers.insert(ETAG, value);
    }
    headers.insert(CACHE_CONTROL, HeaderValue::from_static(cache_policy(path)));
    if let Some(enc) = encoding.header_value() {
        headers.insert(CONTENT_ENCODING, HeaderValue::from_static(enc));
        headers.insert(VARY, HeaderValue::from_static("Accept-Encoding"));
    }

    response
}

/// 缓存策略。
///
/// 带内容哈希的构建产物可以永久缓存（Vite 会给它们加 `-a1b2c3d4` 后缀，
/// 内容变了文件名就变）。`index.html` 绝不能缓存 —— 它引用的正是那些带哈希的
/// 文件名，缓存住它等于把用户永久钉在旧版本上。
fn cache_policy(path: &str) -> &'static str {
    if path.ends_with(".html") {
        "no-cache"
    } else if path.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "public, max-age=3600"
    }
}

/// 把哈希字节渲染成十六进制。
fn hex_digest(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    // 只取前 16 字节：ETag 不需要抗碰撞强度，短一点省头部字节。
    bytes
        .iter()
        .take(16)
        .fold(String::with_capacity(32), |mut acc, b| {
            let _ = write!(acc, "{b:02x}");
            acc
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_prefixes_are_never_swallowed_by_the_spa_fallback() {
        // 这是最重要的一条：API 的 404 必须保持 404。
        for path in [
            "api/channels",
            "health/not-a-probe",
            "v1/chat/completions",
            "v1beta/models",
        ] {
            assert!(
                serve(path, None, None).is_none(),
                "{path} must not be handled by the static server"
            );
        }
    }

    #[test]
    fn bare_api_prefixes_are_never_swallowed_by_the_spa_fallback() {
        // `GET /api`（不带后续段）同样必须 404：`starts_with("api/")`
        // 对裸前缀不命中，之前会被 fallback 吃掉变成 200 + HTML。
        for path in ["api", "health", "v1", "v1beta"] {
            assert!(
                serve(path, None, None).is_none(),
                "{path} must not be handled by the static server"
            );
        }
    }

    #[test]
    fn api_prefix_check_does_not_match_lookalike_paths() {
        // `/apidocs` 不是 API 路径，不该被排除 —— 前缀判断带斜杠正是为此。
        let handled = serve("apidocs", None, None);
        // dist 可能为空（CI 未构建前端），此时返回 None 也算通过；
        // 关键是它不因为「以 api 开头」被主动排除。
        if Assets::get("index.html").is_some() {
            assert!(handled.is_some(), "apidocs should fall through to the SPA");
        }
    }

    #[test]
    fn cache_policy_never_caches_the_entry_document() {
        assert_eq!(cache_policy("index.html"), "no-cache");
    }

    #[test]
    fn hashed_assets_are_cached_forever() {
        assert!(cache_policy("assets/index-a1b2c3.js").contains("immutable"));
        assert!(cache_policy("assets/main-99.css").contains("max-age=31536000"));
    }

    #[test]
    fn unhashed_files_get_a_short_cache() {
        let policy = cache_policy("favicon.ico");
        assert!(policy.contains("max-age=3600"));
        assert!(!policy.contains("immutable"));
    }

    #[test]
    fn hex_digest_is_lowercase_hex_and_bounded() {
        let digest = hex_digest(&[0x00, 0xab, 0xff, 0x10]);
        assert_eq!(digest, "00abff10");
        // 超长输入被截断到 32 个十六进制字符。
        let long = hex_digest(&[0xcd; 64]);
        assert_eq!(long.len(), 32);
    }

    #[tokio::test]
    async fn missing_asset_falls_back_to_index_when_frontend_is_built() {
        // 前端未构建时 dist 为空，这个断言无从谈起 —— 跳过而不是假装通过。
        if Assets::get("index.html").is_none() {
            return;
        }
        let response = serve("channels", None, None).expect("SPA route should be served");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers().get(CONTENT_TYPE).unwrap(), "text/html");
    }

    #[tokio::test]
    async fn matching_etag_yields_304() {
        if Assets::get("index.html").is_none() {
            return;
        }
        let first = serve("", None, None).expect("index");
        let etag = first
            .headers()
            .get(ETAG)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();

        let second = serve("", None, Some(&etag)).expect("index");
        assert_eq!(second.status(), StatusCode::NOT_MODIFIED);
    }

    #[tokio::test]
    async fn serves_compressed_assets_when_requested() {
        if Assets::get("index.html").is_none() {
            return;
        }
        // 请求 gzip
        if Assets::get("index.html.gz").is_some() {
            let gz_resp = serve("", Some("gzip, deflate"), None).expect("gzip index");
            assert_eq!(gz_resp.headers().get(CONTENT_ENCODING).unwrap(), "gzip");
            assert_eq!(gz_resp.headers().get(VARY).unwrap(), "Accept-Encoding");
        }
        // 请求 br
        if Assets::get("index.html.br").is_some() {
            let br_resp = serve("", Some("gzip, deflate, br"), None).expect("brotli index");
            assert_eq!(br_resp.headers().get(CONTENT_ENCODING).unwrap(), "br");
            assert_eq!(br_resp.headers().get(VARY).unwrap(), "Accept-Encoding");
        }
    }
}
