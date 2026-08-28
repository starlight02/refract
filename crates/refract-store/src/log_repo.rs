//! 请求日志与统计仓储。

use chrono::{DateTime, Utc};
use refract_core::{ChannelId, Protocol};
use sqlx::Row;

use crate::db::{Database, StoreError};
use crate::key_repo::parse_ts;

/// 一条请求日志。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct RequestLog {
    /// 主键。
    pub id: i64,
    /// 请求 ID，与响应头里的 `x-request-id` 一致。
    pub request_id: String,
    /// 时间。
    pub created_at: DateTime<Utc>,
    /// 使用的网关密钥。
    pub api_key_id: Option<i64>,
    /// 发起请求的用户。历史日志可能为空。
    #[serde(default)]
    pub user_id: Option<i64>,
    /// 命中的渠道。
    pub channel_id: Option<ChannelId>,
    /// 渠道名快照（渠道被删后日志仍可读）。
    pub channel_name: Option<String>,
    /// 客户端使用的协议。
    pub inbound_protocol: String,
    /// 上游端点的协议。
    pub upstream_protocol: String,
    /// 是否发生了协议转换。
    pub transcoded: bool,
    /// 对外模型名。
    pub model: String,
    /// 上游模型名。
    pub upstream_model: String,
    /// 是否流式。
    pub stream: bool,
    /// 响应状态码。
    pub status: i64,
    /// 首字节延迟（毫秒）。
    pub ttfb_ms: Option<i64>,
    /// 总耗时（毫秒）。
    pub duration_ms: i64,
    /// 输入 token。
    pub input_tokens: i64,
    /// 输出 token。
    pub output_tokens: i64,
    /// 缓存命中 token。
    pub cached_tokens: i64,
    /// 缓存写入 token（Anthropic cache_creation）。
    pub cache_write_tokens: i64,
    /// 推理 token。
    pub reasoning_tokens: i64,
    /// 重试次数。
    pub retries: i64,
    /// 按落库时价表计算的成本。
    pub cost: f64,
    /// 错误种类。
    pub error_kind: Option<String>,
    /// 错误消息。
    pub error_message: Option<String>,
    /// 实际使用的上游钥匙的脱敏提示（多密钥池排障用）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential_hint: Option<String>,
    /// 命中并已使用的亲和规则名。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub affinity_rule: Option<String>,
    /// 请求正文快照。列表查询不取（`None`），单条详情才有。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_body: Option<String>,
    /// 响应正文快照。同上；流式响应存聚合后的文本。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_body: Option<String>,
}

/// 新增日志的参数。
#[derive(Debug, Clone)]
pub struct NewRequestLog {
    /// 所有者。
    pub owner_id: i64,
    /// 请求 ID。
    pub request_id: String,
    /// 网关密钥 ID。
    pub api_key_id: Option<i64>,
    /// 发起请求的用户。
    pub user_id: Option<i64>,
    /// 渠道 ID。
    pub channel_id: Option<ChannelId>,
    /// 渠道名。
    pub channel_name: Option<String>,
    /// 入口协议。
    pub inbound_protocol: Protocol,
    /// 上游协议。
    pub upstream_protocol: Protocol,
    /// 对外模型名。
    pub model: String,
    /// 上游模型名。
    pub upstream_model: String,
    /// 是否流式。
    pub stream: bool,
    /// 状态码。
    pub status: u16,
    /// 首字节延迟。
    pub ttfb_ms: Option<u64>,
    /// 总耗时。
    pub duration_ms: u64,
    /// 输入 token。
    pub input_tokens: u64,
    /// 输出 token。
    pub output_tokens: u64,
    /// 缓存命中 token。
    pub cached_tokens: u64,
    /// 缓存写入 token（Anthropic cache_creation）。
    pub cache_write_tokens: u64,
    /// 推理 token。
    pub reasoning_tokens: u64,
    /// 重试次数。
    pub retries: u32,
    /// 本次请求的成本（由网关按当前价表计算）。
    pub cost: f64,
    /// 错误种类。
    pub error_kind: Option<String>,
    /// 错误消息。
    pub error_message: Option<String>,
    /// 实际使用的上游钥匙的脱敏提示。
    pub credential_hint: Option<String>,
    /// 命中并已使用的亲和规则名。
    pub affinity_rule: Option<String>,
    /// 请求正文快照（已按上限截断）。
    pub request_body: Option<String>,
    /// 响应正文快照（已按上限截断）。
    pub response_body: Option<String>,
}

impl NewRequestLog {
    /// 构造一份基础请求日志，默认 token 用量为 0，状态码 200，无错误。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        owner_id: i64,
        request_id: String,
        api_key_id: Option<i64>,
        inbound_protocol: Protocol,
        model: String,
        stream: bool,
    ) -> Self {
        Self {
            owner_id,
            request_id,
            api_key_id,
            user_id: None,
            channel_id: None,
            channel_name: None,
            inbound_protocol,
            upstream_protocol: inbound_protocol,
            model,
            upstream_model: String::new(),
            stream,
            status: 200,
            ttfb_ms: None,
            duration_ms: 0,
            input_tokens: 0,
            output_tokens: 0,
            cached_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: 0,
            retries: 0,
            cost: 0.0,
            error_kind: None,
            error_message: None,
            request_body: None,
            response_body: None,
            credential_hint: None,
            affinity_rule: None,
        }
    }

    /// 注入路由命中的渠道与上游信息。
    pub fn with_channel(
        mut self,
        id: ChannelId,
        name: String,
        protocol: Protocol,
        model: String,
    ) -> Self {
        self.channel_id = Some(id);
        self.channel_name = Some(name);
        self.upstream_protocol = protocol;
        self.upstream_model = model;
        self
    }

    /// 注入 Token 用量。
    pub fn with_tokens(
        mut self,
        input: u64,
        output: u64,
        cached: u64,
        cache_write: u64,
        reasoning: u64,
    ) -> Self {
        self.input_tokens = input;
        self.output_tokens = output;
        self.cached_tokens = cached;
        self.cache_write_tokens = cache_write;
        self.reasoning_tokens = reasoning;
        self
    }

    /// 注入耗时与首字节延迟。
    pub fn with_timing(mut self, ttfb_ms: Option<u64>, duration_ms: u64) -> Self {
        self.ttfb_ms = ttfb_ms;
        self.duration_ms = duration_ms;
        self
    }

    /// 注入请求与响应正文快照。
    pub fn with_snapshots(mut self, req: Option<String>, resp: Option<String>) -> Self {
        self.request_body = req;
        self.response_body = resp;
        self
    }

    /// 注入错误信息。
    pub fn with_error(mut self, kind: String, msg: String) -> Self {
        self.error_kind = Some(kind);
        self.error_message = Some(msg);
        self
    }

    /// 注入排障上下文：实际使用的钥匙提示与亲和规则。
    pub fn with_routing_context(
        mut self,
        credential_hint: Option<String>,
        affinity_rule: Option<String>,
    ) -> Self {
        self.credential_hint = credential_hint;
        self.affinity_rule = affinity_rule;
        self
    }

    /// 是否发生了协议转换。由两个协议字段推导，不单独传参 —— 避免不一致。
    pub fn transcoded(&self) -> bool {
        self.inbound_protocol != self.upstream_protocol
    }
}

/// 日志筛选条件。
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct LogFilter {
    /// 按模型筛。
    #[serde(default)]
    pub model: Option<String>,
    /// 按渠道筛。
    #[serde(default)]
    pub channel_id: Option<ChannelId>,
    /// 按网关密钥筛。
    #[serde(default)]
    pub api_key_id: Option<i64>,
    /// 按用户筛。
    #[serde(default)]
    pub user_id: Option<i64>,
    /// 按请求 ID 精确检索 —— `x-refract-request-id` 响应头的排障动线。
    #[serde(default)]
    pub request_id: Option<String>,
    /// 起始时间（含），RFC3339 或 `YYYY-MM-DD HH:MM:SS`（UTC）。
    #[serde(default)]
    pub since: Option<String>,
    /// 截止时间（含）。
    #[serde(default)]
    pub until: Option<String>,
    /// 只看失败。
    #[serde(default)]
    pub failures_only: bool,
    /// 分页大小。
    #[serde(default)]
    pub limit: Option<u32>,
    /// 分页偏移。
    #[serde(default)]
    pub offset: Option<u32>,
}

/// 汇总统计。
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize)]
pub struct StatsSummary {
    /// 请求总数。
    pub requests: i64,
    /// 失败数。
    pub failures: i64,
    /// 输入 token 总量。
    pub input_tokens: i64,
    /// 输出 token 总量。
    pub output_tokens: i64,
    /// 平均耗时（毫秒）。
    pub avg_duration_ms: f64,
    /// 平均首字节延迟（毫秒），仅统计有值的记录。
    pub avg_ttfb_ms: Option<f64>,
    /// 发生了协议转换的请求数。
    pub transcoded: i64,
    /// 累计成本。
    pub cost: f64,
}

/// 按模型的统计。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ModelStat {
    /// 模型名。
    pub model: String,
    /// 请求数。
    pub requests: i64,
    /// 输入 token。
    pub input_tokens: i64,
    /// 输出 token。
    pub output_tokens: i64,
    /// 累计成本。
    pub cost: f64,
    /// 平均首字延迟（毫秒），仅统计有值的记录。
    pub avg_ttfb_ms: Option<f64>,
    /// 平均总耗时（毫秒）。
    pub avg_duration_ms: f64,
    /// 输出速率（token/秒）：输出量 ÷ 首字之后的生成时长。
    pub tokens_per_sec: Option<f64>,
}

/// 按渠道的用量统计。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ChannelStat {
    /// 渠道 ID。渠道被删后日志仍在，此时只剩名字快照。
    pub channel_id: Option<i64>,
    /// 渠道名快照。
    pub channel_name: String,
    /// 请求数。
    pub requests: i64,
    /// 失败数。
    pub failures: i64,
    /// 输入 token。
    pub input_tokens: i64,
    /// 输出 token。
    pub output_tokens: i64,
    /// 累计成本。
    pub cost: f64,
    /// 平均首字延迟（毫秒）。
    pub avg_ttfb_ms: Option<f64>,
    /// 平均总耗时（毫秒）。
    pub avg_duration_ms: f64,
}

/// 一个时间桶的聚合。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct TimeBucket {
    /// 桶标签（`YYYY-MM-DD HH:00` 或 `YYYY-MM-DD`，UTC）。
    pub bucket: String,
    /// 请求数。
    pub requests: i64,
    /// 失败数。
    pub failures: i64,
    /// 输入 token。
    pub input_tokens: i64,
    /// 输出 token。
    pub output_tokens: i64,
    /// 累计成本。
    pub cost: f64,
}

/// 按网关密钥的用量统计。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct KeyUsageStat {
    /// 密钥 ID。
    pub api_key_id: i64,
    /// 请求数。
    pub requests: i64,
    /// 失败数。
    pub failures: i64,
    /// 输入 token。
    pub input_tokens: i64,
    /// 输出 token。
    pub output_tokens: i64,
    /// 累计成本。
    pub cost: f64,
}

/// 日志仓储。
#[derive(Debug, Clone)]
pub struct LogRepo {
    db: Database,
}
macro_rules! log_summary_cols {
    () => {
        "id, request_id, created_at, api_key_id, user_id, channel_id, channel_name, \
         inbound_protocol, upstream_protocol, transcoded, model, upstream_model, stream, \
         status, ttfb_ms, duration_ms, input_tokens, output_tokens, cached_tokens, \
         cache_write_tokens, reasoning_tokens, retries, cost, error_kind, error_message, \
         credential_hint, affinity_rule"
    };
}

macro_rules! log_detail_cols {
    () => {
        "id, request_id, created_at, api_key_id, user_id, channel_id, channel_name, \
         inbound_protocol, upstream_protocol, transcoded, model, upstream_model, stream, \
         status, ttfb_ms, duration_ms, input_tokens, output_tokens, cached_tokens, \
         cache_write_tokens, reasoning_tokens, retries, cost, error_kind, error_message, \
         credential_hint, affinity_rule, request_body, response_body"
    };
}

const QUERY_LOGS_SQL: &str = concat!(
    "SELECT ",
    log_summary_cols!(),
    " FROM request_logs \
     WHERE owner_id = ? \
       AND (? IS NULL OR model = ?) \
       AND (? IS NULL OR channel_id = ?) \
       AND (? IS NULL OR api_key_id = ?) \
       AND (? IS NULL OR user_id = ?) \
       AND (? IS NULL OR request_id = ?) \
       AND (? IS NULL OR created_at >= ?) \
       AND (? IS NULL OR created_at <= ?) \
       AND (? = 0 OR status >= 400) \
     ORDER BY id DESC LIMIT ? OFFSET ?"
);

const GET_LOG_DETAIL_SQL: &str = concat!(
    "SELECT ",
    log_detail_cols!(),
    " FROM request_logs WHERE owner_id = ? AND id = ?"
);

impl LogRepo {
    /// 绑定到一个数据库。
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// 追加一条日志。
    pub async fn append(&self, entry: &NewRequestLog) -> Result<i64, StoreError> {
        let id: i64 = sqlx::query(
            "INSERT INTO request_logs \
             (owner_id, user_id, request_id, api_key_id, channel_id, channel_name, inbound_protocol, \
              upstream_protocol, transcoded, model, upstream_model, stream, status, ttfb_ms, \
              duration_ms, input_tokens, output_tokens, cached_tokens, cache_write_tokens, \
              reasoning_tokens, retries, cost, error_kind, error_message, credential_hint, \
              affinity_rule, request_body, response_body) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING id",
        )
        .bind(entry.owner_id)
        .bind(entry.user_id)
        .bind(&entry.request_id)
        .bind(entry.api_key_id)
        .bind(entry.channel_id)
        .bind(entry.channel_name.as_deref())
        .bind(entry.inbound_protocol.as_str())
        .bind(entry.upstream_protocol.as_str())
        .bind(entry.transcoded())
        .bind(&entry.model)
        .bind(&entry.upstream_model)
        .bind(entry.stream)
        .bind(i64::from(entry.status))
        .bind(entry.ttfb_ms.map(|v| v as i64))
        .bind(entry.duration_ms as i64)
        .bind(entry.input_tokens as i64)
        .bind(entry.output_tokens as i64)
        .bind(entry.cached_tokens as i64)
        .bind(entry.cache_write_tokens as i64)
        .bind(entry.reasoning_tokens as i64)
        .bind(i64::from(entry.retries))
        .bind(entry.cost)
        .bind(entry.error_kind.as_deref())
        .bind(entry.error_message.as_deref())
        .bind(entry.credential_hint.as_deref())
        .bind(entry.affinity_rule.as_deref())
        .bind(entry.request_body.as_deref())
        .bind(entry.response_body.as_deref())
        .fetch_one(self.db.pool())
        .await?
        .get(0);
        Ok(id)
    }

    /// 按条件分页查询。
    pub async fn query(
        &self,
        owner_id: i64,
        filter: &LogFilter,
    ) -> Result<Vec<RequestLog>, StoreError> {
        // 条件是有限且固定的组合，用固定占位符 + 「NULL 即不筛」的写法，
        // 避免动态拼 SQL 字符串。
        //
        // 注意：sqlx 0.9 的 SQLite 驱动不能把匿名 `?` 和编号 `?N` 混用（混用时
        // 参数会错位，报 `datatype mismatch`），所以每个可选条件把同一个值
        // 绑定两次，全程只用匿名占位符。
        let limit = filter.limit.unwrap_or(50).clamp(1, 500);
        let offset = filter.offset.unwrap_or(0);

        let rows = sqlx::query(QUERY_LOGS_SQL)
            .bind(owner_id)
            .bind(filter.model.as_deref())
            .bind(filter.model.as_deref())
            .bind(filter.channel_id)
            .bind(filter.channel_id)
            .bind(filter.api_key_id)
            .bind(filter.api_key_id)
            .bind(filter.user_id)
            .bind(filter.user_id)
            .bind(filter.request_id.as_deref())
            .bind(filter.request_id.as_deref())
            .bind(filter.since.as_deref())
            .bind(filter.since.as_deref())
            .bind(filter.until.as_deref())
            .bind(filter.until.as_deref())
            .bind(i64::from(filter.failures_only))
            .bind(i64::from(limit))
            .bind(i64::from(offset))
            .fetch_all(self.db.pool())
            .await?;

        rows.iter().map(Self::from_row).collect()
    }

    /// 取单条日志的完整记录（含请求/响应正文快照）。
    ///
    /// 正文可能到几十 KB，列表查询绝不带它们；这个方法只在用户点开
    /// 「查看完整请求」时按需调用。
    pub async fn get(&self, owner_id: i64, id: i64) -> Result<RequestLog, StoreError> {
        let row = sqlx::query(GET_LOG_DETAIL_SQL)
            .bind(owner_id)
            .bind(id)
            .fetch_optional(self.db.pool())
            .await?
            .ok_or_else(|| StoreError::not_found("request log", id))?;
        Self::from_row(&row)
    }

    /// 最近 N 小时的汇总统计。
    pub async fn summary(&self, owner_id: i64, hours: u32) -> Result<StatsSummary, StoreError> {
        let row = sqlx::query(
            "SELECT COUNT(*) AS requests, \
                    COALESCE(SUM(status >= 400), 0) AS failures, \
                    COALESCE(SUM(input_tokens), 0) AS input_tokens, \
                    COALESCE(SUM(output_tokens), 0) AS output_tokens, \
                    COALESCE(AVG(duration_ms), 0.0) AS avg_duration, \
                    AVG(ttfb_ms) AS avg_ttfb, \
                    COALESCE(SUM(transcoded), 0) AS transcoded, \
                    COALESCE(SUM(cost), 0.0) AS cost \
             FROM request_logs \
             WHERE owner_id = ? AND created_at >= datetime('now', ?)",
        )
        .bind(owner_id)
        .bind(format!("-{hours} hours"))
        .fetch_one(self.db.pool())
        .await?;

        Ok(StatsSummary {
            requests: row.get("requests"),
            failures: row.get("failures"),
            input_tokens: row.get("input_tokens"),
            output_tokens: row.get("output_tokens"),
            avg_duration_ms: row.get("avg_duration"),
            avg_ttfb_ms: row.get("avg_ttfb"),
            transcoded: row.get("transcoded"),
            cost: row.get("cost"),
        })
    }

    /// 最近 N 小时按模型的用量排行。
    pub async fn by_model(
        &self,
        owner_id: i64,
        hours: u32,
        limit: u32,
    ) -> Result<Vec<ModelStat>, StoreError> {
        let rows = sqlx::query(
            "SELECT model, COUNT(*) AS requests, \
                    COALESCE(SUM(input_tokens), 0) AS input_tokens, \
                    COALESCE(SUM(output_tokens), 0) AS output_tokens, \
                    COALESCE(SUM(cost), 0.0) AS cost, \
                    AVG(ttfb_ms) AS avg_ttfb, \
                    COALESCE(AVG(duration_ms), 0.0) AS avg_duration, \
                    CAST(SUM(output_tokens) AS REAL) * 1000.0 \
                      / NULLIF(SUM(MAX(duration_ms - COALESCE(ttfb_ms, 0), 0)), 0) AS tokens_per_sec \
             FROM request_logs \
             WHERE owner_id = ? AND created_at >= datetime('now', ?) \
             GROUP BY model ORDER BY requests DESC LIMIT ?",
        )
        .bind(owner_id)
        .bind(format!("-{hours} hours"))
        .bind(i64::from(limit.clamp(1, 100)))
        .fetch_all(self.db.pool())
        .await?;

        Ok(rows
            .iter()
            .map(|r| ModelStat {
                model: r.get("model"),
                requests: r.get("requests"),
                input_tokens: r.get("input_tokens"),
                output_tokens: r.get("output_tokens"),
                cost: r.get("cost"),
                avg_ttfb_ms: r.get("avg_ttfb"),
                avg_duration_ms: r.get("avg_duration"),
                tokens_per_sec: r.get("tokens_per_sec"),
            })
            .collect())
    }

    /// 最近 N 小时按渠道的用量聚合 —— 「哪个上游在燒錢、哪个成功率差」。
    pub async fn by_channel(
        &self,
        owner_id: i64,
        hours: u32,
    ) -> Result<Vec<ChannelStat>, StoreError> {
        let rows = sqlx::query(
            "SELECT channel_id, COALESCE(channel_name, '(未知渠道)') AS channel_name, \
                    COUNT(*) AS requests, \
                    COALESCE(SUM(status >= 400), 0) AS failures, \
                    COALESCE(SUM(input_tokens), 0) AS input_tokens, \
                    COALESCE(SUM(output_tokens), 0) AS output_tokens, \
                    COALESCE(SUM(cost), 0.0) AS cost, \
                    AVG(ttfb_ms) AS avg_ttfb, \
                    COALESCE(AVG(duration_ms), 0.0) AS avg_duration \
             FROM request_logs \
             WHERE owner_id = ? AND created_at >= datetime('now', ?) \
             GROUP BY channel_id, channel_name ORDER BY requests DESC",
        )
        .bind(owner_id)
        .bind(format!("-{hours} hours"))
        .fetch_all(self.db.pool())
        .await?;

        Ok(rows
            .iter()
            .map(|r| ChannelStat {
                channel_id: r.get("channel_id"),
                channel_name: r.get("channel_name"),
                requests: r.get("requests"),
                failures: r.get("failures"),
                input_tokens: r.get("input_tokens"),
                output_tokens: r.get("output_tokens"),
                cost: r.get("cost"),
                avg_ttfb_ms: r.get("avg_ttfb"),
                avg_duration_ms: r.get("avg_duration"),
            })
            .collect())
    }

    /// 最近 N 小时的时间序列聚合。`daily` 为真时按天分桶，否则按小时。
    ///
    /// 费用观测的核心是趋势不是快照 —— 「这周费用怎么涨的」只有时序
    /// 能回答。空桶不补零：前端按标签补齐比 SQL 生成日历表简单得多。
    pub async fn timeseries(
        &self,
        owner_id: i64,
        hours: u32,
        daily: bool,
    ) -> Result<Vec<TimeBucket>, StoreError> {
        let format = if daily { "%Y-%m-%d" } else { "%Y-%m-%d %H:00" };
        let rows = sqlx::query(
            "SELECT strftime(?, created_at) AS bucket, \
                    COUNT(*) AS requests, \
                    COALESCE(SUM(status >= 400), 0) AS failures, \
                    COALESCE(SUM(input_tokens), 0) AS input_tokens, \
                    COALESCE(SUM(output_tokens), 0) AS output_tokens, \
                    COALESCE(SUM(cost), 0.0) AS cost \
             FROM request_logs \
             WHERE owner_id = ? AND created_at >= datetime('now', ?) \
             GROUP BY bucket ORDER BY bucket",
        )
        .bind(format)
        .bind(owner_id)
        .bind(format!("-{hours} hours"))
        .fetch_all(self.db.pool())
        .await?;

        Ok(rows
            .iter()
            .map(|r| TimeBucket {
                bucket: r.get("bucket"),
                requests: r.get("requests"),
                failures: r.get("failures"),
                input_tokens: r.get("input_tokens"),
                output_tokens: r.get("output_tokens"),
                cost: r.get("cost"),
            })
            .collect())
    }

    /// 最近 N 小时按网关密钥的用量聚合。
    ///
    /// 只统计带密钥的请求：免鉴权模式下 `api_key_id` 为 NULL 的行没有归属，
    /// 混进来只会让每把密钥的数字失真。
    pub async fn by_key(&self, owner_id: i64, hours: u32) -> Result<Vec<KeyUsageStat>, StoreError> {
        let rows = sqlx::query(
            "SELECT api_key_id, COUNT(*) AS requests, \
                    COALESCE(SUM(status >= 400), 0) AS failures, \
                    COALESCE(SUM(input_tokens), 0) AS input_tokens, \
                    COALESCE(SUM(output_tokens), 0) AS output_tokens, \
                    COALESCE(SUM(cost), 0.0) AS cost \
             FROM request_logs \
             WHERE owner_id = ? AND api_key_id IS NOT NULL \
               AND created_at >= datetime('now', ?) \
             GROUP BY api_key_id ORDER BY requests DESC",
        )
        .bind(owner_id)
        .bind(format!("-{hours} hours"))
        .fetch_all(self.db.pool())
        .await?;

        Ok(rows
            .iter()
            .map(|r| KeyUsageStat {
                api_key_id: r.get("api_key_id"),
                requests: r.get("requests"),
                failures: r.get("failures"),
                input_tokens: r.get("input_tokens"),
                output_tokens: r.get("output_tokens"),
                cost: r.get("cost"),
            })
            .collect())
    }

    /// 最近 N 小时汇总，限定 `user_id`（`None` 只匹配 `user_id IS NULL` 的行）。
    pub async fn summary_for_user(
        &self,
        owner_id: i64,
        user_id: Option<i64>,
        hours: u32,
    ) -> Result<StatsSummary, StoreError> {
        let row = sqlx::query(
            "SELECT COUNT(*) AS requests, \
                    COALESCE(SUM(status >= 400), 0) AS failures, \
                    COALESCE(SUM(input_tokens), 0) AS input_tokens, \
                    COALESCE(SUM(output_tokens), 0) AS output_tokens, \
                    COALESCE(AVG(duration_ms), 0.0) AS avg_duration, \
                    AVG(ttfb_ms) AS avg_ttfb, \
                    COALESCE(SUM(transcoded), 0) AS transcoded, \
                    COALESCE(SUM(cost), 0.0) AS cost \
             FROM request_logs \
             WHERE owner_id = ? AND (user_id = ? OR (user_id IS NULL AND ? IS NULL)) \
               AND created_at >= datetime('now', ?)",
        )
        .bind(owner_id)
        .bind(user_id)
        .bind(user_id)
        .bind(format!("-{hours} hours"))
        .fetch_one(self.db.pool())
        .await?;

        Ok(StatsSummary {
            requests: row.get("requests"),
            failures: row.get("failures"),
            input_tokens: row.get("input_tokens"),
            output_tokens: row.get("output_tokens"),
            avg_duration_ms: row.get("avg_duration"),
            avg_ttfb_ms: row.get("avg_ttfb"),
            transcoded: row.get("transcoded"),
            cost: row.get("cost"),
        })
    }

    /// 最近 N 小时按模型排行，限定用户。
    pub async fn by_model_for_user(
        &self,
        owner_id: i64,
        user_id: Option<i64>,
        hours: u32,
        limit: u32,
    ) -> Result<Vec<ModelStat>, StoreError> {
        let rows = sqlx::query(
            "SELECT model, COUNT(*) AS requests, \
                    COALESCE(SUM(input_tokens), 0) AS input_tokens, \
                    COALESCE(SUM(output_tokens), 0) AS output_tokens, \
                    COALESCE(SUM(cost), 0.0) AS cost, \
                    AVG(ttfb_ms) AS avg_ttfb, \
                    COALESCE(AVG(duration_ms), 0.0) AS avg_duration, \
                    CAST(SUM(output_tokens) AS REAL) * 1000.0 \
                      / NULLIF(SUM(MAX(duration_ms - COALESCE(ttfb_ms, 0), 0)), 0) AS tokens_per_sec \
             FROM request_logs \
             WHERE owner_id = ? AND (user_id = ? OR (user_id IS NULL AND ? IS NULL)) \
               AND created_at >= datetime('now', ?) \
             GROUP BY model ORDER BY requests DESC LIMIT ?",
        )
        .bind(owner_id)
        .bind(user_id)
        .bind(user_id)
        .bind(format!("-{hours} hours"))
        .bind(i64::from(limit.clamp(1, 100)))
        .fetch_all(self.db.pool())
        .await?;

        Ok(rows
            .iter()
            .map(|r| ModelStat {
                model: r.get("model"),
                requests: r.get("requests"),
                input_tokens: r.get("input_tokens"),
                output_tokens: r.get("output_tokens"),
                cost: r.get("cost"),
                avg_ttfb_ms: r.get("avg_ttfb"),
                avg_duration_ms: r.get("avg_duration"),
                tokens_per_sec: r.get("tokens_per_sec"),
            })
            .collect())
    }

    /// 最近 N 小时按渠道聚合，限定用户。
    pub async fn by_channel_for_user(
        &self,
        owner_id: i64,
        user_id: Option<i64>,
        hours: u32,
    ) -> Result<Vec<ChannelStat>, StoreError> {
        let rows = sqlx::query(
            "SELECT channel_id, COALESCE(channel_name, '(未知渠道)') AS channel_name, \
                    COUNT(*) AS requests, \
                    COALESCE(SUM(status >= 400), 0) AS failures, \
                    COALESCE(SUM(input_tokens), 0) AS input_tokens, \
                    COALESCE(SUM(output_tokens), 0) AS output_tokens, \
                    COALESCE(SUM(cost), 0.0) AS cost, \
                    AVG(ttfb_ms) AS avg_ttfb, \
                    COALESCE(AVG(duration_ms), 0.0) AS avg_duration \
             FROM request_logs \
             WHERE owner_id = ? AND (user_id = ? OR (user_id IS NULL AND ? IS NULL)) \
               AND created_at >= datetime('now', ?) \
             GROUP BY channel_id, channel_name ORDER BY requests DESC",
        )
        .bind(owner_id)
        .bind(user_id)
        .bind(user_id)
        .bind(format!("-{hours} hours"))
        .fetch_all(self.db.pool())
        .await?;

        Ok(rows
            .iter()
            .map(|r| ChannelStat {
                channel_id: r.get("channel_id"),
                channel_name: r.get("channel_name"),
                requests: r.get("requests"),
                failures: r.get("failures"),
                input_tokens: r.get("input_tokens"),
                output_tokens: r.get("output_tokens"),
                cost: r.get("cost"),
                avg_ttfb_ms: r.get("avg_ttfb"),
                avg_duration_ms: r.get("avg_duration"),
            })
            .collect())
    }

    /// 最近 N 小时时间序列，限定用户。
    pub async fn timeseries_for_user(
        &self,
        owner_id: i64,
        user_id: Option<i64>,
        hours: u32,
        daily: bool,
    ) -> Result<Vec<TimeBucket>, StoreError> {
        let format = if daily { "%Y-%m-%d" } else { "%Y-%m-%d %H:00" };
        let rows = sqlx::query(
            "SELECT strftime(?, created_at) AS bucket, \
                    COUNT(*) AS requests, \
                    COALESCE(SUM(status >= 400), 0) AS failures, \
                    COALESCE(SUM(input_tokens), 0) AS input_tokens, \
                    COALESCE(SUM(output_tokens), 0) AS output_tokens, \
                    COALESCE(SUM(cost), 0.0) AS cost \
             FROM request_logs \
             WHERE owner_id = ? AND (user_id = ? OR (user_id IS NULL AND ? IS NULL)) \
               AND created_at >= datetime('now', ?) \
             GROUP BY bucket ORDER BY bucket",
        )
        .bind(format)
        .bind(owner_id)
        .bind(user_id)
        .bind(user_id)
        .bind(format!("-{hours} hours"))
        .fetch_all(self.db.pool())
        .await?;

        Ok(rows
            .iter()
            .map(|r| TimeBucket {
                bucket: r.get("bucket"),
                requests: r.get("requests"),
                failures: r.get("failures"),
                input_tokens: r.get("input_tokens"),
                output_tokens: r.get("output_tokens"),
                cost: r.get("cost"),
            })
            .collect())
    }

    /// 最近 N 小时按密钥聚合，限定用户。
    pub async fn by_key_for_user(
        &self,
        owner_id: i64,
        user_id: Option<i64>,
        hours: u32,
    ) -> Result<Vec<KeyUsageStat>, StoreError> {
        let rows = sqlx::query(
            "SELECT api_key_id, COUNT(*) AS requests, \
                    COALESCE(SUM(status >= 400), 0) AS failures, \
                    COALESCE(SUM(input_tokens), 0) AS input_tokens, \
                    COALESCE(SUM(output_tokens), 0) AS output_tokens, \
                    COALESCE(SUM(cost), 0.0) AS cost \
             FROM request_logs \
             WHERE owner_id = ? AND api_key_id IS NOT NULL \
               AND (user_id = ? OR (user_id IS NULL AND ? IS NULL)) \
               AND created_at >= datetime('now', ?) \
             GROUP BY api_key_id ORDER BY requests DESC",
        )
        .bind(owner_id)
        .bind(user_id)
        .bind(user_id)
        .bind(format!("-{hours} hours"))
        .fetch_all(self.db.pool())
        .await?;

        Ok(rows
            .iter()
            .map(|r| KeyUsageStat {
                api_key_id: r.get("api_key_id"),
                requests: r.get("requests"),
                failures: r.get("failures"),
                input_tokens: r.get("input_tokens"),
                output_tokens: r.get("output_tokens"),
                cost: r.get("cost"),
            })
            .collect())
    }

    /// 删除 N 天前的日志，返回删除条数。
    ///
    /// 分批删：一次性 DELETE 在大表上是分钟级长事务，WAL 模式下会阻塞
    /// checkpoint 并让 wal 文件持续膨胀。每批一个短事务，删日志的同时
    /// 请求日志的写入不受影响。
    pub async fn prune(&self, days: u32) -> Result<u64, StoreError> {
        const BATCH: u32 = 5_000;
        let cutoff = format!("-{days} days");
        let mut total = 0_u64;
        loop {
            let affected = sqlx::query(
                "DELETE FROM request_logs WHERE id IN (\
                 SELECT id FROM request_logs WHERE created_at < datetime('now', ?) LIMIT ?)",
            )
            .bind(&cutoff)
            .bind(i64::from(BATCH))
            .execute(self.db.pool())
            .await?
            .rows_affected();
            total += affected;
            if affected < u64::from(BATCH) {
                break;
            }
        }
        Ok(total)
    }

    fn from_row(row: &sqlx::sqlite::SqliteRow) -> Result<RequestLog, StoreError> {
        Ok(RequestLog {
            id: row.get("id"),
            request_id: row.get("request_id"),
            created_at: parse_ts(row.get::<Option<String>, _>("created_at"))
                .unwrap_or_else(Utc::now),
            api_key_id: row.get("api_key_id"),
            user_id: row.get("user_id"),
            channel_id: row.get("channel_id"),
            channel_name: row.get("channel_name"),
            inbound_protocol: row.get("inbound_protocol"),
            upstream_protocol: row.get("upstream_protocol"),
            transcoded: row.get("transcoded"),
            model: row.get("model"),
            upstream_model: row.get("upstream_model"),
            stream: row.get("stream"),
            status: row.get("status"),
            ttfb_ms: row.get("ttfb_ms"),
            duration_ms: row.get("duration_ms"),
            input_tokens: row.get("input_tokens"),
            output_tokens: row.get("output_tokens"),
            cached_tokens: row.get("cached_tokens"),
            cache_write_tokens: row.get("cache_write_tokens"),
            reasoning_tokens: row.get("reasoning_tokens"),
            retries: row.get("retries"),
            cost: row.get("cost"),
            error_kind: row.get("error_kind"),
            error_message: row.get("error_message"),
            credential_hint: row.get("credential_hint"),
            affinity_rule: row.get("affinity_rule"),
            // 列表查询的 SELECT 不含这两列；只有单条详情才取。
            request_body: row.try_get("request_body").unwrap_or(None),
            response_body: row.try_get("response_body").unwrap_or(None),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use refract_core::DEFAULT_OWNER_ID;

    async fn repo() -> LogRepo {
        LogRepo::new(Database::open_in_memory().await.unwrap())
    }

    fn entry(model: &str, status: u16) -> NewRequestLog {
        NewRequestLog {
            owner_id: DEFAULT_OWNER_ID,
            request_id: uuid::Uuid::new_v4().to_string(),
            api_key_id: Some(1),
            user_id: None,
            channel_id: Some(7),
            channel_name: Some("relay".into()),
            inbound_protocol: Protocol::Chat,
            upstream_protocol: Protocol::Chat,
            model: model.into(),
            upstream_model: model.into(),
            stream: false,
            status,
            ttfb_ms: Some(120),
            duration_ms: 800,
            input_tokens: 100,
            output_tokens: 50,
            cached_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: 0,
            retries: 0,
            cost: 0.0,
            error_kind: None,
            error_message: None,
            credential_hint: None,
            affinity_rule: None,
            request_body: None,
            response_body: None,
        }
    }

    #[tokio::test]
    async fn append_then_query_returns_the_entry() {
        let repo = repo().await;
        repo.append(&entry("gpt-4o", 200)).await.unwrap();

        let logs = repo
            .query(DEFAULT_OWNER_ID, &LogFilter::default())
            .await
            .unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].model, "gpt-4o");
        assert_eq!(logs[0].status, 200);
        assert_eq!(logs[0].ttfb_ms, Some(120));
        assert!(!logs[0].transcoded);
    }

    #[tokio::test]
    async fn transcoded_flag_is_derived_from_protocol_pair() {
        let repo = repo().await;
        let mut e = entry("claude-sonnet-4-6", 200);
        e.inbound_protocol = Protocol::Messages;
        e.upstream_protocol = Protocol::Chat;
        assert!(e.transcoded());
        repo.append(&e).await.unwrap();

        let logs = repo
            .query(DEFAULT_OWNER_ID, &LogFilter::default())
            .await
            .unwrap();
        assert!(logs[0].transcoded);
        assert_eq!(logs[0].inbound_protocol, "messages");
        assert_eq!(logs[0].upstream_protocol, "chat");
    }

    #[tokio::test]
    async fn query_orders_newest_first() {
        let repo = repo().await;
        repo.append(&entry("first", 200)).await.unwrap();
        repo.append(&entry("second", 200)).await.unwrap();

        let logs = repo
            .query(DEFAULT_OWNER_ID, &LogFilter::default())
            .await
            .unwrap();
        assert_eq!(logs[0].model, "second");
        assert_eq!(logs[1].model, "first");
    }

    #[tokio::test]
    async fn filter_by_model() {
        let repo = repo().await;
        repo.append(&entry("gpt-4o", 200)).await.unwrap();
        repo.append(&entry("claude", 200)).await.unwrap();

        let logs = repo
            .query(
                DEFAULT_OWNER_ID,
                &LogFilter {
                    model: Some("claude".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].model, "claude");
    }

    #[tokio::test]
    async fn filter_by_channel() {
        let repo = repo().await;
        repo.append(&entry("a", 200)).await.unwrap();
        let mut other = entry("b", 200);
        other.channel_id = Some(99);
        repo.append(&other).await.unwrap();

        let logs = repo
            .query(
                DEFAULT_OWNER_ID,
                &LogFilter {
                    channel_id: Some(99),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].model, "b");
    }

    #[tokio::test]
    async fn failures_only_filter_uses_status_threshold() {
        let repo = repo().await;
        repo.append(&entry("ok", 200)).await.unwrap();
        repo.append(&entry("bad", 429)).await.unwrap();
        repo.append(&entry("worse", 502)).await.unwrap();

        let logs = repo
            .query(
                DEFAULT_OWNER_ID,
                &LogFilter {
                    failures_only: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(logs.len(), 2);
        assert!(logs.iter().all(|l| l.status >= 400));
    }

    #[tokio::test]
    async fn pagination_limits_and_offsets() {
        let repo = repo().await;
        for i in 0..5 {
            repo.append(&entry(&format!("m{i}"), 200)).await.unwrap();
        }
        let page = repo
            .query(
                DEFAULT_OWNER_ID,
                &LogFilter {
                    limit: Some(2),
                    offset: Some(1),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(page.len(), 2);
        // 倒序 m4,m3,m2,m1,m0 → offset 1 limit 2 得到 m3,m2。
        assert_eq!(page[0].model, "m3");
        assert_eq!(page[1].model, "m2");
    }

    #[tokio::test]
    async fn limit_is_clamped_to_a_sane_ceiling() {
        let repo = repo().await;
        repo.append(&entry("m", 200)).await.unwrap();
        // 超大 limit 不应炸掉，会被夹到 500。
        let logs = repo
            .query(
                DEFAULT_OWNER_ID,
                &LogFilter {
                    limit: Some(100_000),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(logs.len(), 1);
    }

    #[tokio::test]
    async fn summary_aggregates_counts_and_averages() {
        let repo = repo().await;
        repo.append(&entry("a", 200)).await.unwrap();
        let mut failed = entry("b", 500);
        failed.duration_ms = 1_200;
        failed.ttfb_ms = None;
        failed.error_kind = Some("UpstreamError".into());
        repo.append(&failed).await.unwrap();

        let s = repo.summary(DEFAULT_OWNER_ID, 24).await.unwrap();
        assert_eq!(s.requests, 2);
        assert_eq!(s.failures, 1);
        assert_eq!(s.input_tokens, 200);
        assert_eq!(s.output_tokens, 100);
        assert_eq!(s.avg_duration_ms, 1_000.0);
        // avg_ttfb 只统计有值的那条。
        assert_eq!(s.avg_ttfb_ms, Some(120.0));
    }

    #[tokio::test]
    async fn summary_on_empty_table_is_all_zeros() {
        let repo = repo().await;
        let s = repo.summary(DEFAULT_OWNER_ID, 24).await.unwrap();
        assert_eq!(s.requests, 0);
        assert_eq!(s.avg_duration_ms, 0.0);
        assert_eq!(s.avg_ttfb_ms, None);
    }

    #[tokio::test]
    async fn by_model_ranks_by_request_count() {
        let repo = repo().await;
        for _ in 0..3 {
            repo.append(&entry("popular", 200)).await.unwrap();
        }
        repo.append(&entry("rare", 200)).await.unwrap();

        let stats = repo.by_model(DEFAULT_OWNER_ID, 24, 10).await.unwrap();
        assert_eq!(stats[0].model, "popular");
        assert_eq!(stats[0].requests, 3);
        assert_eq!(stats[0].input_tokens, 300);
        assert_eq!(stats[1].model, "rare");
    }

    #[tokio::test]
    async fn prune_keeps_recent_entries() {
        let repo = repo().await;
        repo.append(&entry("recent", 200)).await.unwrap();
        // 手动插一条 100 天前的。
        sqlx::query(
            "INSERT INTO request_logs (owner_id, request_id, created_at, inbound_protocol, \
             upstream_protocol, model, upstream_model, status, duration_ms) \
             VALUES (1, 'old', datetime('now', '-100 days'), 'chat', 'chat', 'old', 'old', 200, 1)",
        )
        .execute(repo.db.pool())
        .await
        .unwrap();

        let removed = repo.prune(30).await.unwrap();
        assert_eq!(removed, 1);
        let left = repo
            .query(DEFAULT_OWNER_ID, &LogFilter::default())
            .await
            .unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].model, "recent");
    }

    #[tokio::test]
    async fn by_key_groups_usage_per_key_and_skips_keyless_rows() {
        let repo = repo().await;
        repo.append(&entry("a", 200)).await.unwrap(); // key 1
        repo.append(&entry("b", 500)).await.unwrap(); // key 1, 失败
        let mut other_key = entry("c", 200);
        other_key.api_key_id = Some(2);
        repo.append(&other_key).await.unwrap();
        let mut keyless = entry("d", 200);
        keyless.api_key_id = None;
        repo.append(&keyless).await.unwrap();

        let stats = repo.by_key(DEFAULT_OWNER_ID, 24).await.unwrap();
        assert_eq!(stats.len(), 2, "无密钥的请求不参与聚合");
        assert_eq!(stats[0].api_key_id, 1);
        assert_eq!(stats[0].requests, 2);
        assert_eq!(stats[0].failures, 1);
        assert_eq!(stats[0].input_tokens, 200);
        assert_eq!(stats[1].api_key_id, 2);
        assert_eq!(stats[1].requests, 1);
    }

    #[tokio::test]
    async fn prune_removes_more_rows_than_one_batch() {
        let repo = repo().await;
        // 插入超过一个批次（5000）的过期行，验证分批循环真的删完。
        // 用单条 INSERT ... SELECT 生成，逐条插 5000 次太慢。
        sqlx::query(
            "WITH RECURSIVE seq(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM seq WHERE n < 5005) \
             INSERT INTO request_logs (owner_id, request_id, created_at, inbound_protocol, \
             upstream_protocol, model, upstream_model, status, duration_ms) \
             SELECT 1, 'old-' || n, datetime('now', '-100 days'), 'chat', 'chat', 'old', 'old', \
             200, 1 FROM seq",
        )
        .execute(repo.db.pool())
        .await
        .unwrap();
        repo.append(&entry("recent", 200)).await.unwrap();

        let removed = repo.prune(30).await.unwrap();
        assert_eq!(removed, 5_005);
        let left = repo
            .query(DEFAULT_OWNER_ID, &LogFilter::default())
            .await
            .unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].model, "recent");
    }

    #[tokio::test]
    async fn owner_scoping_applies_to_logs() {
        let repo = repo().await;
        let mut other = entry("other", 200);
        other.owner_id = 2;
        repo.append(&other).await.unwrap();

        assert!(
            repo.query(DEFAULT_OWNER_ID, &LogFilter::default())
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(repo.query(2, &LogFilter::default()).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn filter_by_user_id() {
        let repo = repo().await;
        let mut a = entry("alice", 200);
        a.user_id = Some(1);
        let mut b = entry("bob", 200);
        b.user_id = Some(2);
        repo.append(&a).await.unwrap();
        repo.append(&b).await.unwrap();

        let logs = repo
            .query(
                DEFAULT_OWNER_ID,
                &LogFilter {
                    user_id: Some(1),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].model, "alice");
        assert_eq!(logs[0].user_id, Some(1));
    }

    #[tokio::test]
    async fn stats_for_user_are_isolated() {
        let repo = repo().await;
        let mut a = entry("alice", 200);
        a.user_id = Some(1);
        a.input_tokens = 10;
        let mut b = entry("bob", 200);
        b.user_id = Some(2);
        b.input_tokens = 99;
        repo.append(&a).await.unwrap();
        repo.append(&b).await.unwrap();

        let s1 = repo
            .summary_for_user(DEFAULT_OWNER_ID, Some(1), 24)
            .await
            .unwrap();
        assert_eq!(s1.requests, 1);
        assert_eq!(s1.input_tokens, 10);

        let s2 = repo
            .summary_for_user(DEFAULT_OWNER_ID, Some(2), 24)
            .await
            .unwrap();
        assert_eq!(s2.requests, 1);
        assert_eq!(s2.input_tokens, 99);

        let all = repo.summary(DEFAULT_OWNER_ID, 24).await.unwrap();
        assert_eq!(all.requests, 2);
        assert_eq!(all.input_tokens, 109);

        let models = repo
            .by_model_for_user(DEFAULT_OWNER_ID, Some(1), 24, 10)
            .await
            .unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].model, "alice");
    }
}
