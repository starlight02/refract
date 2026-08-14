//! 渠道健康度仓储 —— 熔断与统计的持久化面。
//!
//! 为什么健康度要落库而不是只放内存：网关重启后如果忘记「某个端点刚刚连续
//! 失败 20 次」，就会立刻把流量再打过去，把上游的封禁窗口续上。重启后保留
//! 熔断状态是正确行为，不是可选优化。
//!
//! 粒度是 `(channel_id, protocol)` 而非 `channel_id`：聚合渠道的各协议端点
//! 打的是不同的上游地址与密钥，一个挂了不代表其他也挂了（需求 3）。

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use chrono::{DateTime, Duration, Utc};
use refract_core::{ChannelId, Protocol};
use sqlx::Row;

use crate::db::{Database, StoreError};
use crate::key_repo::parse_ts;

/// 一个协议端点的健康快照。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct EndpointHealth {
    /// 所属渠道。
    pub channel_id: ChannelId,
    /// 端点协议。
    pub protocol: Protocol,
    /// 连续失败次数。成功一次即归零。
    pub consecutive_fails: u32,
    /// 累计请求数。
    pub total_requests: u64,
    /// 累计失败数。
    pub total_failures: u64,
    /// 最近成功时刻。
    pub last_success_at: Option<DateTime<Utc>>,
    /// 最近失败时刻。
    pub last_failure_at: Option<DateTime<Utc>>,
    /// 最近一次错误的摘要。
    pub last_error: Option<String>,
    /// 熔断到期时刻；`None` 表示未熔断。
    pub suspended_until: Option<DateTime<Utc>>,
    /// 指数平滑后的平均延迟（毫秒）。
    pub avg_latency_ms: u64,
}

impl EndpointHealth {
    /// 在给定时刻是否处于熔断中。
    ///
    /// 显式传入 `now` 而不是内部取当前时间：路由决策要在同一个时间基准上
    /// 比较多个端点，否则同一批判断可能横跨熔断边界。
    pub fn is_suspended_at(&self, now: DateTime<Utc>) -> bool {
        self.suspended_until.is_some_and(|until| until > now)
    }

    /// 失败率，`0.0..=1.0`。没有样本时返回 0。
    pub fn failure_rate(&self) -> f64 {
        if self.total_requests == 0 {
            return 0.0;
        }
        self.total_failures as f64 / self.total_requests as f64
    }
}

/// 熔断策略。
///
/// 阈值与退避时长做成参数而非常量：不同上游的容忍度差异很大，个人自用场景
/// 也可能希望完全关掉熔断（`threshold = 0`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct BreakerPolicy {
    /// 连续失败达到该值即熔断。0 表示不熔断。
    pub failure_threshold: u32,
    /// 首次熔断的时长（秒）。
    pub base_cooldown_secs: u32,
    /// 熔断时长上限（秒）。
    pub max_cooldown_secs: u32,
}

impl Default for BreakerPolicy {
    fn default() -> Self {
        Self {
            failure_threshold: 5,
            base_cooldown_secs: 30,
            max_cooldown_secs: 900,
        }
    }
}

/// 熔断参数的取值上限。
///
/// 阈值上限防手滑（10 万次连续失败才熔断等于没配）；冷却上限 24 小时 ——
/// 再长就不是「等上游恢复」而是「忘了这个端点」，那应该禁用渠道而不是熔断。
pub const MAX_FAILURE_THRESHOLD: u32 = 1_000;
/// 冷却时长上限（秒）。
pub const MAX_COOLDOWN_SECS: u32 = 86_400;

impl BreakerPolicy {
    /// 校验参数范围。管理 API 的写路径必须先过这里。
    pub fn validate(&self) -> Result<(), String> {
        if self.failure_threshold > MAX_FAILURE_THRESHOLD {
            return Err(format!(
                "failure_threshold must be at most {MAX_FAILURE_THRESHOLD} (0 disables the breaker)"
            ));
        }
        if self.base_cooldown_secs == 0 || self.base_cooldown_secs > MAX_COOLDOWN_SECS {
            return Err(format!(
                "base_cooldown_secs must be between 1 and {MAX_COOLDOWN_SECS}"
            ));
        }
        if self.max_cooldown_secs < self.base_cooldown_secs
            || self.max_cooldown_secs > MAX_COOLDOWN_SECS
        {
            return Err(format!(
                "max_cooldown_secs must be between base_cooldown_secs and {MAX_COOLDOWN_SECS}"
            ));
        }
        Ok(())
    }

    /// 计算第 n 次越阈值时的退避时长。
    ///
    /// 指数退避并封顶：一直失败的端点不该每 30 秒被重试一次，但也不该永久
    /// 拉黑 —— 上游恢复后必须能自动回来。
    pub fn cooldown_for(&self, consecutive_fails: u32) -> Duration {
        if self.failure_threshold == 0 || consecutive_fails < self.failure_threshold {
            return Duration::zero();
        }
        let over = consecutive_fails - self.failure_threshold;
        // 移位次数封在 16 以内，避免 u32 溢出。
        let factor = 1_u64 << over.min(16);
        let secs = (self.base_cooldown_secs as u64)
            .saturating_mul(factor)
            .min(self.max_cooldown_secs as u64);
        Duration::seconds(secs as i64)
    }
}

/// 健康度仓储。
///
/// 熔断状态同时维护一份进程内缓存：路由规划在每个请求的热路径上要判断
/// 每个候选是否熔断，逐个查 SQLite 的开销与延迟都不可接受。缓存采用
/// 写穿策略 —— 所有写路径（成功/失败/手动重置）同步更新缓存，读路径
/// `(channel, protocol)` -> 熔断到期时刻的共享缓存。
type SuspensionCache = Arc<RwLock<HashMap<(ChannelId, Protocol), DateTime<Utc>>>>;

/// 只碰内存；重启后由 [`HealthRepo::warm_cache`] 从库里恢复。
#[derive(Debug, Clone)]
pub struct HealthRepo {
    db: Database,
    /// 熔断策略可在运行时被管理端更新，所以放在共享锁后面。
    /// 读端只在失败路径上拿一次快照，锁竞争可忽略。
    policy: Arc<RwLock<BreakerPolicy>>,
    /// 未熔断的端点不在表里。
    suspensions: SuspensionCache,
}

/// 延迟平滑系数：新样本占 25%。
///
/// 用 EWMA 而不是「总时长 / 总次数」：真均值会被历史稀释，上游突然变慢时
/// 反应太迟；EWMA 只需一行状态，且对近期更敏感。
const LATENCY_ALPHA: f64 = 0.25;
macro_rules! health_cols {
    () => {
        "channel_id, protocol, consecutive_fails, total_requests, total_failures, \
         last_success_at, last_failure_at, last_error, suspended_until, avg_latency_ms"
    };
}

const SELECT_HEALTH_BY_ENDPOINT: &str = concat!(
    "SELECT ",
    health_cols!(),
    " FROM channel_health WHERE channel_id = ? AND protocol = ?"
);
const SELECT_ALL_HEALTH: &str = concat!(
    "SELECT ",
    health_cols!(),
    " FROM channel_health ORDER BY channel_id, protocol"
);

impl HealthRepo {
    /// 用默认熔断策略构造。
    pub fn new(db: Database) -> Self {
        Self::with_policy(db, BreakerPolicy::default())
    }

    /// 用指定熔断策略构造。
    pub fn with_policy(db: Database, policy: BreakerPolicy) -> Self {
        Self {
            db,
            policy: Arc::new(RwLock::new(policy)),
            suspensions: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// 当前生效的熔断策略。
    pub fn policy(&self) -> BreakerPolicy {
        *self.policy.read().expect("breaker policy lock poisoned")
    }

    /// 运行时更新熔断策略。只影响之后的失败判定；已生效的熔断不回溯调整。
    pub fn set_policy(&self, policy: BreakerPolicy) {
        *self.policy.write().expect("breaker policy lock poisoned") = policy;
    }

    /// 从库里恢复熔断缓存。启动时调用一次；不调用则缓存从空开始，
    /// 重启前的熔断状态要等端点再次失败才会重新生效。
    pub async fn warm_cache(&self) -> Result<(), StoreError> {
        let all = self.all().await?;
        let now = Utc::now();
        let mut cache = self.suspensions.write().expect("suspension cache poisoned");
        cache.clear();
        for h in all {
            if let Some(until) = h.suspended_until
                && until > now
            {
                cache.insert((h.channel_id, h.protocol), until);
            }
        }
        Ok(())
    }

    /// 端点当前的熔断到期时刻（读内存缓存，不碰数据库）。
    ///
    /// 返回 `None` 表示未熔断或已过期。过期条目顺手清掉，避免缓存无限增长。
    pub fn suspended_until(
        &self,
        channel_id: ChannelId,
        protocol: Protocol,
    ) -> Option<DateTime<Utc>> {
        let key = (channel_id, protocol);
        {
            let cache = self.suspensions.read().expect("suspension cache poisoned");
            match cache.get(&key) {
                Some(&until) if until > Utc::now() => return Some(until),
                Some(_) => {}
                None => return None,
            }
        }
        self.suspensions
            .write()
            .expect("suspension cache poisoned")
            .remove(&key);
        None
    }

    /// 写穿缓存：设置或清除一个端点的熔断状态。
    fn cache_suspension(
        &self,
        channel_id: ChannelId,
        protocol: Protocol,
        until: Option<DateTime<Utc>>,
    ) {
        let mut cache = self.suspensions.write().expect("suspension cache poisoned");
        match until {
            Some(until) if until > Utc::now() => {
                cache.insert((channel_id, protocol), until);
            }
            _ => {
                cache.remove(&(channel_id, protocol));
            }
        }
    }

    /// 记录一次成功。
    ///
    /// 副作用：清零连续失败数、解除熔断、更新 EWMA 延迟。上游恢复后第一次
    /// 成功就该完全恢复它的资格，不该有「观察期」—— 那只会让恢复变慢。
    pub async fn record_success(
        &self,
        channel_id: ChannelId,
        protocol: Protocol,
        latency_ms: u64,
    ) -> Result<bool, StoreError> {
        // 成功前处于挂起中 = 本次成功解除了熔断（值得通知一声）。
        let was_suspended = self.suspended_until(channel_id, protocol).is_some();
        sqlx::query(
            "INSERT INTO channel_health \
             (channel_id, protocol, consecutive_fails, total_requests, total_failures, \
              last_success_at, suspended_until, avg_latency_ms) \
             VALUES (?, ?, 0, 1, 0, datetime('now'), NULL, ?) \
             ON CONFLICT (channel_id, protocol) DO UPDATE SET \
               consecutive_fails = 0, \
               total_requests = total_requests + 1, \
               last_success_at = datetime('now'), \
               suspended_until = NULL, \
               avg_latency_ms = CAST( \
                 CASE WHEN avg_latency_ms = 0 THEN excluded.avg_latency_ms \
                      ELSE avg_latency_ms * (1 - ?) + excluded.avg_latency_ms * ? \
                 END AS INTEGER)",
        )
        .bind(channel_id)
        .bind(protocol.as_str())
        .bind(latency_ms as i64)
        .bind(LATENCY_ALPHA)
        .bind(LATENCY_ALPHA)
        .execute(self.db.pool())
        .await?;
        self.cache_suspension(channel_id, protocol, None);
        Ok(was_suspended)
    }

    /// 记录一次失败，返回更新后的健康快照。
    ///
    /// 返回快照而不是 `()`：调用方（重试逻辑）需要立刻知道这次失败有没有
    /// 触发熔断，再去查一次是多余的往返。
    ///
    /// `retry_after` 是上游通过 `Retry-After` 头声明的等待时长：即使连续
    /// 失败数未达熔断阈值，也按上游说的时长悬停 —— 上游明确说了「别来」，
    /// 继续打过去只会把限流窗口越拖越长。两者同时存在时取更晚的时刻。
    pub async fn record_failure(
        &self,
        channel_id: ChannelId,
        protocol: Protocol,
        error: &str,
        retry_after: Option<std::time::Duration>,
    ) -> Result<EndpointHealth, StoreError> {
        // 先自增，再按新的连续失败数决定退避 —— 退避时长依赖自增后的值，
        // 所以不能在一条 SQL 里算完。两条语句放在事务里，避免并发下读到
        // 中间态。
        let mut tx = self.db.pool().begin().await?;

        sqlx::query(
            "INSERT INTO channel_health \
             (channel_id, protocol, consecutive_fails, total_requests, total_failures, \
              last_failure_at, last_error) \
             VALUES (?, ?, 1, 1, 1, datetime('now'), ?) \
             ON CONFLICT (channel_id, protocol) DO UPDATE SET \
               consecutive_fails = consecutive_fails + 1, \
               total_requests = total_requests + 1, \
               total_failures = total_failures + 1, \
               last_failure_at = datetime('now'), \
               last_error = excluded.last_error",
        )
        .bind(channel_id)
        .bind(protocol.as_str())
        .bind(truncate_error(error))
        .execute(&mut *tx)
        .await?;

        let fails: i64 = sqlx::query(
            "SELECT consecutive_fails FROM channel_health WHERE channel_id = ? AND protocol = ?",
        )
        .bind(channel_id)
        .bind(protocol.as_str())
        .fetch_one(&mut *tx)
        .await?
        .get(0);

        let now = Utc::now();
        let breaker_until = match self.policy().cooldown_for(fails.max(0) as u32) {
            d if d > Duration::zero() => Some(now + d),
            _ => None,
        };
        let hold_until = retry_after
            .and_then(|d| Duration::from_std(d).ok())
            .map(|d| now + d);
        let suspend_until = match (breaker_until, hold_until) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };

        if let Some(until) = suspend_until {
            sqlx::query(
                "UPDATE channel_health SET suspended_until = ? \
                 WHERE channel_id = ? AND protocol = ?",
            )
            .bind(until.to_rfc3339())
            .bind(channel_id)
            .bind(protocol.as_str())
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        if suspend_until.is_some() {
            self.cache_suspension(channel_id, protocol, suspend_until);
        }
        self.get(channel_id, protocol)
            .await?
            .ok_or_else(|| StoreError::not_found("channel health", channel_id))
    }
    /// 取一个端点的健康快照。
    pub async fn get(
        &self,
        channel_id: ChannelId,
        protocol: Protocol,
    ) -> Result<Option<EndpointHealth>, StoreError> {
        let row = sqlx::query(SELECT_HEALTH_BY_ENDPOINT)
            .bind(channel_id)
            .bind(protocol.as_str())
            .fetch_optional(self.db.pool())
            .await?;
        row.as_ref().map(from_row).transpose()
    }

    /// 全量健康快照。路由层启动时一次性载入内存。
    pub async fn all(&self) -> Result<Vec<EndpointHealth>, StoreError> {
        let rows = sqlx::query(SELECT_ALL_HEALTH)
            .fetch_all(self.db.pool())
            .await?;
        rows.iter().map(from_row).collect()
    }

    /// 手动解除熔断（管理端「立即重试」按钮）。
    pub async fn reset(&self, channel_id: ChannelId, protocol: Protocol) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE channel_health SET consecutive_fails = 0, suspended_until = NULL \
             WHERE channel_id = ? AND protocol = ?",
        )
        .bind(channel_id)
        .bind(protocol.as_str())
        .execute(self.db.pool())
        .await?;
        self.cache_suspension(channel_id, protocol, None);
        Ok(())
    }

    /// 清空累计计数，保留熔断状态。用于「统计归零」而非「恢复端点」。
    pub async fn clear_counters(&self) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE channel_health SET total_requests = 0, total_failures = 0, avg_latency_ms = 0",
        )
        .execute(self.db.pool())
        .await?;
        Ok(())
    }
}

/// 错误摘要截断长度。
///
/// 上游的错误体可能是几十 KB 的 HTML，全存进去只会把库撑大而不增加信息量。
const MAX_ERROR_LEN: usize = 500;

fn truncate_error(error: &str) -> String {
    let trimmed = error.trim();
    if trimmed.chars().count() <= MAX_ERROR_LEN {
        return trimmed.to_owned();
    }
    // 按字符而非字节截断，避免切断多字节序列。
    trimmed.chars().take(MAX_ERROR_LEN).collect()
}

fn from_row(row: &sqlx::sqlite::SqliteRow) -> Result<EndpointHealth, StoreError> {
    let protocol_raw: String = row.try_get("protocol")?;
    let protocol = protocol_raw.parse::<Protocol>().map_err(|_| {
        StoreError::Invalid(format!("unknown protocol in health row: {protocol_raw}"))
    })?;
    let fails: i64 = row.try_get("consecutive_fails")?;
    let requests: i64 = row.try_get("total_requests")?;
    let failures: i64 = row.try_get("total_failures")?;
    let latency: i64 = row.try_get("avg_latency_ms")?;

    Ok(EndpointHealth {
        channel_id: row.try_get("channel_id")?,
        protocol,
        consecutive_fails: fails.max(0) as u32,
        total_requests: requests.max(0) as u64,
        total_failures: failures.max(0) as u64,
        last_success_at: parse_ts(row.try_get("last_success_at")?),
        last_failure_at: parse_ts(row.try_get("last_failure_at")?),
        last_error: row.try_get("last_error")?,
        suspended_until: parse_ts(row.try_get("suspended_until")?),
        avg_latency_ms: latency.max(0) as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    async fn repo() -> HealthRepo {
        HealthRepo::new(seeded_db().await)
    }

    /// 建库并插入一个渠道 —— 健康行有外键，必须先有渠道。
    async fn seeded_db() -> Database {
        let db = Database::open_in_memory().await.unwrap();
        insert_channel(&db, 1, "c").await;
        db
    }

    async fn insert_channel(db: &Database, id: i64, name: &str) {
        sqlx::query("INSERT INTO channels (id, name, kind) VALUES (?, ?, 'chat')")
            .bind(id)
            .bind(name)
            .execute(db.pool())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn unseen_endpoint_has_no_health_row() {
        let repo = repo().await;
        assert!(repo.get(1, Protocol::Chat).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn first_success_creates_the_row() {
        let repo = repo().await;
        repo.record_success(1, Protocol::Chat, 400).await.unwrap();

        let h = repo.get(1, Protocol::Chat).await.unwrap().unwrap();
        assert_eq!(h.consecutive_fails, 0);
        assert_eq!(h.total_requests, 1);
        assert_eq!(h.total_failures, 0);
        // 首个样本直接作为初值，不与 0 做平滑 —— 否则第一次的延迟会被砍到 1/4。
        assert_eq!(h.avg_latency_ms, 400);
        assert!(h.last_success_at.is_some());
        assert!(!h.is_suspended_at(Utc::now()));
    }

    #[tokio::test]
    async fn latency_is_smoothed_not_replaced() {
        let repo = repo().await;
        repo.record_success(1, Protocol::Chat, 400).await.unwrap();
        repo.record_success(1, Protocol::Chat, 800).await.unwrap();

        let h = repo.get(1, Protocol::Chat).await.unwrap().unwrap();
        // 400 * 0.75 + 800 * 0.25 = 500
        assert_eq!(h.avg_latency_ms, 500);
        assert_eq!(h.total_requests, 2);
    }

    #[tokio::test]
    async fn failures_accumulate_and_record_the_error() {
        let repo = repo().await;
        let h = repo
            .record_failure(1, Protocol::Chat, "  upstream 502  ", None)
            .await
            .unwrap();
        assert_eq!(h.consecutive_fails, 1);
        assert_eq!(h.total_failures, 1);
        assert_eq!(h.total_requests, 1);
        // 错误摘要去掉首尾空白。
        assert_eq!(h.last_error.as_deref(), Some("upstream 502"));
        assert!(
            h.suspended_until.is_none(),
            "one failure must not trip the breaker"
        );
    }

    #[tokio::test]
    async fn breaker_trips_at_the_threshold() {
        let repo = repo().await;
        let policy = repo.policy();

        for i in 1..policy.failure_threshold {
            let h = repo
                .record_failure(1, Protocol::Chat, "boom", None)
                .await
                .unwrap();
            assert!(
                h.suspended_until.is_none(),
                "must not suspend before threshold (failure {i})"
            );
        }

        let tripped = repo
            .record_failure(1, Protocol::Chat, "boom", None)
            .await
            .unwrap();
        assert_eq!(tripped.consecutive_fails, policy.failure_threshold);
        assert!(tripped.is_suspended_at(Utc::now()));
    }

    #[tokio::test]
    async fn success_clears_the_breaker_immediately() {
        let repo = repo().await;
        for _ in 0..repo.policy().failure_threshold {
            repo.record_failure(1, Protocol::Chat, "boom", None)
                .await
                .unwrap();
        }
        assert!(
            repo.get(1, Protocol::Chat)
                .await
                .unwrap()
                .unwrap()
                .is_suspended_at(Utc::now())
        );

        repo.record_success(1, Protocol::Chat, 100).await.unwrap();
        let h = repo.get(1, Protocol::Chat).await.unwrap().unwrap();
        assert_eq!(h.consecutive_fails, 0);
        assert!(h.suspended_until.is_none());
        // 累计计数不清零 —— 它们是历史统计，不是熔断状态。
        assert_eq!(h.total_failures, u64::from(repo.policy().failure_threshold));
    }

    #[tokio::test]
    async fn protocols_of_one_channel_are_tracked_separately() {
        let repo = repo().await;
        for _ in 0..repo.policy().failure_threshold {
            repo.record_failure(1, Protocol::Chat, "boom", None)
                .await
                .unwrap();
        }
        repo.record_success(1, Protocol::Messages, 200)
            .await
            .unwrap();

        let chat = repo.get(1, Protocol::Chat).await.unwrap().unwrap();
        let messages = repo.get(1, Protocol::Messages).await.unwrap().unwrap();
        assert!(chat.is_suspended_at(Utc::now()));
        assert!(!messages.is_suspended_at(Utc::now()));
    }

    #[tokio::test]
    async fn manual_reset_restores_the_endpoint() {
        let repo = repo().await;
        for _ in 0..repo.policy().failure_threshold {
            repo.record_failure(1, Protocol::Chat, "boom", None)
                .await
                .unwrap();
        }
        repo.reset(1, Protocol::Chat).await.unwrap();

        let h = repo.get(1, Protocol::Chat).await.unwrap().unwrap();
        assert_eq!(h.consecutive_fails, 0);
        assert!(!h.is_suspended_at(Utc::now()));
    }

    #[tokio::test]
    async fn cooldown_backs_off_exponentially_and_caps() {
        let policy = BreakerPolicy {
            failure_threshold: 2,
            base_cooldown_secs: 10,
            max_cooldown_secs: 40,
        };
        assert_eq!(policy.cooldown_for(1), Duration::zero());
        assert_eq!(policy.cooldown_for(2), Duration::seconds(10));
        assert_eq!(policy.cooldown_for(3), Duration::seconds(20));
        assert_eq!(policy.cooldown_for(4), Duration::seconds(40));
        // 封顶后不再增长。
        assert_eq!(policy.cooldown_for(9), Duration::seconds(40));
        // 极端值不能溢出 panic。
        assert_eq!(policy.cooldown_for(u32::MAX), Duration::seconds(40));
    }

    #[test]
    fn breaker_policy_validation_catches_bad_combinations() {
        assert!(BreakerPolicy::default().validate().is_ok());
        // 0 阈值 = 关闭熔断，合法。
        assert!(
            BreakerPolicy {
                failure_threshold: 0,
                ..Default::default()
            }
            .validate()
            .is_ok()
        );
        // base 为 0、max < base、超上限都要拒。
        assert!(
            BreakerPolicy {
                base_cooldown_secs: 0,
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            BreakerPolicy {
                base_cooldown_secs: 600,
                max_cooldown_secs: 300,
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            BreakerPolicy {
                failure_threshold: MAX_FAILURE_THRESHOLD + 1,
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            BreakerPolicy {
                max_cooldown_secs: MAX_COOLDOWN_SECS + 1,
                ..Default::default()
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn policy_hot_update_is_visible_through_clones() {
        // HealthRepo 在 AppState 里被 clone 共享，热更新必须对所有克隆可见。
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let repo =
            rt.block_on(async { HealthRepo::new(Database::open_in_memory().await.unwrap()) });
        let clone = repo.clone();
        repo.set_policy(BreakerPolicy {
            failure_threshold: 42,
            ..Default::default()
        });
        assert_eq!(clone.policy().failure_threshold, 42);
    }

    #[tokio::test]
    async fn zero_threshold_disables_the_breaker() {
        let db = seeded_db().await;
        let repo = HealthRepo::with_policy(
            db,
            BreakerPolicy {
                failure_threshold: 0,
                ..Default::default()
            },
        );

        for _ in 0..50 {
            repo.record_failure(1, Protocol::Chat, "boom", None)
                .await
                .unwrap();
        }
        let h = repo.get(1, Protocol::Chat).await.unwrap().unwrap();
        assert_eq!(h.consecutive_fails, 50);
        assert!(h.suspended_until.is_none());
    }

    #[tokio::test]
    async fn failure_rate_needs_no_samples_to_be_safe() {
        let h = EndpointHealth {
            channel_id: 1,
            protocol: Protocol::Chat,
            consecutive_fails: 0,
            total_requests: 0,
            total_failures: 0,
            last_success_at: None,
            last_failure_at: None,
            last_error: None,
            suspended_until: None,
            avg_latency_ms: 0,
        };
        assert_eq!(h.failure_rate(), 0.0);
    }

    #[tokio::test]
    async fn failure_rate_reflects_counts() {
        let repo = repo().await;
        repo.record_success(1, Protocol::Chat, 100).await.unwrap();
        repo.record_failure(1, Protocol::Chat, "boom", None)
            .await
            .unwrap();
        repo.record_success(1, Protocol::Chat, 100).await.unwrap();
        repo.record_failure(1, Protocol::Chat, "boom", None)
            .await
            .unwrap();

        let h = repo.get(1, Protocol::Chat).await.unwrap().unwrap();
        assert_eq!(h.total_requests, 4);
        assert_eq!(h.failure_rate(), 0.5);
    }

    #[tokio::test]
    async fn long_errors_are_truncated_on_char_boundaries() {
        let repo = repo().await;
        let long = "错".repeat(MAX_ERROR_LEN + 100);
        let h = repo
            .record_failure(1, Protocol::Chat, &long, None)
            .await
            .unwrap();
        let stored = h.last_error.unwrap();
        assert_eq!(stored.chars().count(), MAX_ERROR_LEN);
        assert!(stored.starts_with('错'));
    }

    #[tokio::test]
    async fn all_lists_every_endpoint_sorted() {
        let repo = repo().await;
        insert_channel(&repo.db, 2, "d").await;

        repo.record_success(2, Protocol::Gemini, 10).await.unwrap();
        repo.record_success(1, Protocol::Messages, 10)
            .await
            .unwrap();
        repo.record_success(1, Protocol::Chat, 10).await.unwrap();

        let all = repo.all().await.unwrap();
        let keys: Vec<_> = all.iter().map(|h| (h.channel_id, h.protocol)).collect();
        assert_eq!(
            keys,
            vec![
                (1, Protocol::Chat),
                (1, Protocol::Messages),
                (2, Protocol::Gemini)
            ]
        );
    }

    #[tokio::test]
    async fn clear_counters_keeps_suspension() {
        let repo = repo().await;
        for _ in 0..repo.policy().failure_threshold {
            repo.record_failure(1, Protocol::Chat, "boom", None)
                .await
                .unwrap();
        }
        repo.clear_counters().await.unwrap();

        let h = repo.get(1, Protocol::Chat).await.unwrap().unwrap();
        assert_eq!(h.total_requests, 0);
        assert_eq!(h.total_failures, 0);
        assert!(
            h.is_suspended_at(Utc::now()),
            "clearing stats must not silently revive a broken endpoint"
        );
    }

    #[tokio::test]
    async fn retry_after_suspends_even_below_the_threshold() {
        let repo = repo().await;
        // 第一次失败就带 Retry-After：上游明说了等 120 秒，必须悬停，
        // 不能等连续失败攒够阈值。
        let h = repo
            .record_failure(
                1,
                Protocol::Chat,
                "429 too many requests",
                Some(std::time::Duration::from_secs(120)),
            )
            .await
            .unwrap();
        assert_eq!(h.consecutive_fails, 1);
        assert!(h.is_suspended_at(Utc::now()), "Retry-After 必须立刻悬停");
        // 悬停时长以 Retry-After 为准（允许秒级误差）。
        let until = h.suspended_until.unwrap();
        let wait = (until - Utc::now()).num_seconds();
        assert!((115..=121).contains(&wait), "悬停 {wait}s，应约 120s");
        // 缓存同步可见。
        assert!(repo.suspended_until(1, Protocol::Chat).is_some());
    }

    #[tokio::test]
    async fn retry_after_and_breaker_take_the_later_deadline() {
        let db = seeded_db().await;
        let repo = HealthRepo::with_policy(
            db,
            BreakerPolicy {
                failure_threshold: 1,
                base_cooldown_secs: 600,
                max_cooldown_secs: 600,
            },
        );
        // 阈值退避 600s > Retry-After 5s：取更晚的 600s。
        let h = repo
            .record_failure(
                1,
                Protocol::Chat,
                "boom",
                Some(std::time::Duration::from_secs(5)),
            )
            .await
            .unwrap();
        let wait = (h.suspended_until.unwrap() - Utc::now()).num_seconds();
        assert!(wait > 500, "应取熔断退避的 600s，实际 {wait}s");
    }

    #[tokio::test]
    async fn suspension_cache_follows_every_write_path() {
        let repo = repo().await;
        // 失败到熔断 → 缓存可见。
        for _ in 0..repo.policy().failure_threshold {
            repo.record_failure(1, Protocol::Chat, "boom", None)
                .await
                .unwrap();
        }
        assert!(repo.suspended_until(1, Protocol::Chat).is_some());

        // 成功 → 缓存清除。
        repo.record_success(1, Protocol::Chat, 50).await.unwrap();
        assert!(repo.suspended_until(1, Protocol::Chat).is_none());

        // 再次熔断 → 手动 reset → 缓存清除。
        for _ in 0..repo.policy().failure_threshold {
            repo.record_failure(1, Protocol::Chat, "boom", None)
                .await
                .unwrap();
        }
        assert!(repo.suspended_until(1, Protocol::Chat).is_some());
        repo.reset(1, Protocol::Chat).await.unwrap();
        assert!(repo.suspended_until(1, Protocol::Chat).is_none());
    }

    #[tokio::test]
    async fn warm_cache_restores_suspensions_after_restart() {
        let db = seeded_db().await;
        let repo = HealthRepo::new(db.clone());
        for _ in 0..repo.policy().failure_threshold {
            repo.record_failure(1, Protocol::Chat, "boom", None)
                .await
                .unwrap();
        }

        // 模拟重启：同一个库新建 repo，缓存是冷的。
        let reborn = HealthRepo::new(db);
        assert!(
            reborn.suspended_until(1, Protocol::Chat).is_none(),
            "冷缓存看不到熔断"
        );
        reborn.warm_cache().await.unwrap();
        assert!(
            reborn.suspended_until(1, Protocol::Chat).is_some(),
            "预热后必须恢复熔断状态"
        );
    }

    #[tokio::test]
    async fn health_rows_die_with_their_channel() {
        let repo = repo().await;
        repo.record_success(1, Protocol::Chat, 10).await.unwrap();
        sqlx::query("DELETE FROM channels WHERE id = 1")
            .execute(repo.db.pool())
            .await
            .unwrap();
        assert!(
            repo.get(1, Protocol::Chat).await.unwrap().is_none(),
            "orphan health rows would resurrect stale breaker state on id reuse"
        );
    }
}
