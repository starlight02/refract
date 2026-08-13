//! 数据库连接与迁移。

use std::path::Path;
use std::str::FromStr;

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};

/// 存储层错误。
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// SQL 执行失败。
    #[error("database error: {0}")]
    Sql(#[from] sqlx::Error),
    /// 迁移失败。
    #[error("migration failed: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    /// 存储中的 JSON 列无法解析。
    #[error("corrupt json in column `{column}`: {source}")]
    Json {
        /// 列名。
        column: &'static str,
        /// 底层错误。
        #[source]
        source: serde_json::Error,
    },
    /// 记录不存在。
    #[error("{entity} `{id}` not found")]
    NotFound {
        /// 实体名。
        entity: &'static str,
        /// 标识。
        id: String,
    },
    /// 违反业务不变量。
    #[error("{0}")]
    Invalid(String),
}

impl StoreError {
    /// 构造一个「未找到」错误。
    pub fn not_found(entity: &'static str, id: impl std::fmt::Display) -> Self {
        Self::NotFound {
            entity,
            id: id.to_string(),
        }
    }

    /// 标注 JSON 解析失败的列名。
    pub fn json(column: &'static str) -> impl FnOnce(serde_json::Error) -> Self {
        move |source| Self::Json { column, source }
    }
}

impl From<StoreError> for refract_core::GatewayError {
    fn from(err: StoreError) -> Self {
        match err {
            StoreError::NotFound { .. } => refract_core::GatewayError::not_found(err.to_string()),
            StoreError::Invalid(msg) => refract_core::GatewayError::invalid_request(msg),
            other => refract_core::GatewayError::internal(other.to_string()),
        }
    }
}

/// 数据库句柄。
#[derive(Debug, Clone)]
pub struct Database {
    pool: SqlitePool,
}

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

impl Database {
    /// 打开（并在需要时创建）一个 SQLite 数据库文件，然后跑迁移。
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).ok();
        }
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            // WAL 让读写不互相阻塞 —— 网关在高频写日志的同时要低延迟读配置。
            .journal_mode(SqliteJournalMode::Wal)
            // NORMAL 在 WAL 下已经足够安全，FULL 会让每次写都 fsync。
            .synchronous(SqliteSynchronous::Normal)
            .foreign_keys(true)
            .busy_timeout(std::time::Duration::from_secs(10));

        Self::from_options(options).await
    }

    /// 打开一个内存数据库。用于测试。
    pub async fn open_in_memory() -> Result<Self, StoreError> {
        // `:memory:` 每个连接一个独立库，所以必须用共享缓存 + 单连接池，
        // 否则迁移建的表在下一个连接上看不到。
        let options = SqliteConnectOptions::from_str("sqlite::memory:")
            .expect("static sqlite url")
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(options)
            .await?;
        MIGRATOR.run(&pool).await?;
        Ok(Self { pool })
    }

    async fn from_options(options: SqliteConnectOptions) -> Result<Self, StoreError> {
        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(options)
            .await?;
        MIGRATOR.run(&pool).await?;
        Ok(Self { pool })
    }

    /// 底层连接池。
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// 执行一次轻量查询，确认连接池与 SQLite 都能响应。
    pub async fn ping(&self) -> Result<(), StoreError> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }

    /// 优雅关闭。
    pub async fn close(&self) {
        self.pool.close().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn in_memory_database_runs_migrations() {
        let db = Database::open_in_memory().await.unwrap();
        // 迁移建的表应当存在。
        let tables: Vec<(String,)> =
            sqlx::query_as("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
                .fetch_all(db.pool())
                .await
                .unwrap();
        let names: Vec<&str> = tables.iter().map(|t| t.0.as_str()).collect();
        for expected in [
            "api_keys",
            "channel_endpoints",
            "channel_health",
            "channels",
            "request_logs",
            "settings",
        ] {
            assert!(
                names.contains(&expected),
                "missing table {expected}: {names:?}"
            );
        }
    }

    #[tokio::test]
    async fn ping_reflects_pool_availability() {
        let db = Database::open_in_memory().await.unwrap();
        db.ping().await.unwrap();
        db.close().await;
        assert!(db.ping().await.is_err());
    }

    #[tokio::test]
    async fn foreign_keys_cascade_endpoint_deletion() {
        let db = Database::open_in_memory().await.unwrap();
        sqlx::query("INSERT INTO channels (id, name, kind) VALUES (1, 'c', 'chat')")
            .execute(db.pool())
            .await
            .unwrap();
        sqlx::query("INSERT INTO channel_endpoints (channel_id, protocol) VALUES (1, 'chat')")
            .execute(db.pool())
            .await
            .unwrap();

        sqlx::query("DELETE FROM channels WHERE id = 1")
            .execute(db.pool())
            .await
            .unwrap();

        let remaining: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM channel_endpoints")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(remaining.0, 0, "endpoints must cascade with their channel");
    }

    #[tokio::test]
    async fn endpoint_protocol_is_unique_per_channel() {
        let db = Database::open_in_memory().await.unwrap();
        sqlx::query("INSERT INTO channels (id, name, kind) VALUES (1, 'c', 'aggregate')")
            .execute(db.pool())
            .await
            .unwrap();
        sqlx::query("INSERT INTO channel_endpoints (channel_id, protocol) VALUES (1, 'chat')")
            .execute(db.pool())
            .await
            .unwrap();
        let dup =
            sqlx::query("INSERT INTO channel_endpoints (channel_id, protocol) VALUES (1, 'chat')")
                .execute(db.pool())
                .await;
        assert!(
            dup.is_err(),
            "duplicate protocol per channel must be rejected"
        );
    }
}
