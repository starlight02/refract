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
    /// 推理 token。
    pub reasoning_tokens: i64,
    /// 重试次数。
    pub retries: i64,
    /// 错误种类。
    pub error_kind: Option<String>,
    /// 错误消息。
    pub error_message: Option<String>,
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
    /// 推理 token。
    pub reasoning_tokens: u64,
    /// 重试次数。
    pub retries: u32,
    /// 错误种类。
    pub error_kind: Option<String>,
    /// 错误消息。
    pub error_message: Option<String>,
}

impl NewRequestLog {
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
}

/// 日志仓储。
#[derive(Debug, Clone)]
pub struct LogRepo {
    db: Database,
}

impl LogRepo {
    /// 绑定到一个数据库。
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// 追加一条日志。
    pub async fn append(&self, entry: &NewRequestLog) -> Result<i64, StoreError> {
        let id: i64 = sqlx::query(
            "INSERT INTO request_logs \
             (owner_id, request_id, api_key_id, channel_id, channel_name, inbound_protocol, \
              upstream_protocol, transcoded, model, upstream_model, stream, status, ttfb_ms, \
              duration_ms, input_tokens, output_tokens, cached_tokens, reasoning_tokens, retries, \
              error_kind, error_message) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING id",
        )
        .bind(entry.owner_id)
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
        .bind(entry.reasoning_tokens as i64)
        .bind(i64::from(entry.retries))
        .bind(entry.error_kind.as_deref())
        .bind(entry.error_message.as_deref())
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

        let rows = sqlx::query(
            "SELECT id, request_id, created_at, api_key_id, channel_id, channel_name, \
             inbound_protocol, upstream_protocol, transcoded, model, upstream_model, stream, \
             status, ttfb_ms, duration_ms, input_tokens, output_tokens, cached_tokens, \
             reasoning_tokens, retries, error_kind, error_message \
             FROM request_logs \
             WHERE owner_id = ? \
               AND (? IS NULL OR model = ?) \
               AND (? IS NULL OR channel_id = ?) \
               AND (? = 0 OR status >= 400) \
             ORDER BY id DESC LIMIT ? OFFSET ?",
        )
        .bind(owner_id)
        .bind(filter.model.as_deref())
        .bind(filter.model.as_deref())
        .bind(filter.channel_id)
        .bind(filter.channel_id)
        .bind(i64::from(filter.failures_only))
        .bind(i64::from(limit))
        .bind(i64::from(offset))
        .fetch_all(self.db.pool())
        .await?;

        rows.iter().map(Self::from_row).collect()
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
                    COALESCE(SUM(transcoded), 0) AS transcoded \
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
                    COALESCE(SUM(output_tokens), 0) AS output_tokens \
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
                    COALESCE(SUM(output_tokens), 0) AS output_tokens \
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
            reasoning_tokens: row.get("reasoning_tokens"),
            retries: row.get("retries"),
            error_kind: row.get("error_kind"),
            error_message: row.get("error_message"),
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
            reasoning_tokens: 0,
            retries: 0,
            error_kind: None,
            error_message: None,
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
}
