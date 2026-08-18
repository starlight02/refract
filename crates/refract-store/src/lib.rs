//! 持久化层。
//!
//! SQLite 存储 + 仓储接口。所有查询方法都带 `owner_id` 参数（当前恒为
//! [`refract_core::DEFAULT_OWNER_ID`]），为将来的多用户预留。

// lint 配置统一在 workspace `Cargo.toml` 的 [workspace.lints] 里维护。

pub mod channel_repo;
pub mod crypto;
pub mod db;
pub mod health_repo;
pub mod key_repo;
pub mod log_repo;
pub mod settings_repo;

pub use channel_repo::ChannelRepo;
pub use crypto::{
    CryptoError, decrypt_credential, encrypt_credential, is_encrypted, parse_master_key,
};
pub use db::{Database, StoreError};
pub use health_repo::{BreakerPolicy, EndpointHealth, HealthRepo};
pub use key_repo::{ApiKey, ApiKeyRepo, ExportedApiKey, NewApiKey};
pub use log_repo::{
    KeyUsageStat, LogFilter, LogRepo, ModelStat, NewRequestLog, RequestLog, StatsSummary,
};
pub use settings_repo::{
    BackupSettings, GlobalLimits, IpLimits, ModelPrice, SettingsRepo, default_backup_keep,
    price_for,
};

/// 确保影响行数大于 0，否则返回 EntityNotFound 错误。
#[inline]
pub(crate) fn ensure_affected(
    affected: u64,
    entity: &'static str,
    id: impl std::fmt::Display,
) -> Result<(), StoreError> {
    if affected == 0 {
        Err(StoreError::not_found(entity, id))
    } else {
        Ok(())
    }
}
