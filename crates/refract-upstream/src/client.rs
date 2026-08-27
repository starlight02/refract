//! 上游请求执行器。

use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use dashmap::DashMap;
use futures_util::Stream;
use refract_core::{
    Action, AuthScheme, Credential, ErrorKind, GatewayError, Protocol, UpstreamAddress,
};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;

use crate::sse::{ByteStream, SseEvent, SseStream};

/// Anthropic 要求的 API 版本头。
///
/// 这是协议版本而非模型版本，Anthropic 长期保持向后兼容。写死是正确的：
/// 让用户去配一个他们并不理解的日期字符串只会制造支持负担。
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// 渠道级代理 Client 上限。超出后淘汰任意一条，避免地址变动导致无界增长。
const MAX_PROXY_CLIENTS: usize = 32;

/// 客户端配置。
#[derive(Debug, Clone)]
pub struct UpstreamClientConfig {
    /// 单请求整体超时。流式请求不受此限制（见 `stream_idle_timeout`）。
    pub timeout: Duration,
    /// 建立连接的超时。
    pub connect_timeout: Duration,
    /// 流式请求的**空闲**超时：两帧之间超过这个时长才算挂。
    ///
    /// 流式不能用整体超时：一次长回答合法地跑几分钟，整体超时会把正常请求
    /// 掐断。但完全不设超时会让上游静默挂起时连接永久占用。
    pub stream_idle_timeout: Duration,
    /// 连接池每主机空闲连接上限。
    pub pool_max_idle_per_host: usize,
    /// 空闲连接保活时长。
    pub pool_idle_timeout: Duration,
    /// 出站代理，形如 `http://host:port` 或 `socks5://host:port`。
    pub proxy: Option<String>,
    /// `User-Agent` 头。
    pub user_agent: String,
}

impl Default for UpstreamClientConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(300),
            connect_timeout: Duration::from_secs(10),
            stream_idle_timeout: Duration::from_secs(120),
            pool_max_idle_per_host: 32,
            pool_idle_timeout: Duration::from_secs(90),
            proxy: None,
            user_agent: concat!("refract/", env!("CARGO_PKG_VERSION")).to_owned(),
        }
    }
}

/// 一个待发往上游的请求。
///
/// 它是**已经决定好**的：地址、凭据、协议、body 都由上层（路由 + 协议转换）
/// 算好。上游客户端不做任何决策，这样它的行为完全可预测。
#[derive(Debug, Clone)]
pub struct UpstreamRequest<'a> {
    /// 上游端点的原生协议 —— 决定鉴权头的形式。
    pub protocol: Protocol,
    /// 地址配置。
    pub address: &'a UpstreamAddress,
    /// 凭据。
    pub credential: &'a Credential,
    /// 上游真实模型名（可能与客户端请求的名字不同）。
    pub model: &'a str,
    /// 请求动作。
    pub action: Action,
    /// 请求体。`None` 表示 GET（如模型列表）。
    pub body: Option<&'a Value>,
    /// 原始请求体。原生协议直通时使用，与 `body` 互斥。
    pub raw_body: Option<&'a [u8]>,
    /// `raw_body` 的 Content-Type。缺省按 JSON 发送；multipart 直通
    /// （音频转写、图像编辑）必须原样携带客户端的 boundary。
    pub raw_content_type: Option<&'a str>,
    /// 需要透传给上游的额外请求头。
    pub extra_headers: &'a [(String, String)],
    /// 渠道级出站代理。设置时覆盖客户端的全局代理。
    pub proxy: Option<&'a str>,
    /// 覆盖默认超时。
    pub timeout: Option<Duration>,
}

impl<'a> UpstreamRequest<'a> {
    /// 构造一次 POST 调用。
    pub fn post(
        protocol: Protocol,
        address: &'a UpstreamAddress,
        credential: &'a Credential,
        model: &'a str,
        action: Action,
        body: &'a Value,
    ) -> Self {
        Self {
            protocol,
            address,
            credential,
            model,
            action,
            body: Some(body),
            raw_body: None,
            raw_content_type: None,
            extra_headers: &[],
            proxy: None,
            timeout: None,
        }
    }

    /// 构造一次模型列表调用。
    pub fn list_models(
        protocol: Protocol,
        address: &'a UpstreamAddress,
        credential: &'a Credential,
    ) -> Self {
        Self {
            protocol,
            address,
            credential,
            model: "",
            action: Action::ListModels,
            body: None,
            raw_body: None,
            raw_content_type: None,
            extra_headers: &[],
            proxy: None,
            timeout: None,
        }
    }

    /// 构造一次不解析、不重编码 JSON 的原生协议 POST 调用。
    pub fn post_raw(
        protocol: Protocol,
        address: &'a UpstreamAddress,
        credential: &'a Credential,
        model: &'a str,
        action: Action,
        body: &'a [u8],
    ) -> Self {
        Self {
            protocol,
            address,
            credential,
            model,
            action,
            body: None,
            raw_body: Some(body),
            raw_content_type: None,
            extra_headers: &[],
            proxy: None,
            timeout: None,
        }
    }
}

/// 一次非流式调用的结果。
#[derive(Debug)]
pub struct UpstreamResponse {
    /// HTTP 状态码。
    pub status: u16,
    /// 解析后的响应体。
    pub body: Value,
    /// 上游返回的响应头中我们关心的部分（限流信息等）。
    pub headers: HeaderMap,
    /// 从收到首个响应 body 字节到 body 完整结束的时间。
    pub first_byte_to_end: Duration,
}

/// 原生协议非流式响应，body 保持上游返回的原始字节。
#[derive(Debug)]
pub struct UpstreamRawResponse {
    /// HTTP 状态码。
    pub status: u16,
    /// 未解析的响应体。
    pub body: Bytes,
    /// 上游返回的响应头中我们关心的部分。
    pub headers: HeaderMap,
    /// 从收到首个响应 body 字节到 body 完整结束的时间。
    pub first_byte_to_end: Duration,
}

/// 原生协议流式响应，保留建立流时收到的 HTTP 元数据。
pub struct UpstreamRawStream {
    /// HTTP 状态码。
    pub status: u16,
    /// 上游响应头。
    pub headers: HeaderMap,
    /// 未解析的响应字节流。
    pub stream: ByteStream,
}

/// 解析型 SSE 响应，保留严格响应校验需要的 HTTP 元数据。
pub struct UpstreamSseStream {
    /// HTTP 状态码。
    pub status: u16,
    /// 上游响应头。
    pub headers: HeaderMap,
    /// 解析后的 SSE 事件流。
    pub stream: SseStream,
}

impl std::fmt::Debug for UpstreamSseStream {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UpstreamSseStream")
            .field("status", &self.status)
            .field("headers", &self.headers)
            .finish_non_exhaustive()
    }
}

impl Stream for UpstreamSseStream {
    type Item = Result<SseEvent, GatewayError>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        self.stream.as_mut().poll_next(cx)
    }
}

impl std::fmt::Debug for UpstreamRawStream {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UpstreamRawStream")
            .field("status", &self.status)
            .field("headers", &self.headers)
            .finish_non_exhaustive()
    }
}

/// 上游客户端。克隆是浅拷贝，共享同一个连接池。
#[derive(Debug, Clone)]
pub struct UpstreamClient {
    http: reqwest::Client,
    proxy_clients: Arc<DashMap<String, reqwest::Client>>,
    config: UpstreamClientConfig,
}

impl UpstreamClient {
    /// 用给定配置构造。
    pub fn new(config: UpstreamClientConfig) -> Result<Self, GatewayError> {
        let http = build_http_client(&config, config.proxy.as_deref())?;

        Ok(Self {
            http,
            proxy_clients: Arc::new(DashMap::new()),
            config,
        })
    }

    /// 当前配置。
    pub fn config(&self) -> &UpstreamClientConfig {
        &self.config
    }

    /// 对任意 URL 发一个带 Bearer 鉴权的 GET 并解析 JSON。
    ///
    /// 面向余额探测这类**非协议标准端点**的辅助调用 —— 地址不经
    /// `UpstreamAddress::resolve` 的形状校验，调用方自己对 URL 负责。
    /// `timeout` 由调用方定：这类辅助调用的合理时长与数据面请求不同。
    pub async fn get_json(
        &self,
        url: &str,
        credential: &Credential,
        proxy: Option<&str>,
        timeout: Duration,
    ) -> Result<serde_json::Value, GatewayError> {
        let http = self.http_for(proxy)?;
        let response = http
            .get(url)
            .headers(auth_headers(Protocol::Chat, credential)?)
            .timeout(timeout)
            .send()
            .await
            .map_err(|e| classify_transport_error(&e, "upstream request failed"))?;

        let status = response.status().as_u16();
        let body = response
            .bytes()
            .await
            .map_err(|e| classify_transport_error(&e, "failed to read upstream response"))?;
        if !(200..300).contains(&status) {
            return Err(GatewayError::new(
                ErrorKind::UpstreamError,
                format!("upstream returned {status}"),
            )
            .with_upstream(status, String::from_utf8_lossy(&body).as_ref()));
        }
        serde_json::from_slice(&body).map_err(|e| {
            GatewayError::new(
                ErrorKind::UpstreamError,
                format!("upstream returned malformed JSON: {e}"),
            )
        })
    }

    /// 发送请求并把响应体解析为 JSON。
    pub async fn send(&self, req: UpstreamRequest<'_>) -> Result<UpstreamResponse, GatewayError> {
        let raw = self.send_raw(req).await?;

        // 空 body 视作 null 而不是报错：某些上游对 204 之类的响应就是空的。
        let body = if raw.body.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&raw.body).map_err(|e| {
                GatewayError::new(
                    ErrorKind::UpstreamError,
                    format!("upstream returned malformed JSON: {e}"),
                )
                .with_upstream(raw.status, snippet(&raw.body))
            })?
        };

        Ok(UpstreamResponse {
            status: raw.status,
            body,
            headers: raw.headers,
            first_byte_to_end: raw.first_byte_to_end,
        })
    }

    /// 发送一次非流式请求并保留成功响应的原始 body。
    pub async fn send_raw(
        &self,
        req: UpstreamRequest<'_>,
    ) -> Result<UpstreamRawResponse, GatewayError> {
        let mut response = self.dispatch(&req).await?;
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let mut body = BytesMut::new();
        let mut first_byte_at = None;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| classify_transport_error(&e, "failed to read upstream response body"))?
        {
            if first_byte_at.is_none() && !chunk.is_empty() {
                first_byte_at = Some(std::time::Instant::now());
            }
            body.extend_from_slice(&chunk);
        }
        let first_byte_to_end = first_byte_at.map_or(Duration::ZERO, |at| at.elapsed());
        let body = body.freeze();

        if !(200..300).contains(&status) {
            return Err(attach_retry_after(
                upstream_status_error(status, &body),
                parse_retry_after(&headers),
            ));
        }

        Ok(UpstreamRawResponse {
            status,
            body,
            headers,
            first_byte_to_end,
        })
    }

    /// 发送一次流式请求，返回逐帧的 SSE 事件流。
    ///
    /// 错误响应在这里就地判定：上游返回 4xx/5xx 时 body 是一个完整的 JSON
    /// 错误对象而非 SSE 流，此时读完它并转成 `GatewayError`，而不是把错误
    /// 文本当成 SSE 帧喂给下游解析器。
    pub async fn stream(
        &self,
        req: UpstreamRequest<'_>,
    ) -> Result<UpstreamSseStream, GatewayError> {
        let response = self.dispatch(&req).await?;
        let status = response.status().as_u16();

        if !(200..300).contains(&status) {
            let retry_after = parse_retry_after(response.headers());
            let bytes = response.bytes().await.unwrap_or_default();
            return Err(attach_retry_after(
                upstream_status_error(status, &bytes),
                retry_after,
            ));
        }

        let headers = response.headers().clone();
        let stream =
            crate::sse::sse_stream(response.bytes_stream(), self.config.stream_idle_timeout);
        Ok(UpstreamSseStream {
            status,
            headers,
            stream,
        })
    }

    /// 发送一次流式请求，返回原始字节流（不做 SSE 解析）。
    ///
    /// 用于「同协议直通」：客户端与上游协议相同时，逐字节转发比
    /// 「解析成事件再重新编码」更快，也不会因为我们不认识某个新字段而丢信息。
    pub async fn stream_raw(
        &self,
        req: UpstreamRequest<'_>,
    ) -> Result<UpstreamRawStream, GatewayError> {
        let response = self.dispatch(&req).await?;
        let status = response.status().as_u16();

        if !(200..300).contains(&status) {
            let retry_after = parse_retry_after(response.headers());
            let bytes = response.bytes().await.unwrap_or_default();
            return Err(attach_retry_after(
                upstream_status_error(status, &bytes),
                retry_after,
            ));
        }

        let headers = response.headers().clone();
        let stream =
            crate::sse::byte_stream(response.bytes_stream(), self.config.stream_idle_timeout);
        Ok(UpstreamRawStream {
            status,
            headers,
            stream,
        })
    }

    /// 解析地址、装配请求头、发出请求。三种发送方式共用。
    async fn dispatch(&self, req: &UpstreamRequest<'_>) -> Result<reqwest::Response, GatewayError> {
        let url = req
            .address
            .resolve(req.protocol, req.action, req.model)
            .map_err(|e| {
                GatewayError::new(ErrorKind::Configuration, e.to_string())
                    .with_protocol(req.protocol)
            })?;

        let method = if req.body.is_some() || req.raw_body.is_some() {
            reqwest::Method::POST
        } else {
            reqwest::Method::GET
        };

        let http = self.http_for(req.proxy)?;
        let mut builder = http
            .request(method, url)
            .headers(auth_headers(req.protocol, req.credential)?);

        // 非流式请求有整体 deadline；流式请求只能限制「多久没有任何数据」。
        // 否则一次持续数分钟但一直正常产出 token 的回答会被误杀。
        if req.action != Action::Stream {
            builder = builder.timeout(req.timeout.unwrap_or(self.config.timeout));
        }

        for (name, value) in req.extra_headers {
            let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                GatewayError::new(
                    ErrorKind::Configuration,
                    format!("invalid upstream header name {name:?}"),
                )
            })?;
            let value = HeaderValue::from_str(value).map_err(|_| {
                GatewayError::new(
                    ErrorKind::Configuration,
                    format!("invalid value for upstream header {name:?}"),
                )
            })?;
            builder = builder.header(name, value);
        }

        if let Some(body) = req.body {
            builder = builder.json(body);
        } else if let Some(body) = req.raw_body {
            builder = builder
                .header(
                    reqwest::header::CONTENT_TYPE,
                    req.raw_content_type.unwrap_or("application/json"),
                )
                .body(body.to_owned());
        }

        if req.action == Action::Stream {
            tokio::time::timeout(self.config.stream_idle_timeout, builder.send())
                .await
                .map_err(|_| {
                    GatewayError::new(
                        ErrorKind::Timeout,
                        format!(
                            "upstream stalled before response headers for {}s",
                            self.config.stream_idle_timeout.as_secs().max(1)
                        ),
                    )
                })?
                .map_err(|e| classify_transport_error(&e, "upstream request failed"))
        } else {
            builder
                .send()
                .await
                .map_err(|e| classify_transport_error(&e, "upstream request failed"))
        }
    }

    /// 选择本次请求的连接池。渠道代理优先于全局代理；相同代理复用同一个池。
    fn http_for(&self, proxy: Option<&str>) -> Result<reqwest::Client, GatewayError> {
        let Some(proxy) = proxy.map(str::trim).filter(|value| !value.is_empty()) else {
            return Ok(self.http.clone());
        };

        if self.config.proxy.as_deref().map(str::trim) == Some(proxy) {
            return Ok(self.http.clone());
        }

        if let Some(client) = self.proxy_clients.get(proxy) {
            return Ok(client.clone());
        }

        let client = build_http_client(&self.config, Some(proxy))?;
        if self.proxy_clients.len() >= MAX_PROXY_CLIENTS
            && let Some(evict) = self
                .proxy_clients
                .iter()
                .next()
                .map(|entry| entry.key().clone())
        {
            self.proxy_clients.remove(&evict);
        }
        self.proxy_clients.insert(proxy.to_owned(), client.clone());
        Ok(client)
    }
}

/// 构建一个连接池。渠道级代理池与默认池必须使用完全相同的网络参数。
fn build_http_client(
    config: &UpstreamClientConfig,
    proxy: Option<&str>,
) -> Result<reqwest::Client, GatewayError> {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(config.connect_timeout)
        .pool_max_idle_per_host(config.pool_max_idle_per_host)
        .pool_idle_timeout(config.pool_idle_timeout)
        .user_agent(config.user_agent.clone())
        // 上游返回 3xx 时不自动跟随：一个 LLM API 端点重定向到别处几乎
        // 总是配置错误（典型是 base_url 少了 /v1），静默跟随会把这个错误
        // 藏起来，还可能把 Authorization 头发给第三方主机。
        .redirect(reqwest::redirect::Policy::none());

    if let Some(proxy) = proxy {
        let configured = reqwest::Proxy::all(proxy).map_err(|e| {
            GatewayError::new(
                ErrorKind::Configuration,
                format!("invalid upstream proxy {proxy:?}: {e}"),
            )
        })?;
        builder = builder.proxy(configured);
    } else {
        builder = builder.no_proxy();
    }

    builder.build().map_err(|e| {
        GatewayError::new(
            ErrorKind::Configuration,
            format!("failed to build upstream http client: {e}"),
        )
    })
}

/// 按协议装配鉴权头。
///
/// 凭据形式是协议的属性，不是渠道的属性：Anthropic 用 `x-api-key`，Google 用
/// `x-goog-api-key`，其余用 `Authorization: Bearer`。聚合渠道的每个端点各按
/// 自己的协议注入（需求 3）。
fn auth_headers(protocol: Protocol, credential: &Credential) -> Result<HeaderMap, GatewayError> {
    let mut headers = HeaderMap::new();

    // 缺凭据是配置错误而不是鉴权错误：错在网关的配置，不在客户端的请求。
    // 分清这一点才能给出可操作的报错。
    if credential.is_empty() {
        return Err(GatewayError::new(
            ErrorKind::Configuration,
            "upstream credential is empty; set an API key on the channel or endpoint",
        )
        .with_protocol(protocol));
    }

    let key = credential.expose();
    let rendered = match protocol.auth_scheme() {
        AuthScheme::Bearer => format!("Bearer {key}"),
        AuthScheme::AnthropicApiKey | AuthScheme::GoogleApiKey => key.to_owned(),
    };
    let mut secret = HeaderValue::from_str(&rendered).map_err(|_| {
        GatewayError::new(
            ErrorKind::Configuration,
            "upstream credential contains characters that cannot be sent in a header",
        )
        .with_protocol(protocol)
    })?;
    // 标记为敏感：hyper 的调试输出与 HPACK 都会尊重这个标记，避免密钥
    // 出现在日志或被 HTTP/2 头部索引缓存。
    secret.set_sensitive(true);

    match protocol.auth_scheme() {
        AuthScheme::Bearer => {
            headers.insert(reqwest::header::AUTHORIZATION, secret);
        }
        AuthScheme::AnthropicApiKey => {
            headers.insert(HeaderName::from_static("x-api-key"), secret);
            headers.insert(
                HeaderName::from_static("anthropic-version"),
                HeaderValue::from_static(ANTHROPIC_VERSION),
            );
        }
        AuthScheme::GoogleApiKey => {
            headers.insert(HeaderName::from_static("x-goog-api-key"), secret);
        }
    }

    Ok(headers)
}

/// `Retry-After` 悬停时长的上限。
///
/// 有的上游会回夸张的值（几小时甚至日期解析出来的负值），悬停太久等价于
/// 手动拉黑 —— 封顶到一小时，超过就当它一小时。
const MAX_RETRY_AFTER: std::time::Duration = std::time::Duration::from_secs(3600);

/// 解析 `Retry-After` 头（只认秒数形式；HTTP-date 形式少见且时钟不可靠）。
fn parse_retry_after(headers: &HeaderMap) -> Option<std::time::Duration> {
    let secs: u64 = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some(std::time::Duration::from_secs(secs).min(MAX_RETRY_AFTER))
}

/// 给错误挂上 `Retry-After` 信息（若有）。
fn attach_retry_after(
    error: GatewayError,
    retry_after: Option<std::time::Duration>,
) -> GatewayError {
    match retry_after {
        Some(wait) => error.with_retry_after(wait),
        None => error,
    }
}

/// 把上游的非 2xx 响应转成网关错误。
///
/// 状态码映射决定了**是否重试**，所以必须精确：429/5xx 可重试，4xx 不可
/// —— 拿着同一个错误的 key 重试十个渠道只会放大问题。
fn upstream_status_error(status: u16, body: &[u8]) -> GatewayError {
    let kind = match status {
        401 => ErrorKind::Unauthenticated,
        403 => ErrorKind::PermissionDenied,
        404 => ErrorKind::NotFound,
        413 => ErrorKind::PayloadTooLarge,
        429 => ErrorKind::RateLimited,
        // 408/504 是上游侧超时，语义上等同于我们自己超时。
        408 | 504 => ErrorKind::Timeout,
        400 | 422 => ErrorKind::InvalidRequest,
        _ => ErrorKind::UpstreamError,
    };

    // 尽量把上游的错误文案提取出来给客户端看，而不是甩一句 "upstream error"。
    let detail =
        extract_error_message(body).unwrap_or_else(|| format!("upstream returned HTTP {status}"));

    GatewayError::new(kind, detail).with_upstream(status, snippet(body))
}

/// 从上游错误体里挖出人类可读的信息。
///
/// 四家的错误结构各不相同，但都能在这几条路径之一里找到：
/// - OpenAI / Anthropic: `{"error": {"message": "..."}}`
/// - Gemini: `{"error": {"message": "..."}}`（同形，但可能是数组包裹）
/// - 中转站: 五花八门，退化到顶层 `message` / `detail`。
fn extract_error_message(body: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(body).ok()?;

    // Gemini 的批量错误会用数组包一层。
    let value = match &value {
        Value::Array(items) => items.first()?.clone(),
        other => other.clone(),
    };

    let candidates = [
        value.pointer("/error/message"),
        value.pointer("/error"),
        value.pointer("/message"),
        value.pointer("/detail"),
        value.pointer("/error_msg"),
    ];

    candidates.into_iter().flatten().find_map(|v| match v {
        Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
        _ => None,
    })
}

/// 传输层错误分类。
///
/// 超时单独分类，其他请求发送、连接和解码失败统一视为上游错误。
fn classify_transport_error(err: &reqwest::Error, context: &str) -> GatewayError {
    let kind = if err.is_timeout() {
        ErrorKind::Timeout
    } else {
        ErrorKind::UpstreamError
    };
    GatewayError::new(kind, format!("{context}: {err}"))
}

/// 截取响应体片段用于诊断。
///
/// 上限 2 KiB：足够看清错误，又不会让一个返回 HTML 错误页的上游把日志刷爆。
fn snippet(body: &[u8]) -> String {
    const MAX: usize = 2048;
    let text = String::from_utf8_lossy(body);
    let trimmed = text.trim();
    if trimmed.chars().count() <= MAX {
        return trimmed.to_owned();
    }
    trimmed.chars().take(MAX).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn official() -> UpstreamAddress {
        UpstreamAddress::OFFICIAL
    }

    #[test]
    fn bearer_scheme_prefixes_the_key() {
        let headers = auth_headers(Protocol::Chat, &Credential::new("sk-abc")).unwrap();
        assert_eq!(
            headers.get(reqwest::header::AUTHORIZATION).unwrap(),
            "Bearer sk-abc"
        );
    }

    #[test]
    fn anthropic_scheme_uses_x_api_key_and_version() {
        let headers = auth_headers(Protocol::Messages, &Credential::new("sk-ant")).unwrap();
        assert_eq!(headers.get("x-api-key").unwrap(), "sk-ant");
        assert_eq!(headers.get("anthropic-version").unwrap(), ANTHROPIC_VERSION);
        // Anthropic 不用 Authorization —— 带上它某些上游会报重复鉴权。
        assert!(headers.get(reqwest::header::AUTHORIZATION).is_none());
    }

    #[test]
    fn google_scheme_uses_goog_header() {
        let headers = auth_headers(Protocol::Gemini, &Credential::new("AIza")).unwrap();
        assert_eq!(headers.get("x-goog-api-key").unwrap(), "AIza");
    }

    #[test]
    fn credentials_are_marked_sensitive() {
        let headers = auth_headers(Protocol::Chat, &Credential::new("sk-abc")).unwrap();
        assert!(
            headers
                .get(reqwest::header::AUTHORIZATION)
                .unwrap()
                .is_sensitive(),
            "an unmarked credential can leak into HPACK tables and debug logs"
        );
    }

    #[test]
    fn empty_credential_is_a_configuration_error() {
        let err = auth_headers(Protocol::Chat, &Credential::new("   ")).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Configuration);
        // 不能报成 401：错的是网关配置，不是客户端。
        assert_eq!(err.status(), 500);
    }

    #[test]
    fn credential_with_newline_is_rejected_not_panicked() {
        let err = auth_headers(Protocol::Chat, &Credential::new("sk-a\nInjected: 1")).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Configuration);
    }

    #[test]
    fn status_mapping_decides_retryability() {
        // 可重试性的语义见 `ErrorKind::is_retryable`：上游 401/403 表示这条渠道
        // 的密钥失效，换渠道是对的处理，因此可重试。
        let cases = [
            (400, ErrorKind::InvalidRequest, false),
            (401, ErrorKind::Unauthenticated, true),
            (403, ErrorKind::PermissionDenied, true),
            (404, ErrorKind::NotFound, false),
            (413, ErrorKind::PayloadTooLarge, false),
            (422, ErrorKind::InvalidRequest, false),
            (429, ErrorKind::RateLimited, true),
            (500, ErrorKind::UpstreamError, true),
            (502, ErrorKind::UpstreamError, true),
            (503, ErrorKind::UpstreamError, true),
            (504, ErrorKind::Timeout, true),
        ];
        for (status, kind, retryable) in cases {
            let err = upstream_status_error(status, b"{}");
            assert_eq!(err.kind, kind, "status {status}");
            assert_eq!(
                err.is_retryable(),
                retryable,
                "status {status} retryability"
            );
            assert_eq!(err.upstream_status, Some(status));
        }
    }

    #[test]
    fn openai_error_message_is_surfaced() {
        let body = br#"{"error":{"message":"model not found","type":"invalid_request_error"}}"#;
        let err = upstream_status_error(404, body);
        assert_eq!(err.message, "model not found");
    }

    #[test]
    fn gemini_array_wrapped_error_is_surfaced() {
        let body =
            br#"[{"error":{"code":429,"message":"quota exceeded","status":"RESOURCE_EXHAUSTED"}}]"#;
        let err = upstream_status_error(429, body);
        assert_eq!(err.message, "quota exceeded");
    }

    #[test]
    fn relay_style_flat_message_is_surfaced() {
        let err = upstream_status_error(500, br#"{"message":"no available channel"}"#);
        assert_eq!(err.message, "no available channel");
    }

    #[test]
    fn error_as_bare_string_is_surfaced() {
        let err = upstream_status_error(500, br#"{"error":"boom"}"#);
        assert_eq!(err.message, "boom");
    }

    #[test]
    fn non_json_error_body_falls_back_to_status_text() {
        let err = upstream_status_error(502, b"<html>Bad Gateway</html>");
        assert_eq!(err.message, "upstream returned HTTP 502");
        // 原始片段仍要保留，否则排查时完全看不到上游说了什么。
        assert_eq!(
            err.upstream_body.as_deref(),
            Some("<html>Bad Gateway</html>")
        );
    }

    #[test]
    fn empty_error_message_does_not_win_over_fallback() {
        let err = upstream_status_error(500, br#"{"error":{"message":"   "}}"#);
        assert_eq!(err.message, "upstream returned HTTP 500");
    }

    #[test]
    fn snippet_is_capped_on_char_boundaries() {
        let body = "错".repeat(3000).into_bytes();
        let text = snippet(&body);
        assert_eq!(text.chars().count(), 2048);
        assert!(text.starts_with('错'));
    }

    #[test]
    fn invalid_proxy_is_reported_at_construction() {
        let err = UpstreamClient::new(UpstreamClientConfig {
            proxy: Some("not a url".into()),
            ..Default::default()
        })
        .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Configuration);
    }

    #[test]
    fn client_builds_with_defaults() {
        assert!(UpstreamClient::new(UpstreamClientConfig::default()).is_ok());
    }

    #[test]
    fn request_helpers_pick_the_right_method() {
        let addr = official();
        let cred = Credential::new("k");
        let body = serde_json::json!({"model": "m"});
        let post =
            UpstreamRequest::post(Protocol::Chat, &addr, &cred, "m", Action::Generate, &body);
        assert!(post.body.is_some());

        let list = UpstreamRequest::list_models(Protocol::Chat, &addr, &cred);
        assert!(list.body.is_none());
        assert_eq!(list.action, Action::ListModels);
    }
}
