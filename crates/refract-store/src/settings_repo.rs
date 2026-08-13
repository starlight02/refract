//! 运行时设置仓储。
//!
//! 设置以 JSON 形式存在 `settings` 表，读取时反序列化成强类型。
//! 用一张 KV 表而非固定列，是因为设置项会随功能演进增删，加列要迁移，加键不用。

use refract_core::RoutingPolicy;
use serde::{Serialize, de::DeserializeOwned};
use sqlx::Row;

use crate::db::{Database, StoreError};
use crate::health_repo::BreakerPolicy;

/// 「原生优先」开关的设置键（需求 6）。
pub const KEY_NATIVE_FIRST: &str = "routing.native_first";
/// 路由策略的设置键。
pub const KEY_ROUTING_POLICY: &str = "routing.policy";
/// 熔断策略的设置键。
pub const KEY_BREAKER_POLICY: &str = "routing.breaker";
/// 管理端令牌的设置键。
pub const KEY_ADMIN_TOKEN_HASH: &str = "auth.admin_token_hash";
/// 日志保留天数。
pub const KEY_LOG_RETENTION_DAYS: &str = "logs.retention_days";
/// 上游请求默认超时（秒）。
pub const KEY_DEFAULT_TIMEOUT_SECS: &str = "upstream.default_timeout_secs";
/// 默认日志保留天数。
pub const DEFAULT_LOG_RETENTION_DAYS: u32 = 30;
/// 日志保留范围上限，防止配置溢出 SQLite 日期计算。
pub const MAX_LOG_RETENTION_DAYS: u32 = 3650;

/// 设置仓储。
#[derive(Debug, Clone)]
pub struct SettingsRepo {
    db: Database,
}

impl SettingsRepo {
    /// 绑定到一个数据库。
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// 读取一个设置项。不存在时返回 `None`。
    pub async fn get<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>, StoreError> {
        let row = sqlx::query("SELECT value FROM settings WHERE key = ?")
            .bind(key)
            .fetch_optional(self.db.pool())
            .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let raw: String = row.get("value");
        serde_json::from_str(&raw)
            .map(Some)
            .map_err(StoreError::json("settings.value"))
    }

    /// 读取一个设置项，不存在或解析失败时返回默认值。
    ///
    /// 解析失败也回落到默认值并告警：一个坏掉的设置行不应该让网关起不来。
    pub async fn get_or_default<T: DeserializeOwned + Default>(&self, key: &str) -> T {
        match self.get::<T>(key).await {
            Ok(Some(v)) => v,
            Ok(None) => T::default(),
            Err(err) => {
                tracing::warn!(key, %err, "unreadable setting, falling back to default");
                T::default()
            }
        }
    }

    /// 写入一个设置项。
    pub async fn set<T: Serialize>(&self, key: &str, value: &T) -> Result<(), StoreError> {
        let raw = serde_json::to_string(value).map_err(StoreError::json("settings.value"))?;
        sqlx::query(
            "INSERT INTO settings (key, value, updated_at) VALUES (?, ?, datetime('now')) \
             ON CONFLICT (key) DO UPDATE SET value = excluded.value, \
             updated_at = excluded.updated_at",
        )
        .bind(key)
        .bind(raw)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// 删除一个设置项。
    pub async fn remove(&self, key: &str) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM settings WHERE key = ?")
            .bind(key)
            .execute(self.db.pool())
            .await?;
        Ok(())
    }

    /// 列出全部设置项（原始 JSON 字符串）。
    pub async fn all(&self) -> Result<Vec<(String, String)>, StoreError> {
        let rows = sqlx::query("SELECT key, value FROM settings ORDER BY key")
            .fetch_all(self.db.pool())
            .await?;
        Ok(rows
            .iter()
            .map(|r| (r.get::<String, _>("key"), r.get::<String, _>("value")))
            .collect())
    }

    /// 读取路由策略。
    ///
    /// 单独给它一对类型化访问器（而不是让调用方自己拼 key + 泛型），是因为
    /// 它是每个请求都要用的核心配置：key 拼错的代价是「策略静默回落到默认值」，
    /// 这种错误不会报任何错，只会让路由行为莫名其妙。
    pub async fn routing_policy(&self) -> Result<RoutingPolicy, StoreError> {
        Ok(self.get_or_default(KEY_ROUTING_POLICY).await)
    }

    /// 写入路由策略。
    pub async fn set_routing_policy(&self, policy: &RoutingPolicy) -> Result<(), StoreError> {
        self.set(KEY_ROUTING_POLICY, policy).await
    }

    /// 读取熔断策略；缺失或损坏时使用默认值。
    pub async fn breaker_policy(&self) -> Result<BreakerPolicy, StoreError> {
        Ok(self.get_or_default(KEY_BREAKER_POLICY).await)
    }

    /// 校验并保存熔断策略。
    pub async fn set_breaker_policy(&self, policy: &BreakerPolicy) -> Result<(), StoreError> {
        policy.validate().map_err(StoreError::Invalid)?;
        self.set(KEY_BREAKER_POLICY, policy).await
    }

    /// 读取日志保留天数；缺失或损坏时使用 30 天。
    pub async fn log_retention_days(&self) -> u32 {
        match self.get::<u32>(KEY_LOG_RETENTION_DAYS).await {
            Ok(Some(days @ 1..=MAX_LOG_RETENTION_DAYS)) => days,
            Ok(Some(days)) => {
                tracing::warn!(days, "invalid log retention setting, using default");
                DEFAULT_LOG_RETENTION_DAYS
            }
            Ok(None) => DEFAULT_LOG_RETENTION_DAYS,
            Err(error) => {
                tracing::warn!(%error, "unreadable log retention setting, using default");
                DEFAULT_LOG_RETENTION_DAYS
            }
        }
    }

    /// 校验并保存日志保留天数。
    pub async fn set_log_retention_days(&self, days: u32) -> Result<(), StoreError> {
        if !(1..=MAX_LOG_RETENTION_DAYS).contains(&days) {
            return Err(StoreError::Invalid(format!(
                "log retention days must be between 1 and {MAX_LOG_RETENTION_DAYS}"
            )));
        }
        self.set(KEY_LOG_RETENTION_DAYS, &days).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn repo() -> SettingsRepo {
        SettingsRepo::new(Database::open_in_memory().await.unwrap())
    }

    #[tokio::test]
    async fn set_then_get_roundtrips_a_bool() {
        let repo = repo().await;
        assert_eq!(repo.get::<bool>(KEY_NATIVE_FIRST).await.unwrap(), None);

        repo.set(KEY_NATIVE_FIRST, &true).await.unwrap();
        assert_eq!(
            repo.get::<bool>(KEY_NATIVE_FIRST).await.unwrap(),
            Some(true)
        );

        repo.set(KEY_NATIVE_FIRST, &false).await.unwrap();
        assert_eq!(
            repo.get::<bool>(KEY_NATIVE_FIRST).await.unwrap(),
            Some(false)
        );
    }

    #[tokio::test]
    async fn upsert_does_not_duplicate_rows() {
        let repo = repo().await;
        repo.set(KEY_LOG_RETENTION_DAYS, &30_u32).await.unwrap();
        repo.set(KEY_LOG_RETENTION_DAYS, &7_u32).await.unwrap();

        let all = repo.all().await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(
            repo.get::<u32>(KEY_LOG_RETENTION_DAYS).await.unwrap(),
            Some(7)
        );
    }

    #[tokio::test]
    async fn log_retention_is_typed_bounded_and_defaults_safely() {
        let repo = repo().await;
        assert_eq!(repo.log_retention_days().await, DEFAULT_LOG_RETENTION_DAYS);
        repo.set_log_retention_days(90).await.unwrap();
        assert_eq!(repo.log_retention_days().await, 90);
        assert!(repo.set_log_retention_days(0).await.is_err());
        assert!(
            repo.set_log_retention_days(MAX_LOG_RETENTION_DAYS + 1)
                .await
                .is_err()
        );
        assert_eq!(repo.log_retention_days().await, 90);
    }

    #[tokio::test]
    async fn routing_policy_roundtrips_as_a_struct() {
        let repo = repo().await;
        let policy = RoutingPolicy {
            native_first: false,
            max_attempts: 5,
            ..Default::default()
        };
        repo.set(KEY_ROUTING_POLICY, &policy).await.unwrap();

        let back: RoutingPolicy = repo.get(KEY_ROUTING_POLICY).await.unwrap().unwrap();
        assert_eq!(back, policy);
        assert!(!back.native_first);
        assert_eq!(back.max_attempts, 5);
    }

    #[tokio::test]
    async fn missing_key_yields_type_default() {
        let repo = repo().await;
        let policy: RoutingPolicy = repo.get_or_default(KEY_ROUTING_POLICY).await;
        // RoutingPolicy 的默认值是原生优先开启。
        assert!(policy.native_first);
    }

    #[tokio::test]
    async fn corrupt_value_falls_back_instead_of_failing() {
        // 一行坏设置不能让网关起不来。
        let repo = repo().await;
        sqlx::query("INSERT INTO settings (key, value) VALUES (?, 'not json')")
            .bind(KEY_ROUTING_POLICY)
            .execute(repo.db.pool())
            .await
            .unwrap();

        assert!(repo.get::<RoutingPolicy>(KEY_ROUTING_POLICY).await.is_err());
        let fallback: RoutingPolicy = repo.get_or_default(KEY_ROUTING_POLICY).await;
        assert_eq!(fallback, RoutingPolicy::default());
    }

    #[tokio::test]
    async fn breaker_policy_roundtrips_and_validates() {
        let repo = repo().await;
        // 缺省回落默认值。
        assert_eq!(
            repo.breaker_policy().await.unwrap(),
            BreakerPolicy::default()
        );

        let custom = BreakerPolicy {
            failure_threshold: 3,
            base_cooldown_secs: 10,
            max_cooldown_secs: 120,
        };
        repo.set_breaker_policy(&custom).await.unwrap();
        assert_eq!(repo.breaker_policy().await.unwrap(), custom);

        // 非法组合在写入前被拒。
        let bad = BreakerPolicy {
            failure_threshold: 3,
            base_cooldown_secs: 600,
            max_cooldown_secs: 300,
        };
        assert!(repo.set_breaker_policy(&bad).await.is_err());
        assert_eq!(repo.breaker_policy().await.unwrap(), custom);
    }

    #[tokio::test]
    async fn remove_deletes_the_key() {
        let repo = repo().await;
        repo.set(KEY_ADMIN_TOKEN_HASH, &"abc").await.unwrap();
        repo.remove(KEY_ADMIN_TOKEN_HASH).await.unwrap();
        assert_eq!(
            repo.get::<String>(KEY_ADMIN_TOKEN_HASH).await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn all_returns_sorted_keys() {
        let repo = repo().await;
        repo.set(KEY_NATIVE_FIRST, &true).await.unwrap();
        repo.set(KEY_DEFAULT_TIMEOUT_SECS, &120_u32).await.unwrap();

        let all = repo.all().await.unwrap();
        let keys: Vec<&str> = all.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            vec![KEY_NATIVE_FIRST, KEY_DEFAULT_TIMEOUT_SECS]
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>()
        );
    }
}
