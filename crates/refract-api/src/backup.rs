//! 自动备份与在线 SQLite 备份文件管理。

use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::Utc;
use refract_store::StoreError;
use serde::{Deserialize, Serialize};

use crate::state::AppState;

/// 备份文件信息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupInfo {
    /// 文件名，格式 `refract-YYYYmmdd-HHMMSS.db`。
    pub name: String,
    /// 文件大小（字节）。
    pub size_bytes: u64,
    /// 创建时间（RFC 3339 字符串）。
    pub created_at: String,
}

/// 校验备份文件名合法性（防止路径穿越与注入）。
pub fn is_valid_backup_name(name: &str) -> bool {
    if name.len() != "refract-YYYYmmdd-HHMMSS.db".len() {
        return false;
    }
    if !name.starts_with("refract-") || !name.ends_with(".db") {
        return false;
    }
    let middle = &name["refract-".len()..name.len() - ".db".len()];
    let parts: Vec<&str> = middle.split('-').collect();
    if parts.len() != 2 {
        return false;
    }
    parts[0].len() == 8
        && parts[0].chars().all(|c| c.is_ascii_digit())
        && parts[1].len() == 6
        && parts[1].chars().all(|c| c.is_ascii_digit())
}

/// 解析备份目标目录。
pub fn resolve_backup_dir(state: &AppState) -> PathBuf {
    if let Some(dir) = state
        .backup_settings()
        .directory
        .as_deref()
        .filter(|d| !d.trim().is_empty())
    {
        return PathBuf::from(dir);
    }
    PathBuf::from("backups")
}

/// 列出备份目录下的所有合法备份文件。
pub fn list_backups(dir: &Path) -> Vec<BackupInfo> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if !is_valid_backup_name(&name) {
            continue;
        }
        let meta = entry.metadata().ok();
        let size_bytes = meta.as_ref().map(|m| m.len()).unwrap_or(0);
        let created_at = meta
            .and_then(|m| m.created().or_else(|_| m.modified()).ok())
            .map(|t| {
                let dt: chrono::DateTime<Utc> = t.into();
                dt.to_rfc3339()
            })
            .unwrap_or_else(|| Utc::now().to_rfc3339());

        out.push(BackupInfo {
            name,
            size_bytes,
            created_at,
        });
    }
    out.sort_by(|a, b| b.name.cmp(&a.name));
    out
}

/// 执行单次备份并清理超出保留份数的旧备份。
pub async fn run_backup_once(state: &AppState) -> Result<String, StoreError> {
    let dir = resolve_backup_dir(state);
    if !dir.exists() {
        std::fs::create_dir_all(&dir)
            .map_err(|e| StoreError::Invalid(format!("failed to create backup dir: {e}")))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = std::fs::metadata(&dir) {
                let mut perms = meta.permissions();
                perms.set_mode(0o700);
                std::fs::set_permissions(&dir, perms).ok();
            }
        }
    }

    let timestamp = Utc::now().format("%Y%m%d-%H%M%S").to_string();
    let filename = format!("refract-{timestamp}.db");
    let target = dir.join(&filename);

    state.db().vacuum_into(&target).await?;

    let keep = state.backup_settings().keep;
    if keep > 0 {
        let mut backups = list_backups(&dir);
        backups.sort_by(|a, b| b.name.cmp(&a.name));
        if backups.len() > keep as usize {
            for old in &backups[keep as usize..] {
                let old_path = dir.join(&old.name);
                if let Err(e) = std::fs::remove_file(&old_path) {
                    tracing::warn!(%e, path = %old_path.display(), "failed to prune old backup");
                }
            }
        }
    }

    Ok(filename)
}

/// 后台自动备份循环。
pub async fn auto_backup_loop(state: AppState) {
    let mut last_backup = std::time::Instant::now();
    loop {
        tokio::time::sleep(Duration::from_secs(60)).await;
        let settings = state.backup_settings();
        if settings.interval_hours == 0 {
            continue;
        }
        let interval = Duration::from_secs(settings.interval_hours as u64 * 3600);
        if last_backup.elapsed() >= interval {
            tracing::info!(
                interval_hours = settings.interval_hours,
                "running scheduled database backup"
            );
            match run_backup_once(&state).await {
                Ok(filename) => {
                    tracing::info!(filename = %filename, "scheduled backup completed");
                    last_backup = std::time::Instant::now();
                }
                Err(error) => {
                    tracing::error!(%error, "scheduled backup failed");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_backup_names() {
        assert!(is_valid_backup_name("refract-20260818-120000.db"));
        assert!(!is_valid_backup_name("refract-20260818-12000.db"));
        assert!(!is_valid_backup_name("../refract-20260818-120000.db"));
        assert!(!is_valid_backup_name("refract-20260818-120000.db.bak"));
        assert!(!is_valid_backup_name("other.db"));
    }
}
