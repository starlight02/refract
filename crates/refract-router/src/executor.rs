//! 路由执行：把路由计划真正打到上游，失败则按计划重试。
//!
//! 这一层的核心判断是**「什么时候该换渠道」**。判据只有一个：
//! [`refract_core::ErrorKind::is_retryable`]。它由错误的来源决定，而不是由
//! 状态码字面决定 —— 上游 401 意味着这条渠道的密钥废了，换渠道是对的；
//! 客户端请求体不合法则换到哪都一样错。
//!
//! ## 熔断与候选过滤
//!
//! 熔断中的端点被排到最后而不是直接删掉。理由：全部端点都熔断时，打一个
//! 可能失败的上游仍优于确定失败的 503 —— 熔断常常只是上游的短暂抖动。

use std::time::Duration;

use bytes::Bytes;
use futures_util::{Stream, StreamExt as _};
use refract_core::{Action, ChannelId, ErrorKind, GatewayError, Protocol};
use refract_protocol::codec::CodecSet;
use refract_protocol::ir::{UnifiedRequest, UnifiedResponse, Usage};
use refract_protocol::{SseFrame, SseParser, StreamEvent};
use refract_store::HealthRepo;
use refract_upstream::{
    ByteStream, SseStream, UpstreamClient, UpstreamRawResponse, UpstreamRawStream, UpstreamRequest,
};
use serde_json::Value;

use crate::plan::{Candidate, Route};

/// 执行器配置。
#[derive(Debug, Clone)]
pub struct RouterConfig {
    /// 熔断中的端点是否仍可作为最后手段。
    pub allow_suspended_as_last_resort: bool,
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            allow_suspended_as_last_resort: true,
        }
    }
}

/// 一次成功执行的结果。
#[derive(Debug)]
pub struct RouteOutcome<T> {
    /// 命中的渠道 ID。
    pub channel_id: ChannelId,
    /// 命中的渠道名，用于日志。
    pub channel_name: String,
    /// 上游端点的原生协议。
    pub upstream_protocol: Protocol,
    /// 实际发给上游的模型名。
    pub upstream_model: String,
    /// 是否发生了协议转换。
    pub transcoded: bool,
    /// 尝试次数（含成功那次）。
    pub attempts: u8,
    /// 首字节延迟（毫秒）。
    pub latency_ms: u64,
    /// 载荷。
    pub payload: T,
}

impl<T> RouteOutcome<T> {
    /// 换掉载荷，保留元数据。
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> RouteOutcome<U> {
        RouteOutcome {
            channel_id: self.channel_id,
            channel_name: self.channel_name,
            upstream_protocol: self.upstream_protocol,
            upstream_model: self.upstream_model,
            transcoded: self.transcoded,
            attempts: self.attempts,
            latency_ms: self.latency_ms,
            payload: f(self.payload),
        }
    }
}

/// 一次入口请求的执行载荷。
///
/// 管理测试等内部调用可以只给 IR；真实网关同时附带原始 JSON，使同协议候选
/// 能逐字节转发，只有模型别名、参数覆盖或协议转换时才重新编码。
#[derive(Debug, Clone, Copy)]
pub enum InboundPayload<'a> {
    /// 已由调用方构造好的 IR，主要用于内部调用和测试。
    Normalized(&'a UnifiedRequest),
    /// 真实 HTTP 请求；完整 IR 只在协议转换候选执行时构造。
    Raw {
        /// 客户端入口协议。
        protocol: Protocol,
        /// 原始 JSON 字节。
        body: &'a [u8],
        /// 对外模型名。
        model: &'a str,
        /// 是否请求流式响应。
        stream: bool,
        /// 需要透传给上游的入站请求头（已由调用方按白名单过滤）。
        ///
        /// 只在**同协议直通**时随请求发出：`anthropic-beta` 这类头的语义
        /// 绑定在具体协议上，转码后发给别的协议只会引来 400。
        headers: &'a [(String, String)],
    },
}

impl<'a> InboundPayload<'a> {
    /// 构造带原始请求的生产载荷。
    pub const fn raw(protocol: Protocol, body: &'a [u8], model: &'a str, stream: bool) -> Self {
        Self::Raw {
            protocol,
            body,
            model,
            stream,
            headers: &[],
        }
    }

    /// 附上需要透传的入站请求头（白名单过滤后的键值对）。
    #[must_use]
    pub fn with_headers(mut self, headers: &'a [(String, String)]) -> Self {
        if let Self::Raw { headers: slot, .. } = &mut self {
            *slot = headers;
        }
        self
    }
}

impl<'a> From<&'a UnifiedRequest> for InboundPayload<'a> {
    fn from(ir: &'a UnifiedRequest) -> Self {
        Self::Normalized(ir)
    }
}

/// 非流式路由载荷：原生协议保留原始 HTTP 响应，跨协议才返回 IR。
#[derive(Debug)]
pub enum RoutedResponse {
    /// 同协议直通。
    Native {
        /// 上游成功响应的状态、headers 与原始 body。
        response: UpstreamRawResponse,
        /// 尽力从响应副本提取的用量；解析失败不影响直通。
        usage: Usage,
    },
    /// 跨协议转换后的统一响应。
    Transcoded(UnifiedResponse),
}

impl RoutedResponse {
    /// 用于日志与密钥用量统计。
    pub const fn usage(&self) -> Usage {
        match self {
            Self::Native { usage, .. } => *usage,
            Self::Transcoded(response) => response.usage,
        }
    }
}

/// 流式路由载荷。原生流保持字节，跨协议流输出已解析的 SSE 事件。
pub enum RoutedStream {
    /// 同协议原始字节流。
    Native(UpstreamRawStream),
    /// 需要协议转换的 SSE 事件流。
    Transcoded(SseStream),
}

/// 路由执行器。
#[derive(Clone)]
pub struct RouteExecutor {
    client: UpstreamClient,
    codecs: CodecSet,
    health: HealthRepo,
    config: RouterConfig,
}

impl std::fmt::Debug for RouteExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RouteExecutor")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl RouteExecutor {
    /// 构造执行器。
    pub fn new(
        client: UpstreamClient,
        codecs: CodecSet,
        health: HealthRepo,
        config: RouterConfig,
    ) -> Self {
        Self {
            client,
            codecs,
            health,
            config,
        }
    }

    /// 当前配置。
    pub fn config(&self) -> &RouterConfig {
        &self.config
    }

    /// 健康仓储，供上层查询/重置熔断。
    pub fn health(&self) -> &HealthRepo {
        &self.health
    }

    /// 把候选按健康度重排：熔断中的排到最后，再应用 `max_attempts` 上限。
    ///
    /// 截断必须发生在**重排之后** —— 若规划阶段就截断，前 N 名全在熔断中时，
    /// 健康的第 N+1 名永远轮不到，熔断机制反而放大了故障。
    ///
    /// 只读 [`HealthRepo`] 的进程内熔断缓存，不碰数据库 —— 这个判断在每个
    /// 请求的热路径上对每个候选都要做一次。返回的是下标序列而非重排后的
    /// `Vec`，避免复制候选。
    pub fn prioritize(&self, route: &Route<'_>) -> Vec<usize> {
        let mut healthy = Vec::with_capacity(route.attempts.len());
        let mut suspended = Vec::new();

        for (idx, candidate) in route.attempts.iter().enumerate() {
            let is_down = self
                .health
                .suspended_until(candidate.channel_id(), candidate.protocol())
                .is_some();
            if is_down {
                suspended.push(idx);
            } else {
                healthy.push(idx);
            }
        }

        if self.config.allow_suspended_as_last_resort {
            healthy.extend(suspended);
        }
        healthy.truncate(route.attempt_cap.max(1));
        healthy
    }

    /// 把 IR 请求编码成目标端点要的 JSON。
    ///
    /// 同协议时也走一遍 encode：客户端可能用了我们已归一化的别名字段
    /// （如 `max_completion_tokens`），原样转发会把兼容性问题甩给用户。
    fn encode_for(
        &self,
        candidate: &Candidate<'_>,
        ir: &UnifiedRequest,
    ) -> Result<Value, GatewayError> {
        let protocol = candidate.protocol();
        let mut body = self.codecs.for_protocol(protocol).encode_request(ir)?;

        rewrite_model(&mut body, protocol, candidate.upstream_model());

        // 渠道级参数覆盖最后应用，允许用户强制某些字段。
        apply_param_override(&mut body, &candidate.channel.param_override, protocol);

        Ok(body)
    }

    /// 候选的请求超时：渠道配了就用渠道的，否则用客户端默认。
    fn timeout_for(&self, candidate: &Candidate<'_>) -> Option<Duration> {
        match candidate.channel.timeout_secs {
            0 => None,
            secs => Some(Duration::from_secs(u64::from(secs))),
        }
    }

    /// 仅有归一化 IR 时的非流式执行。
    async fn execute_normalized(
        &self,
        route: &Route<'_>,
        ir: &UnifiedRequest,
    ) -> Result<RouteOutcome<RoutedResponse>, GatewayError> {
        let order = self.prioritize(route);
        let mut last_error = no_candidates(route);
        let mut attempts = 0_u8;

        for idx in order {
            let candidate = &route.attempts[idx];
            attempts = attempts.saturating_add(1);
            let body = match self.encode_for(candidate, ir) {
                Ok(b) => b,
                Err(e) => {
                    // 编码失败是我们自己的问题，不是上游的 —— 不记健康度，
                    // 但仍然换下一个候选：目标协议不同，可能就能编码成功。
                    last_error = e;
                    continue;
                }
            };

            let started = std::time::Instant::now();
            let mut req = UpstreamRequest::post(
                candidate.protocol(),
                candidate.address(),
                candidate.credential(),
                candidate.upstream_model(),
                Action::Generate,
                &body,
            );
            req.proxy = candidate.proxy();
            req.timeout = self.timeout_for(candidate);

            match self.client.send(req).await {
                Ok(response) => {
                    let latency_ms = started.elapsed().as_millis() as u64;
                    match self
                        .codecs
                        .for_protocol(candidate.protocol())
                        .decode_response(&response.body)
                    {
                        Ok(payload) => {
                            self.record_success(candidate, latency_ms).await;
                            return Ok(self.outcome(
                                candidate,
                                route,
                                attempts,
                                latency_ms,
                                RoutedResponse::Transcoded(payload),
                            ));
                        }
                        Err(e) => {
                            // 上游回了 200 但我们看不懂 —— 这是上游的问题
                            // （或它根本不是这个协议），记失败并换渠道。
                            let e = annotate_candidate_error(e, candidate, attempts);
                            self.record_failure(candidate, &e).await;
                            last_error = e;
                        }
                    }
                }
                Err(e) => {
                    let e = annotate_candidate_error(e, candidate, attempts);
                    self.record_failure(candidate, &e).await;
                    if !e.is_retryable() {
                        return Err(e);
                    }
                    last_error = e;
                }
            }
        }

        Err(last_error)
    }

    /// 非流式执行：生产请求在原生协议时保留原始字节，内部 IR 调用走规范化路径。
    pub async fn execute<'a>(
        &self,
        route: &Route<'_>,
        payload: impl Into<InboundPayload<'a>>,
    ) -> Result<RouteOutcome<RoutedResponse>, GatewayError> {
        let payload = payload.into();
        let InboundPayload::Raw {
            protocol,
            body: raw_body,
            model,
            stream,
            headers,
        } = payload
        else {
            let InboundPayload::Normalized(ir) = payload else {
                unreachable!();
            };
            return self.execute_normalized(route, ir).await;
        };
        let mut decoded_ir: Option<Result<UnifiedRequest, GatewayError>> = None;
        let order = self.prioritize(route);
        let mut last_error = no_candidates(route);
        let mut attempts = 0_u8;

        for idx in order {
            let candidate = &route.attempts[idx];
            attempts = attempts.saturating_add(1);
            let started = std::time::Instant::now();

            if candidate.needs_transcode(route.inbound) {
                let ir = match decoded_ir.get_or_insert_with(|| {
                    decode_raw_request(self.codecs, protocol, raw_body, model, stream)
                }) {
                    Ok(ir) => ir,
                    Err(error) => {
                        // 解码失败只说明这个请求转不了码 —— 后面的同协议候选
                        // 走字节直通，不需要 IR，不能被这里误杀。
                        last_error = error.clone();
                        continue;
                    }
                };
                let body = match self.encode_for(candidate, ir) {
                    Ok(body) => body,
                    Err(error) => {
                        last_error = annotate_candidate_error(error, candidate, attempts);
                        continue;
                    }
                };
                let mut request = UpstreamRequest::post(
                    candidate.protocol(),
                    candidate.address(),
                    candidate.credential(),
                    candidate.upstream_model(),
                    Action::Generate,
                    &body,
                );
                request.proxy = candidate.proxy();
                request.timeout = self.timeout_for(candidate);

                match self.client.send(request).await {
                    Ok(response) => {
                        let latency_ms = started.elapsed().as_millis() as u64;
                        match self
                            .codecs
                            .for_protocol(candidate.protocol())
                            .decode_response(&response.body)
                        {
                            Ok(payload) => {
                                self.record_success(candidate, latency_ms).await;
                                return Ok(self.outcome(
                                    candidate,
                                    route,
                                    attempts,
                                    latency_ms,
                                    RoutedResponse::Transcoded(payload),
                                ));
                            }
                            Err(error) => {
                                let error = annotate_candidate_error(error, candidate, attempts);
                                self.record_failure(candidate, &error).await;
                                last_error = error;
                            }
                        }
                    }
                    Err(error) => {
                        let error = annotate_candidate_error(error, candidate, attempts);
                        self.record_failure(candidate, &error).await;
                        if !error.is_retryable() {
                            return Err(error);
                        }
                        last_error = error;
                    }
                }
                continue;
            }

            let prepared = prepare_native_body(candidate, route, raw_body)?;
            let mut request = match &prepared {
                PreparedBody::Raw(body) => UpstreamRequest::post_raw(
                    candidate.protocol(),
                    candidate.address(),
                    candidate.credential(),
                    candidate.upstream_model(),
                    Action::Generate,
                    body,
                ),
                PreparedBody::Json(body) => UpstreamRequest::post(
                    candidate.protocol(),
                    candidate.address(),
                    candidate.credential(),
                    candidate.upstream_model(),
                    Action::Generate,
                    body,
                ),
            };
            request.extra_headers = headers;
            request.proxy = candidate.proxy();
            request.timeout = self.timeout_for(candidate);

            match self.client.send_raw(request).await {
                Ok(response) => {
                    let latency_ms = started.elapsed().as_millis() as u64;
                    let usage = match inspect_native_response(
                        self.codecs,
                        candidate.protocol(),
                        response.body.as_ref(),
                    ) {
                        Ok(usage) => usage,
                        Err(error) => {
                            let error = annotate_candidate_error(error, candidate, attempts);
                            self.record_failure(candidate, &error).await;
                            if !error.is_retryable() {
                                return Err(error);
                            }
                            last_error = error;
                            continue;
                        }
                    };
                    self.record_success(candidate, latency_ms).await;
                    return Ok(self.outcome(
                        candidate,
                        route,
                        attempts,
                        latency_ms,
                        RoutedResponse::Native { response, usage },
                    ));
                }
                Err(error) => {
                    let error = annotate_candidate_error(error, candidate, attempts);
                    self.record_failure(candidate, &error).await;
                    if !error.is_retryable() {
                        return Err(error);
                    }
                    last_error = error;
                }
            }
        }

        Err(last_error)
    }

    /// 非对话端点（如 `/v1/embeddings`）的字节透传执行。
    ///
    /// 与 [`Self::execute`] 的差别：这类请求**没有跨协议转换语义**——不经过
    /// IR/codec，请求与响应字节原样往返，只做模型别名与参数覆盖的顶层改写。
    /// 候选必须与入口协议同构（调用方过滤，这里再防御一层）；熔断、健康记录、
    /// 重试语义与对话请求完全一致。
    pub async fn execute_passthrough(
        &self,
        route: &Route<'_>,
        action: Action,
        raw_body: &[u8],
        headers: &[(String, String)],
    ) -> Result<RouteOutcome<UpstreamRawResponse>, GatewayError> {
        let order = self.prioritize(route);
        let mut last_error = no_candidates(route);
        let mut attempts = 0_u8;

        for idx in order {
            let candidate = &route.attempts[idx];
            if candidate.protocol() != route.inbound {
                continue;
            }
            attempts = attempts.saturating_add(1);
            let started = std::time::Instant::now();

            let prepared = prepare_native_body(candidate, route, raw_body)?;
            let mut request = match &prepared {
                PreparedBody::Raw(body) => UpstreamRequest::post_raw(
                    candidate.protocol(),
                    candidate.address(),
                    candidate.credential(),
                    candidate.upstream_model(),
                    action,
                    body,
                ),
                PreparedBody::Json(body) => UpstreamRequest::post(
                    candidate.protocol(),
                    candidate.address(),
                    candidate.credential(),
                    candidate.upstream_model(),
                    action,
                    body,
                ),
            };
            request.extra_headers = headers;
            request.proxy = candidate.proxy();
            request.timeout = self.timeout_for(candidate);

            match self.client.send_raw(request).await {
                Ok(response) => {
                    let latency_ms = started.elapsed().as_millis() as u64;
                    self.record_success(candidate, latency_ms).await;
                    return Ok(self.outcome(candidate, route, attempts, latency_ms, response));
                }
                Err(error) => {
                    let error = annotate_candidate_error(error, candidate, attempts);
                    self.record_failure(candidate, &error).await;
                    if !error.is_retryable() {
                        return Err(error);
                    }
                    last_error = error;
                }
            }
        }

        Err(last_error)
    }

    /// 流式执行。
    ///
    /// 与非流式的关键差别：**只有「建流失败」能重试**。一旦第一帧已经发给
    /// 客户端，中途换渠道会产生两段互相矛盾的响应体 —— 那比直接报错更糟。
    async fn execute_stream_normalized(
        &self,
        route: &Route<'_>,
        ir: &UnifiedRequest,
    ) -> Result<RouteOutcome<RoutedStream>, GatewayError> {
        let order = self.prioritize(route);
        let mut last_error = no_candidates(route);
        let mut attempts = 0_u8;

        for idx in order {
            let candidate = &route.attempts[idx];
            attempts = attempts.saturating_add(1);
            let body = match self.encode_for(candidate, ir) {
                Ok(b) => b,
                Err(e) => {
                    last_error = e;
                    continue;
                }
            };

            let started = std::time::Instant::now();
            let mut req = UpstreamRequest::post(
                candidate.protocol(),
                candidate.address(),
                candidate.credential(),
                candidate.upstream_model(),
                Action::Stream,
                &body,
            );
            req.proxy = candidate.proxy();
            // 流式请求不设整体超时：上游客户端只按 stream_idle_timeout 保护，
            // 整体 deadline 会误杀持续产出 token 的长回答。

            match self.client.stream(req).await {
                Ok(stream) => match preflight_sse_stream(
                    stream,
                    self.codecs,
                    candidate.protocol(),
                    self.client.config().stream_idle_timeout,
                )
                .await
                {
                    Ok((prefix, rest)) => {
                        let latency_ms = started.elapsed().as_millis() as u64;
                        let mut prefix = prefix.into_iter();
                        let first = prefix.next().expect("preflight returns a non-empty prefix");
                        let rest = Box::pin(futures_util::stream::iter(prefix.map(Ok)).chain(rest));
                        let tracked = track_stream(
                            first,
                            rest,
                            self.health.clone(),
                            candidate.channel_id(),
                            candidate.protocol(),
                            latency_ms,
                        );
                        return Ok(self.outcome(
                            candidate,
                            route,
                            attempts,
                            latency_ms,
                            RoutedStream::Transcoded(tracked),
                        ));
                    }
                    Err(error) => {
                        let error = annotate_candidate_error(error, candidate, attempts);
                        self.record_failure(candidate, &error).await;
                        if !error.is_retryable() {
                            return Err(error);
                        }
                        last_error = error;
                    }
                },
                Err(e) => {
                    let e = annotate_candidate_error(e, candidate, attempts);
                    self.record_failure(candidate, &e).await;
                    if !e.is_retryable() {
                        return Err(e);
                    }
                    last_error = e;
                }
            }
        }

        Err(last_error)
    }

    /// 流式执行：原生协议使用字节流，跨协议才解析并转码 SSE。
    pub async fn execute_stream<'a>(
        &self,
        route: &Route<'_>,
        payload: impl Into<InboundPayload<'a>>,
    ) -> Result<RouteOutcome<RoutedStream>, GatewayError> {
        let payload = payload.into();
        let InboundPayload::Raw {
            protocol,
            body: raw_body,
            model,
            stream,
            headers,
        } = payload
        else {
            let InboundPayload::Normalized(ir) = payload else {
                unreachable!();
            };
            return self.execute_stream_normalized(route, ir).await;
        };
        let mut decoded_ir: Option<Result<UnifiedRequest, GatewayError>> = None;
        let order = self.prioritize(route);
        let mut last_error = no_candidates(route);
        let mut attempts = 0_u8;

        for idx in order {
            let candidate = &route.attempts[idx];
            attempts = attempts.saturating_add(1);
            let started = std::time::Instant::now();

            if candidate.needs_transcode(route.inbound) {
                let ir = match decoded_ir.get_or_insert_with(|| {
                    decode_raw_request(self.codecs, protocol, raw_body, model, stream)
                }) {
                    Ok(ir) => ir,
                    Err(error) => {
                        // 同上：解码失败不能杀掉后面走字节直通的同协议候选。
                        last_error = error.clone();
                        continue;
                    }
                };
                let body = match self.encode_for(candidate, ir) {
                    Ok(body) => body,
                    Err(error) => {
                        last_error = annotate_candidate_error(error, candidate, attempts);
                        continue;
                    }
                };
                let mut request = UpstreamRequest::post(
                    candidate.protocol(),
                    candidate.address(),
                    candidate.credential(),
                    candidate.upstream_model(),
                    Action::Stream,
                    &body,
                );
                request.proxy = candidate.proxy();

                match self.client.stream(request).await {
                    Ok(stream) => match preflight_sse_stream(
                        stream,
                        self.codecs,
                        candidate.protocol(),
                        self.client.config().stream_idle_timeout,
                    )
                    .await
                    {
                        Ok((prefix, rest)) => {
                            let latency_ms = started.elapsed().as_millis() as u64;
                            let stream = prepend_stream(prefix, rest);
                            return Ok(self.outcome(
                                candidate,
                                route,
                                attempts,
                                latency_ms,
                                RoutedStream::Transcoded(stream),
                            ));
                        }
                        Err(error) => {
                            let error = annotate_candidate_error(error, candidate, attempts);
                            self.record_failure(candidate, &error).await;
                            if !error.is_retryable() {
                                return Err(error);
                            }
                            last_error = error;
                        }
                    },
                    Err(error) => {
                        let error = annotate_candidate_error(error, candidate, attempts);
                        self.record_failure(candidate, &error).await;
                        if !error.is_retryable() {
                            return Err(error);
                        }
                        last_error = error;
                    }
                }
                continue;
            }

            let prepared = prepare_native_body(candidate, route, raw_body)?;
            let mut request = match &prepared {
                PreparedBody::Raw(body) => UpstreamRequest::post_raw(
                    candidate.protocol(),
                    candidate.address(),
                    candidate.credential(),
                    candidate.upstream_model(),
                    Action::Stream,
                    body,
                ),
                PreparedBody::Json(body) => UpstreamRequest::post(
                    candidate.protocol(),
                    candidate.address(),
                    candidate.credential(),
                    candidate.upstream_model(),
                    Action::Stream,
                    body,
                ),
            };
            request.extra_headers = headers;
            request.proxy = candidate.proxy();

            match self.client.stream_raw(request).await {
                Ok(response) => match preflight_raw_stream(
                    response.stream,
                    self.codecs,
                    candidate.protocol(),
                    self.client.config().stream_idle_timeout,
                )
                .await
                {
                    Ok((prefix, rest)) => {
                        let latency_ms = started.elapsed().as_millis() as u64;
                        let stream = prepend_stream(prefix, rest);
                        return Ok(self.outcome(
                            candidate,
                            route,
                            attempts,
                            latency_ms,
                            RoutedStream::Native(UpstreamRawStream {
                                status: response.status,
                                headers: response.headers,
                                stream,
                            }),
                        ));
                    }
                    Err(error) => {
                        let error = annotate_candidate_error(error, candidate, attempts);
                        self.record_failure(candidate, &error).await;
                        if !error.is_retryable() {
                            return Err(error);
                        }
                        last_error = error;
                    }
                },
                Err(error) => {
                    let error = annotate_candidate_error(error, candidate, attempts);
                    self.record_failure(candidate, &error).await;
                    if !error.is_retryable() {
                        return Err(error);
                    }
                    last_error = error;
                }
            }
        }

        Err(last_error)
    }

    fn outcome<T>(
        &self,
        candidate: &Candidate<'_>,
        route: &Route<'_>,
        attempts: u8,
        latency_ms: u64,
        payload: T,
    ) -> RouteOutcome<T> {
        RouteOutcome {
            channel_id: candidate.channel_id(),
            channel_name: candidate.channel.name.clone(),
            upstream_protocol: candidate.protocol(),
            upstream_model: candidate.upstream_model().to_owned(),
            transcoded: candidate.needs_transcode(route.inbound),
            attempts,
            latency_ms,
            payload,
        }
    }

    async fn record_success(&self, candidate: &Candidate<'_>, latency_ms: u64) {
        if let Err(e) = self
            .health
            .record_success(candidate.channel_id(), candidate.protocol(), latency_ms)
            .await
        {
            // 健康度写失败不能影响请求结果 —— 它是可观测性，不是正确性。
            tracing::warn!(error = %e, "failed to record upstream success");
        }
    }

    async fn record_failure(&self, candidate: &Candidate<'_>, error: &GatewayError) {
        if !affects_endpoint_health(error.kind) {
            return;
        }
        if let Err(e) = self
            .health
            .record_failure(
                candidate.channel_id(),
                candidate.protocol(),
                &error.to_string(),
                error.retry_after,
            )
            .await
        {
            tracing::warn!(error = %e, "failed to record upstream failure");
        }
    }
}

enum PreparedBody<'a> {
    Raw(&'a [u8]),
    Json(Value),
}

/// 原生协议通常直接使用原始字节；模型别名或渠道覆盖只做顶层最小改写。
fn prepare_native_body<'a>(
    candidate: &Candidate<'_>,
    route: &Route<'_>,
    raw_body: &'a [u8],
) -> Result<PreparedBody<'a>, GatewayError> {
    let protocol = candidate.protocol();
    let rewrites_model = protocol != Protocol::Gemini && candidate.upstream_model() != route.model;
    let has_overrides = param_override_touches(&candidate.channel.param_override, protocol);
    if !rewrites_model && !has_overrides {
        return Ok(PreparedBody::Raw(raw_body));
    }

    let mut body: Value = serde_json::from_slice(raw_body).map_err(|error| {
        GatewayError::invalid_request(format!("malformed request body: {error}"))
    })?;
    if rewrites_model {
        rewrite_model(&mut body, protocol, candidate.upstream_model());
    }
    apply_param_override(&mut body, &candidate.channel.param_override, protocol);
    Ok(PreparedBody::Json(body))
}

/// 把渠道级参数覆盖合并进请求体，支持按协议分组。
///
/// 覆盖对象里键名恰好是协议名（`chat` / `responses` / `messages` / `gemini`）
/// 且值为对象的条目，被视为**该协议专属**的覆盖组：只有打到对应协议端点时
/// 才展开合并。其余顶层键对所有端点生效 —— 这是单协议渠道的常见写法。
///
/// 没有这个分组机制时，聚合渠道的顶层覆盖（如 `temperature`）会被盲注进
/// Gemini 的请求体顶层，而 Gemini 的采样参数在 `generationConfig` 里，
/// 顶层未知字段直接 400。
fn apply_param_override(body: &mut Value, param_override: &Option<Value>, protocol: Protocol) {
    let (Some(Value::Object(overrides)), Value::Object(map)) = (param_override, body) else {
        return;
    };
    for (key, value) in overrides {
        match protocol_group(key, value) {
            Some(group_protocol) => {
                if group_protocol == protocol
                    && let Value::Object(group) = value
                {
                    for (k, v) in group {
                        map.insert(k.clone(), v.clone());
                    }
                }
            }
            None => {
                map.insert(key.clone(), value.clone());
            }
        }
    }
}

/// 覆盖对本协议是否有实际效果 —— 决定原生直通要不要为此重编码请求体。
fn param_override_touches(param_override: &Option<Value>, protocol: Protocol) -> bool {
    let Some(Value::Object(overrides)) = param_override else {
        return false;
    };
    overrides
        .iter()
        .any(|(key, value)| match protocol_group(key, value) {
            Some(group_protocol) => {
                group_protocol == protocol
                    && matches!(value, Value::Object(group) if !group.is_empty())
            }
            None => true,
        })
}

/// 键名是协议名且值为对象时，识别为协议分组。
///
/// 值必须是对象才算分组：`messages` 同时也是 Chat/Messages 协议请求体的
/// 字段名，但那个字段的值是数组 —— 用值的形状消除歧义。
fn protocol_group(key: &str, value: &Value) -> Option<Protocol> {
    if !value.is_object() {
        return None;
    }
    key.parse::<Protocol>().ok()
}

fn decode_raw_request(
    codecs: CodecSet,
    protocol: Protocol,
    raw_body: &[u8],
    model: &str,
    stream: bool,
) -> Result<UnifiedRequest, GatewayError> {
    let value: Value = serde_json::from_slice(raw_body).map_err(|error| {
        GatewayError::invalid_request(format!("malformed request body: {error}"))
    })?;
    let mut ir = codecs.for_protocol(protocol).decode_request(&value)?;
    ir.model = model.to_owned();
    ir.stream = stream;
    Ok(ir)
}

fn inspect_native_response(
    codecs: CodecSet,
    protocol: Protocol,
    body: &[u8],
) -> Result<Usage, GatewayError> {
    let value: Value = serde_json::from_slice(body).map_err(|error| {
        GatewayError::new(
            ErrorKind::UpstreamError,
            format!("upstream returned malformed JSON: {error}"),
        )
    })?;

    let explicit_error = value.get("error").is_some_and(|error| !error.is_null())
        || value.get("type").and_then(Value::as_str) == Some("error")
        || value.get("type").and_then(Value::as_str) == Some("response.failed")
        || value.pointer("/response/status").and_then(Value::as_str) == Some("failed");
    let decoded = codecs.for_protocol(protocol).decode_response(&value);
    if explicit_error {
        return Err(decoded.err().unwrap_or_else(|| {
            GatewayError::new(
                ErrorKind::UpstreamError,
                "upstream returned an error envelope with HTTP 2xx",
            )
        }));
    }

    // 未知的新成功形状不得阻断原生直通；能识别时提取 usage，不能识别时为 0。
    Ok(decoded.map(|response| response.usage).unwrap_or_default())
}

const STREAM_PREFLIGHT_LIMIT: usize = 1024 * 1024;

/// 解析型流在交给客户端前必须得到一个协议层有效事件，而不只是 HTTP 2xx。
async fn preflight_sse_stream(
    mut stream: SseStream,
    codecs: CodecSet,
    protocol: Protocol,
    timeout: Duration,
) -> Result<(Vec<refract_upstream::sse::SseEvent>, SseStream), GatewayError> {
    tokio::time::timeout(timeout, async move {
        let mut decoder = codecs.for_protocol(protocol).stream_decoder();
        let mut prefix = Vec::new();
        let mut bytes = 0_usize;

        while let Some(item) = stream.next().await {
            let event = item?;
            bytes = bytes.saturating_add(event.event.len() + event.data.len());
            if bytes > STREAM_PREFLIGHT_LIMIT {
                return Err(GatewayError::new(
                    ErrorKind::UpstreamError,
                    "upstream produced over 1 MiB before its first valid stream event",
                ));
            }
            let frame = SseFrame {
                event: (!event.event.is_empty()).then(|| event.event.clone()),
                data: event.data.clone(),
            };
            let events = decoder.decode(&frame)?;
            let meaningful = validate_stream_events(&events)?;
            prefix.push(event);
            if meaningful {
                return Ok((prefix, stream));
            }
        }

        Err(GatewayError::new(
            ErrorKind::UpstreamError,
            "upstream stream ended before its first valid event",
        ))
    })
    .await
    .map_err(|_| {
        GatewayError::new(
            ErrorKind::Timeout,
            "upstream did not produce a valid first stream event before the idle deadline",
        )
    })?
}

/// 原生字节流预检完整 SSE 帧，同时保留已读取 chunk 供原样回放。
async fn preflight_raw_stream(
    mut stream: ByteStream,
    codecs: CodecSet,
    protocol: Protocol,
    timeout: Duration,
) -> Result<(Vec<Bytes>, ByteStream), GatewayError> {
    tokio::time::timeout(timeout, async move {
        let mut parser = SseParser::new();
        let mut decoder = codecs.for_protocol(protocol).stream_decoder();
        let mut prefix = Vec::new();
        let mut bytes = 0_usize;

        while let Some(item) = stream.next().await {
            let chunk = item?;
            bytes = bytes.saturating_add(chunk.len());
            if bytes > STREAM_PREFLIGHT_LIMIT {
                return Err(GatewayError::new(
                    ErrorKind::UpstreamError,
                    "upstream produced over 1 MiB before its first valid stream event",
                ));
            }
            let frames = parser.feed_bytes(&chunk)?;
            prefix.push(chunk);
            for frame in frames {
                if validate_stream_events(&decoder.decode(&frame)?)? {
                    return Ok((prefix, stream));
                }
            }
        }

        if let Some(frame) = parser.finish_bytes()?
            && validate_stream_events(&decoder.decode(&frame)?)?
        {
            return Ok((prefix, stream));
        }
        Err(GatewayError::new(
            ErrorKind::UpstreamError,
            "upstream stream ended before its first valid event",
        ))
    })
    .await
    .map_err(|_| {
        GatewayError::new(
            ErrorKind::Timeout,
            "upstream did not produce a valid first stream event before the idle deadline",
        )
    })?
}

fn validate_stream_events(events: &[StreamEvent]) -> Result<bool, GatewayError> {
    for event in events {
        if let StreamEvent::Error { message, kind } = event {
            let lowered = kind.to_ascii_lowercase();
            let error_kind = if lowered.contains("rate") || lowered.contains("quota") {
                ErrorKind::RateLimited
            } else if lowered.contains("auth") {
                ErrorKind::Unauthenticated
            } else if lowered.contains("permission") || lowered.contains("forbidden") {
                ErrorKind::PermissionDenied
            } else if lowered.contains("invalid") {
                // 上游明说请求非法 —— 换渠道也一样错，与非流式的 400 行为
                // 对齐：立即报给客户端，不再消耗剩余候选。
                ErrorKind::InvalidRequest
            } else {
                ErrorKind::UpstreamError
            };
            return Err(GatewayError::new(error_kind, message.clone()));
        }
    }
    Ok(events
        .iter()
        .any(|event| !matches!(event, StreamEvent::Ping)))
}

fn prepend_stream<T>(
    prefix: Vec<T>,
    rest: std::pin::Pin<Box<dyn Stream<Item = Result<T, GatewayError>> + Send>>,
) -> std::pin::Pin<Box<dyn Stream<Item = Result<T, GatewayError>> + Send>>
where
    T: Send + 'static,
{
    Box::pin(futures_util::stream::iter(prefix.into_iter().map(Ok)).chain(rest))
}

/// 给上游错误补齐最终命中的候选信息，供错误响应与失败日志使用。
fn annotate_candidate_error(
    mut error: GatewayError,
    candidate: &Candidate<'_>,
    attempts: u8,
) -> GatewayError {
    if error.channel_id.is_none() {
        error = error.with_channel(candidate.channel_id());
    }
    if error.protocol.is_none() {
        error = error.with_protocol(candidate.protocol());
    }
    if error.channel_name.is_none() {
        error.channel_name = Some(candidate.channel.name.clone());
    }
    if error.upstream_model.is_none() {
        error.upstream_model = Some(candidate.upstream_model().to_owned());
    }
    error.attempts = attempts;
    error
}

/// 只有能反映端点健康度的错误才进入熔断统计。
fn affects_endpoint_health(kind: ErrorKind) -> bool {
    matches!(
        kind,
        ErrorKind::Unauthenticated
            | ErrorKind::PermissionDenied
            | ErrorKind::RateLimited
            | ErrorKind::UpstreamError
            | ErrorKind::Timeout
            | ErrorKind::Configuration
            // 上游 404 说明渠道的地址或模型名配错了 —— 客户端拼错模型在
            // 规划阶段就被拦下（Diagnosis::UnknownModel），到得了这里的
            // 404 都是渠道自身的问题，持续失败就该熔断它。
            | ErrorKind::NotFound
    )
}

/// 首帧已验证后的流包装：完整结束记成功，中途错误记失败。
fn track_stream<T>(
    first: T,
    rest: std::pin::Pin<Box<dyn Stream<Item = Result<T, GatewayError>> + Send>>,
    health: HealthRepo,
    channel_id: ChannelId,
    protocol: Protocol,
    latency_ms: u64,
) -> std::pin::Pin<Box<dyn Stream<Item = Result<T, GatewayError>> + Send>>
where
    T: Send + 'static,
{
    let stream = futures_util::stream::once(async move { Ok(first) }).chain(rest);
    let tracked = futures_util::stream::unfold(
        (Box::pin(stream), health, false),
        move |(mut stream, health, done)| async move {
            if done {
                return None;
            }

            match stream.next().await {
                Some(Ok(item)) => Some((Ok(item), (stream, health, false))),
                Some(Err(error)) => {
                    if affects_endpoint_health(error.kind)
                        && let Err(store_error) = health
                            .record_failure(
                                channel_id,
                                protocol,
                                &error.to_string(),
                                error.retry_after,
                            )
                            .await
                    {
                        tracing::warn!(error = %store_error, "failed to record upstream stream failure");
                    }
                    Some((Err(error), (stream, health, true)))
                }
                None => {
                    if let Err(store_error) = health
                        .record_success(channel_id, protocol, latency_ms)
                        .await
                    {
                        tracing::warn!(error = %store_error, "failed to record upstream stream success");
                    }
                    None
                }
            }
        },
    );
    Box::pin(tracked)
}

/// 无候选时的错误。
///
/// 区分「模型不存在」与「协议不被允许」由上层的 `diagnose` 负责；这里只在
/// 计划为空时兜底，消息仍要能指向原因。
fn no_candidates(route: &Route<'_>) -> GatewayError {
    GatewayError::new(
        ErrorKind::NoAvailableChannel,
        format!(
            "no channel can serve model `{}` over the {} protocol",
            route.model,
            route.inbound.as_str()
        ),
    )
    .with_protocol(route.inbound)
}

/// 把请求体里的模型名改写成上游名。
///
/// Gemini 的模型名在 URL 而非 body 里，所以它不需要改写 —— 把这个差异藏在
/// 一个函数里，调用方就不必知道。
fn rewrite_model(body: &mut Value, protocol: Protocol, upstream_model: &str) {
    if protocol == Protocol::Gemini {
        return;
    }
    if let Value::Object(map) = body {
        map.insert("model".into(), Value::String(upstream_model.to_owned()));
    }
}
