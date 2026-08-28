//! 进程内 Prometheus 指标。
//!
//! 计数器住在内存而非数据库：`/metrics` 是运维热路径，抓取间隔通常只有
//! 十几秒，打数据库纯属浪费。重启清零是 Prometheus counter 的正常语义
//! （`rate()`/`increase()` 会自动处理 counter reset），不需要持久化。
//!
//! 与仪表盘的分工：仪表盘读数据库、回答「过去 24 小时发生了什么」；
//! `/metrics` 回答「进程此刻的累计状态」，供外部监控系统消费。

use std::fmt::Write as _;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use refract_core::Protocol;
use refract_store::NewRequestLog;

/// 直方图桶边界（秒）。覆盖「亚秒非流式」到「分钟级长回答」。
const DURATION_BUCKETS: [f64; 10] = [0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0];
/// TTFB 桶边界（秒）。首字延迟集中在低区间，桶更密。
const TTFB_BUCKETS: [f64; 9] = [0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0];

/// 固定桶直方图。`buckets[i]` 是 `<= bounds[i]` 的累计计数（Prometheus 语义）。
#[derive(Debug)]
struct Histogram<const N: usize> {
    buckets: [AtomicU64; N],
    count: AtomicU64,
    /// 总和的微秒表示 —— f64 没有原子类型，用整数微秒累计。
    sum_micros: AtomicU64,
}

impl<const N: usize> Histogram<N> {
    fn new() -> Self {
        Self {
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            count: AtomicU64::new(0),
            sum_micros: AtomicU64::new(0),
        }
    }

    fn observe(&self, bounds: &[f64; N], seconds: f64) {
        for (i, bound) in bounds.iter().enumerate() {
            if seconds <= *bound {
                self.buckets[i].fetch_add(1, Ordering::Relaxed);
            }
        }
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum_micros
            .fetch_add((seconds * 1_000_000.0) as u64, Ordering::Relaxed);
    }

    fn render(&self, out: &mut String, name: &str, help: &str, bounds: &[f64; N]) {
        let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} histogram");
        for (i, bound) in bounds.iter().enumerate() {
            let _ = writeln!(
                out,
                "{name}_bucket{{le=\"{bound}\"}} {}",
                self.buckets[i].load(Ordering::Relaxed)
            );
        }
        let count = self.count.load(Ordering::Relaxed);
        let _ = writeln!(out, "{name}_bucket{{le=\"+Inf\"}} {count}");
        let _ = writeln!(
            out,
            "{name}_sum {}",
            self.sum_micros.load(Ordering::Relaxed) as f64 / 1_000_000.0
        );
        let _ = writeln!(out, "{name}_count {count}");
    }
}

/// 网关运行时计数器。热路径全部是 relaxed 原子操作；
/// 按渠道的计数走一把低争用的 Mutex（个人场景渠道数是个位数）。
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
    /// 钱包扣款失败累计（记账失败不回滚请求，只计数告警）。
    wallet_charge_failures: AtomicU64,
    /// 请求总耗时直方图。
    duration: Histogram<10>,
    /// 首字延迟直方图（仅有 TTFB 的请求）。
    ttfb: Histogram<9>,
    /// 按渠道名的 (请求, 失败) 计数。个人场景基数极低，无爆炸风险。
    by_channel: Mutex<std::collections::BTreeMap<String, (u64, u64)>>,
    /// 按用户 ID 的请求数。仅 `metrics.per_user` 开启时采集（高基数预警）。
    per_user_requests: Mutex<std::collections::BTreeMap<i64, u64>>,
    /// 是否采集 per-user 指标。
    per_user_enabled: std::sync::atomic::AtomicBool,
    /// 按用户 ID 的钱包余额。仅在 Prometheus 抓取时刷新，避免请求热路径写锁。
    wallet_balances: Mutex<std::collections::BTreeMap<i64, f64>>,
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
            wallet_charge_failures: AtomicU64::new(0),
            duration: Histogram::new(),
            ttfb: Histogram::new(),
            by_channel: Mutex::new(std::collections::BTreeMap::new()),
            per_user_requests: Mutex::new(std::collections::BTreeMap::new()),
            per_user_enabled: std::sync::atomic::AtomicBool::new(false),
            wallet_balances: Mutex::new(std::collections::BTreeMap::new()),
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
    /// 钱包扣款失败计数（记账失败不影响已完成的请求，但必须可观测）。
    pub fn note_wallet_charge_failure(&self) {
        self.wallet_charge_failures
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// 开关 per-user 指标采集（settings `metrics.per_user`）。
    pub fn set_per_user_enabled(&self, enabled: bool) {
        self.per_user_enabled
            .store(enabled, std::sync::atomic::Ordering::Relaxed);
    }

    /// 是否已启用 per-user 指标。
    pub fn per_user_enabled(&self) -> bool {
        self.per_user_enabled.load(Ordering::Relaxed)
    }

    /// 使用一次抓取时读取的钱包余额替换 gauge 快照。
    pub fn set_wallet_balances(&self, balances: impl IntoIterator<Item = (i64, f64)>) {
        let mut snapshot = self.wallet_balances.lock().expect("wallet metrics lock");
        snapshot.clear();
        snapshot.extend(balances);
    }

    /// 从一条请求日志采集指标。与日志同源，两边永远一致。
    pub fn observe(&self, entry: &NewRequestLog) {
        let idx = slot(entry.inbound_protocol);
        self.requests[idx].fetch_add(1, Ordering::Relaxed);
        if self
            .per_user_enabled
            .load(std::sync::atomic::Ordering::Relaxed)
            && let Some(uid) = entry.user_id
        {
            let mut map = self
                .per_user_requests
                .lock()
                .expect("per-user metrics lock");
            *map.entry(uid).or_insert(0) += 1;
        }
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
        self.duration
            .observe(&DURATION_BUCKETS, entry.duration_ms as f64 / 1_000.0);
        if let Some(ttfb) = entry.ttfb_ms {
            self.ttfb.observe(&TTFB_BUCKETS, ttfb as f64 / 1_000.0);
        }
        if let Some(channel) = entry.channel_name.as_deref() {
            let mut map = self.by_channel.lock().expect("channel metrics lock");
            let slot = map.entry(channel.to_owned()).or_insert((0, 0));
            slot.0 += 1;
            if entry.status >= 400 {
                slot.1 += 1;
            }
        }
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
            (
                "refract_wallet_charge_failures_total",
                "Wallet charge failures (charging never rolls back a completed request).",
                self.wallet_charge_failures.load(Ordering::Relaxed),
            ),
        ] {
            let _ = writeln!(
                out,
                "# HELP {name} {help}\n# TYPE {name} counter\n{name} {value}"
            );
        }

        self.duration.render(
            &mut out,
            "refract_request_duration_seconds",
            "End-to-end request duration.",
            &DURATION_BUCKETS,
        );
        self.ttfb.render(
            &mut out,
            "refract_ttfb_seconds",
            "Time to first byte from the upstream.",
            &TTFB_BUCKETS,
        );

        {
            let map = self.by_channel.lock().expect("channel metrics lock");
            let _ = writeln!(
                out,
                "# HELP refract_channel_requests_total Requests routed to each channel.\n\
                 # TYPE refract_channel_requests_total counter"
            );
            for (channel, (requests, _)) in map.iter() {
                let _ = writeln!(
                    out,
                    "refract_channel_requests_total{{channel=\"{}\"}} {requests}",
                    channel.replace('\\', "\\\\").replace('"', "\\\"")
                );
            }
            let _ = writeln!(
                out,
                "# HELP refract_channel_failures_total Failed requests (HTTP >= 400) per channel.\n\
                 # TYPE refract_channel_failures_total counter"
            );
            for (channel, (_, failures)) in map.iter() {
                let _ = writeln!(
                    out,
                    "refract_channel_failures_total{{channel=\"{}\"}} {failures}",
                    channel.replace('\\', "\\\\").replace('"', "\\\"")
                );
            }
        }

        if self
            .per_user_enabled
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            let map = self
                .per_user_requests
                .lock()
                .expect("per-user metrics lock");
            let _ = writeln!(
                out,
                "# HELP refract_user_requests_total Gateway requests per user (opt-in high cardinality).\n\
                 # TYPE refract_user_requests_total counter"
            );
            for (user_id, requests) in map.iter() {
                let _ = writeln!(
                    out,
                    "refract_user_requests_total{{user_id=\"{user_id}\"}} {requests}"
                );
            }
            let balances = self.wallet_balances.lock().expect("wallet metrics lock");
            let _ = writeln!(
                out,
                "# HELP refract_wallet_balance Current prepaid wallet balance per user (opt-in high cardinality).\n\
                 # TYPE refract_wallet_balance gauge"
            );
            for (user_id, balance) in balances.iter() {
                let _ = writeln!(
                    out,
                    "refract_wallet_balance{{user_id=\"{user_id}\"}} {balance}"
                );
            }
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
            user_id: None,
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
            cache_write_tokens: 0,
            reasoning_tokens: 0,
            retries: 2,
            cost: 0.0,
            error_kind: None,
            error_message: None,
            request_body: None,
            response_body: None,
            credential_hint: None,
            affinity_rule: None,
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

    #[test]
    fn histograms_and_channel_labels_render() {
        let metrics = GatewayMetrics::default();
        let mut first = entry(Protocol::Chat, 200);
        first.duration_ms = 300;
        first.ttfb_ms = Some(80);
        first.channel_name = Some("主力站".into());
        metrics.observe(&first);
        let mut second = entry(Protocol::Chat, 500);
        second.duration_ms = 4_000;
        second.channel_name = Some("主力站".into());
        metrics.observe(&second);

        let text = metrics.render();
        // 300ms 落进 le=0.5 桶；4s 不落进。
        assert!(text.contains(r#"refract_request_duration_seconds_bucket{le="0.5"} 1"#));
        assert!(text.contains("refract_request_duration_seconds_count 2"));
        // TTFB 只统计有值的记录。
        assert!(text.contains("refract_ttfb_seconds_count 1"));
        assert!(text.contains(r#"refract_ttfb_seconds_bucket{le="0.1"} 1"#));
        // 渠道标签计数。
        assert!(text.contains(r#"refract_channel_requests_total{channel="主力站"} 2"#));
        assert!(text.contains(r#"refract_channel_failures_total{channel="主力站"} 1"#));
    }

    #[test]
    fn per_user_metrics_are_opt_in_and_include_wallet_gauges() {
        let metrics = GatewayMetrics::default();
        let mut log = entry(Protocol::Chat, 200);
        log.user_id = Some(42);
        metrics.observe(&log);
        metrics.set_wallet_balances([(42, 1.25)]);
        assert!(!metrics.render().contains("refract_user_requests_total"));
        assert!(!metrics.render().contains("refract_wallet_balance"));

        metrics.set_per_user_enabled(true);
        metrics.observe(&log);
        let text = metrics.render();
        assert!(text.contains(r#"refract_user_requests_total{user_id="42"} 1"#));
        assert!(text.contains(r#"refract_wallet_balance{user_id="42"} 1.25"#));
    }
}
