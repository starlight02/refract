//! 进程内 Prometheus 指标。
//!
//! 计数器住在内存而非数据库：`/metrics` 是运维热路径，抓取间隔通常只有
//! 十几秒，打数据库纯属浪费。重启清零是 Prometheus counter 的正常语义
//! （`rate()`/`increase()` 会自动处理 counter reset），不需要持久化。
//!
//! 与仪表盘的分工：仪表盘读数据库、回答「过去 24 小时发生了什么」；
//! `/metrics` 回答「进程此刻的累计状态」，供外部监控系统消费。

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};

use refract_core::Protocol;
use refract_store::NewRequestLog;

/// 网关运行时计数器。全部无锁，热路径开销是几次 relaxed 原子加。
#[derive(Debug)]
pub struct GatewayMetrics {
    started_at: std::time::Instant,
    /// 按入口协议的请求数，下标与 [`Protocol::ALL`] 对齐。
    requests: [AtomicU64; 4],
    /// 按入口协议的失败数（HTTP >= 400）。
    failures: [AtomicU64; 4],
    /// 发生协议转换的请求数。
    transcoded: AtomicU64,
    /// 流式请求数。
    streams: AtomicU64,
    /// 输入/输出 token 累计。
    input_tokens: AtomicU64,
    output_tokens: AtomicU64,
    /// 上游重试累计。
    retries: AtomicU64,
}

impl Default for GatewayMetrics {
    fn default() -> Self {
        Self {
            started_at: std::time::Instant::now(),
            requests: Default::default(),
            failures: Default::default(),
            transcoded: AtomicU64::new(0),
            streams: AtomicU64::new(0),
            input_tokens: AtomicU64::new(0),
            output_tokens: AtomicU64::new(0),
            retries: AtomicU64::new(0),
        }
    }
}

fn slot(protocol: Protocol) -> usize {
    Protocol::ALL
        .iter()
        .position(|p| *p == protocol)
        .expect("Protocol::ALL covers every variant")
}

impl GatewayMetrics {
    /// 从一条请求日志采集指标。与日志同源，两边永远一致。
    pub fn observe(&self, entry: &NewRequestLog) {
        let idx = slot(entry.inbound_protocol);
        self.requests[idx].fetch_add(1, Ordering::Relaxed);
        if entry.status >= 400 {
            self.failures[idx].fetch_add(1, Ordering::Relaxed);
        }
        if entry.transcoded() {
            self.transcoded.fetch_add(1, Ordering::Relaxed);
        }
        if entry.stream {
            self.streams.fetch_add(1, Ordering::Relaxed);
        }
        self.input_tokens
            .fetch_add(entry.input_tokens, Ordering::Relaxed);
        self.output_tokens
            .fetch_add(entry.output_tokens, Ordering::Relaxed);
        self.retries
            .fetch_add(u64::from(entry.retries), Ordering::Relaxed);
    }

    /// 渲染成 Prometheus 文本格式（version 0.0.4）。
    pub fn render(&self) -> String {
        let mut out = String::with_capacity(1024);

        let _ = writeln!(
            out,
            "# HELP refract_requests_total Gateway requests by inbound protocol.\n\
             # TYPE refract_requests_total counter"
        );
        for protocol in Protocol::ALL {
            let _ = writeln!(
                out,
                "refract_requests_total{{protocol=\"{protocol}\"}} {}",
                self.requests[slot(protocol)].load(Ordering::Relaxed)
            );
        }

        let _ = writeln!(
            out,
            "# HELP refract_request_failures_total Failed gateway requests (HTTP >= 400) by inbound protocol.\n\
             # TYPE refract_request_failures_total counter"
        );
        for protocol in Protocol::ALL {
            let _ = writeln!(
                out,
                "refract_request_failures_total{{protocol=\"{protocol}\"}} {}",
                self.failures[slot(protocol)].load(Ordering::Relaxed)
            );
        }

        for (name, help, value) in [
            (
                "refract_transcoded_requests_total",
                "Requests that crossed protocols.",
                self.transcoded.load(Ordering::Relaxed),
            ),
            (
                "refract_stream_requests_total",
                "Streaming requests.",
                self.streams.load(Ordering::Relaxed),
            ),
            (
                "refract_input_tokens_total",
                "Input tokens across all requests.",
                self.input_tokens.load(Ordering::Relaxed),
            ),
            (
                "refract_output_tokens_total",
                "Output tokens across all requests.",
                self.output_tokens.load(Ordering::Relaxed),
            ),
            (
                "refract_upstream_retries_total",
                "Upstream retries performed by the router.",
                self.retries.load(Ordering::Relaxed),
            ),
        ] {
            let _ = writeln!(
                out,
                "# HELP {name} {help}\n# TYPE {name} counter\n{name} {value}"
            );
        }

        let _ = writeln!(
            out,
            "# HELP refract_uptime_seconds Seconds since the process started.\n\
             # TYPE refract_uptime_seconds gauge\n\
             refract_uptime_seconds {}",
            self.started_at.elapsed().as_secs()
        );
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(protocol: Protocol, status: u16) -> NewRequestLog {
        NewRequestLog {
            owner_id: 1,
            request_id: "r".into(),
            api_key_id: None,
            channel_id: None,
            channel_name: None,
            inbound_protocol: protocol,
            upstream_protocol: Protocol::Chat,
            model: "m".into(),
            upstream_model: "m".into(),
            stream: false,
            status,
            ttfb_ms: None,
            duration_ms: 1,
            input_tokens: 10,
            output_tokens: 5,
            cached_tokens: 0,
            reasoning_tokens: 0,
            retries: 2,
            error_kind: None,
            error_message: None,
        }
    }

    #[test]
    fn observe_accumulates_and_render_reports() {
        let metrics = GatewayMetrics::default();
        metrics.observe(&entry(Protocol::Chat, 200));
        metrics.observe(&entry(Protocol::Messages, 502));

        let text = metrics.render();
        assert!(text.contains(r#"refract_requests_total{protocol="chat"} 1"#));
        assert!(text.contains(r#"refract_requests_total{protocol="messages"} 1"#));
        assert!(text.contains(r#"refract_request_failures_total{protocol="messages"} 1"#));
        assert!(text.contains(r#"refract_request_failures_total{protocol="chat"} 0"#));
        // messages 入口打到 chat 上游 = 一次转码。
        assert!(text.contains("refract_transcoded_requests_total 1"));
        assert!(text.contains("refract_input_tokens_total 20"));
        assert!(text.contains("refract_output_tokens_total 10"));
        assert!(text.contains("refract_upstream_retries_total 4"));
        assert!(text.contains("refract_uptime_seconds"));
    }
}
