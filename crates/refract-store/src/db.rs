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
    /// 唯一约束冲突。
    #[error("{entity} `{id}` already exists")]
    Conflict {
        /// 实体名。
        entity: &'static str,
        /// 标识。
        id: String,
    },
    /// 违反业务不变量。
    #[error("{0}")]
    Invalid(String),
    /// 凭据静态加密失败。拒绝明文落库，写入中止。
    #[error("credential encryption failed: {0}")]
    Encryption(String),
}

impl StoreError {
    /// 构造一个「未找到」错误。
    pub fn not_found(entity: &'static str, id: impl std::fmt::Display) -> Self {
        Self::NotFound {
            entity,
            id: id.to_string(),
        }
    }
    /// 构造一个「已存在」错误。
    pub fn conflict(entity: &'static str, id: impl std::fmt::Display) -> Self {
        Self::Conflict {
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
        match &err {
            StoreError::NotFound { .. } => refract_core::GatewayError::not_found(err.to_string()),
            StoreError::Invalid(_) | StoreError::Conflict { .. } => {
                refract_core::GatewayError::invalid_request(err.to_string())
            }
            _ => refract_core::GatewayError::internal(err.to_string()),
        }
    }
}

/// 是否为 SQLite 唯一约束冲突。
pub(crate) fn is_unique_violation(err: &sqlx::Error) -> bool {
    matches!(err, sqlx::Error::Database(e) if e.is_unique_violation())
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
        if path.as_os_str() == ":memory:" {
            return Self::open_in_memory().await;
        }
        // 父目录不存在时新建,并把新建的目录收紧到 0700。
        let mut parent_created = false;
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            parent_created = !parent.exists();
            std::fs::create_dir_all(parent).ok();
        }
        // 预创建 0600 的库文件:SQLite 新建文件走默认 umask(常见 0644),
        // 事后再 chmod 会留出一段「同机其他用户可读」的窗口。库里有全部
        // 上游密钥,必须在创建瞬间就是 owner-only。WAL/SHM 文件由 SQLite
        // 按主库文件的权限创建,会继承 0600。
        create_owner_only_file(path);
        if parent_created {
            restrict_dir_owner_only(path.parent().expect("checked above"));
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
        // WAL 只有一个写者。连接过多只会在 SQLITE_BUSY 上互相踩，
        // 读多写少的网关 4 个连接已经够用。
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
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

    /// 数据库观测：文件字节数、日志行数、最旧日志时间。
    pub async fn stats(&self) -> Result<(i64, i64, Option<String>), StoreError> {
        let size: i64 = sqlx::query_scalar(
            "SELECT page_count * page_size FROM pragma_page_count, pragma_page_size",
        )
        .fetch_one(self.pool())
        .await?;
        let log_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_logs")
            .fetch_one(self.pool())
            .await?;
        let oldest: Option<String> = sqlx::query_scalar("SELECT MIN(created_at) FROM request_logs")
            .fetch_one(self.pool())
            .await?;
        Ok((size, log_rows, oldest))
    }

    /// `VACUUM INTO` 在线热备：WAL 模式下安全，产物已紧凑无空页。
    /// 路径由调用方在受控目录里生成，单引号转义只是纵深防御。
    pub async fn vacuum_into(&self, target: &std::path::Path) -> Result<(), StoreError> {
        let path = target.to_string_lossy().replace('\'', "''");
        sqlx::query(sqlx::AssertSqlSafe(format!("VACUUM INTO '{path}'")))
            .execute(self.pool())
            .await?;
        // 备份文件是整库拷贝,含全部凭据 —— 与主库同等待遇。
        restrict_owner_only(target);
        Ok(())
    }

    /// 优雅关闭。
    pub async fn close(&self) {
        self.pool.close().await;
    }
}

/// 以 0600 预创建文件;已存在则不做任何事。
#[cfg(unix)]
fn create_owner_only_file(path: &Path) {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .ok();
}

#[cfg(not(unix))]
fn create_owner_only_file(_path: &Path) {}

/// 把文件权限收紧到 0600(owner 读写)。失败静默:权限只是纵深防御,
/// 不应让备份/开库流程因此失败。
#[cfg(unix)]
fn restrict_owner_only(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mut perms = meta.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(path, perms).ok();
    }
}

#[cfg(not(unix))]
fn restrict_owner_only(_path: &Path) {}

/// 把新建目录权限收紧到 0700。
#[cfg(unix)]
fn restrict_dir_owner_only(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mut perms = meta.permissions();
        perms.set_mode(0o700);
        std::fs::set_permissions(path, perms).ok();
    }
}

#[cfg(not(unix))]
fn restrict_dir_owner_only(_path: &Path) {}

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
            "users",
            "verification_codes",
            "wallet_ledger",
            "wallets",
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

    #[cfg(unix)]
    #[tokio::test]
    async fn new_database_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("nested").join("perm.db");
        let db = Database::open(&db_path).await.unwrap();
        db.ping().await.unwrap();

        let mode = std::fs::metadata(&db_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "new db file must be 0600, got {mode:o}");

        let dir_mode = std::fs::metadata(db_path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            dir_mode, 0o700,
            "new parent dir must be 0700, got {dir_mode:o}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn vacuum_into_backup_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("src.db")).await.unwrap();
        let backup = dir.path().join("backup.db");
        db.vacuum_into(&backup).await.unwrap();

        let mode = std::fs::metadata(&backup).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "backup file must be 0600, got {mode:o}");
    }
}
