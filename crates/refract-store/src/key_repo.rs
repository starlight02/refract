//! 网关自身的 API 密钥仓储。
//!
//! 密钥**只在创建时返回一次明文**，库里存的是 SHA-256 哈希。这不是过度设计：
//! 网关的数据库文件会被备份、会被同步，明文密钥泄漏的代价是上游账单。

use base64::Engine as _;
use chrono::{DateTime, Utc};
use rand::RngExt as _;
use sha2::{Digest, Sha256};
use sqlx::Row;

use crate::db::{Database, StoreError};

/// 密钥前缀，便于在日志与 UI 中一眼认出这是 Refract 的 key。
pub const KEY_PREFIX: &str = "rk-";

/// 一个 API 密钥（不含明文）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ApiKey {
    /// 主键。
    pub id: i64,
    /// 所有者。
    pub owner_id: i64,
    /// 展示名。
    pub name: String,
    /// 明文密钥的前若干位，用于 UI 辨识。
    pub key_prefix: String,
    /// 是否启用。
    pub enabled: bool,
    /// 允许访问的模型；空表示不限。
    pub allowed_models: Vec<String>,
    /// 允许访问的渠道标签；空表示不限。
    pub allowed_tags: Vec<String>,
    /// 配额上限（token 数）。0 表示不限。
    pub quota: i64,
    /// 已用配额。
    pub used_quota: i64,
    /// 过期时间。
    pub expires_at: Option<DateTime<Utc>>,
    /// 最后使用时间。
    pub last_used_at: Option<DateTime<Utc>>,
    /// 创建时间。
    pub created_at: DateTime<Utc>,
}

impl ApiKey {
    /// 该密钥当前是否可用。
    pub fn is_usable(&self, now: DateTime<Utc>) -> bool {
        if !self.enabled {
            return false;
        }
        if let Some(exp) = self.expires_at
            && exp <= now
        {
            return false;
        }
        if self.quota > 0 && self.used_quota >= self.quota {
            return false;
        }
        true
    }

    /// 该密钥是否被允许访问某个模型。
    pub fn allows_model(&self, model: &str) -> bool {
        self.allowed_models.is_empty() || self.allowed_models.iter().any(|m| m == model)
    }
}

/// 备份导出形态的 API 密钥：含 `key_hash`，可在另一实例恢复后继续用原密钥。
///
/// 明文不可导出 —— 库里从未存过。哈希本身不可逆，泄漏面等同数据库文件。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ExportedApiKey {
    /// 展示名。
    pub name: String,
    /// 明文密钥的 SHA-256 哈希。
    pub key_hash: String,
    /// 明文前缀（UI 辨识用）。
    pub key_prefix: String,
    /// 是否启用。
    pub enabled: bool,
    /// 模型白名单。
    #[serde(default)]
    pub allowed_models: Vec<String>,
    /// 渠道标签白名单。
    #[serde(default)]
    pub allowed_tags: Vec<String>,
    /// 配额上限。
    #[serde(default)]
    pub quota: i64,
    /// 已用配额。
    #[serde(default)]
    pub used_quota: i64,
    /// 过期时间。
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
}

/// 新建密钥的参数。
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct NewApiKey {
    /// 展示名。
    pub name: String,
    /// 允许访问的模型。
    #[serde(default)]
    pub allowed_models: Vec<String>,
    /// 允许访问的渠道标签。
    #[serde(default)]
    pub allowed_tags: Vec<String>,
    /// 配额上限。
    #[serde(default)]
    pub quota: i64,
    /// 过期时间。
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
}

/// API 密钥仓储。
#[derive(Debug, Clone)]
pub struct ApiKeyRepo {
    db: Database,
}

impl ApiKeyRepo {
    /// 绑定到一个数据库。
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// 计算密钥哈希。
    ///
    /// 用裸 SHA-256 而非 argon2/bcrypt：API key 是 256 位高熵随机串，不是人选的
    /// 口令，不存在字典攻击面；而鉴权在每个请求的热路径上，慢哈希会直接变成延迟。
    pub fn hash(plaintext: &str) -> String {
        let digest = Sha256::digest(plaintext.as_bytes());
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
    }

    /// 生成一个新的明文密钥。
    pub fn generate_plaintext() -> String {
        let mut bytes = [0_u8; 32];
        rand::rng().fill(&mut bytes);
        format!(
            "{KEY_PREFIX}{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
        )
    }

    /// 创建密钥，返回 `(记录, 明文)`。明文此后无法再取回。
    pub async fn create(
        &self,
        owner_id: i64,
        spec: NewApiKey,
    ) -> Result<(ApiKey, String), StoreError> {
        if spec.name.trim().is_empty() {
            return Err(StoreError::Invalid("api key name must not be empty".into()));
        }
        let plaintext = Self::generate_plaintext();
        let hash = Self::hash(&plaintext);
        // 前缀取到第 12 个字符，足以辨识且不足以暴力还原。
        let prefix: String = plaintext.chars().take(12).collect();

        let id: i64 = sqlx::query(
            "INSERT INTO api_keys \
             (owner_id, name, key_hash, key_prefix, allowed_models, allowed_tags, quota, expires_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?) RETURNING id",
        )
        .bind(owner_id)
        .bind(spec.name.trim())
        .bind(&hash)
        .bind(&prefix)
        .bind(serde_json::to_string(&spec.allowed_models).expect("models serialize"))
        .bind(serde_json::to_string(&spec.allowed_tags).expect("tags serialize"))
        .bind(spec.quota)
        .bind(spec.expires_at.map(|d| d.to_rfc3339()))
        .fetch_one(self.db.pool())
        .await?
        .get(0);

        let key = self.get(owner_id, id).await?;
        Ok((key, plaintext))
    }

    /// 按明文查密钥。鉴权热路径。
    pub async fn find_by_plaintext(&self, plaintext: &str) -> Result<Option<ApiKey>, StoreError> {
        let hash = Self::hash(plaintext);
        let row = sqlx::query(
            "SELECT id, owner_id, name, key_prefix, enabled, allowed_models, allowed_tags, \
             quota, used_quota, expires_at, last_used_at, created_at \
             FROM api_keys WHERE key_hash = ?",
        )
        .bind(hash)
        .fetch_optional(self.db.pool())
        .await?;
        row.map(|r| Self::from_row(&r)).transpose()
    }

    /// 按 ID 取。
    pub async fn get(&self, owner_id: i64, id: i64) -> Result<ApiKey, StoreError> {
        let row = sqlx::query(
            "SELECT id, owner_id, name, key_prefix, enabled, allowed_models, allowed_tags, \
             quota, used_quota, expires_at, last_used_at, created_at \
             FROM api_keys WHERE owner_id = ? AND id = ?",
        )
        .bind(owner_id)
        .bind(id)
        .fetch_optional(self.db.pool())
        .await?
        .ok_or_else(|| StoreError::not_found("api key", id))?;
        Self::from_row(&row)
    }

    /// 列出全部。
    pub async fn list(&self, owner_id: i64) -> Result<Vec<ApiKey>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, owner_id, name, key_prefix, enabled, allowed_models, allowed_tags, \
             quota, used_quota, expires_at, last_used_at, created_at \
             FROM api_keys WHERE owner_id = ? ORDER BY id DESC",
        )
        .bind(owner_id)
        .fetch_all(self.db.pool())
        .await?;
        rows.iter().map(Self::from_row).collect()
    }

    /// 启用/停用。
    pub async fn set_enabled(
        &self,
        owner_id: i64,
        id: i64,
        enabled: bool,
    ) -> Result<(), StoreError> {
        let affected = sqlx::query("UPDATE api_keys SET enabled = ? WHERE id = ? AND owner_id = ?")
            .bind(enabled)
            .bind(id)
            .bind(owner_id)
            .execute(self.db.pool())
            .await?
            .rows_affected();
        if affected == 0 {
            return Err(StoreError::not_found("api key", id));
        }
        Ok(())
    }

    /// 删除。
    pub async fn delete(&self, owner_id: i64, id: i64) -> Result<(), StoreError> {
        let affected = sqlx::query("DELETE FROM api_keys WHERE id = ? AND owner_id = ?")
            .bind(id)
            .bind(owner_id)
            .execute(self.db.pool())
            .await?
            .rows_affected();
        if affected == 0 {
            return Err(StoreError::not_found("api key", id));
        }
        Ok(())
    }

    /// 导出全部密钥（含哈希），用于备份。
    pub async fn export(&self, owner_id: i64) -> Result<Vec<ExportedApiKey>, StoreError> {
        let rows = sqlx::query(
            "SELECT name, key_hash, key_prefix, enabled, allowed_models, allowed_tags, \
             quota, used_quota, expires_at \
             FROM api_keys WHERE owner_id = ? ORDER BY id",
        )
        .bind(owner_id)
        .fetch_all(self.db.pool())
        .await?;
        rows.iter()
            .map(|row| {
                let allowed_models: String = row.get("allowed_models");
                let allowed_tags: String = row.get("allowed_tags");
                Ok(ExportedApiKey {
                    name: row.get("name"),
                    key_hash: row.get("key_hash"),
                    key_prefix: row.get("key_prefix"),
                    enabled: row.get("enabled"),
                    allowed_models: serde_json::from_str(&allowed_models)
                        .map_err(StoreError::json("api_keys.allowed_models"))?,
                    allowed_tags: serde_json::from_str(&allowed_tags)
                        .map_err(StoreError::json("api_keys.allowed_tags"))?,
                    quota: row.get("quota"),
                    used_quota: row.get("used_quota"),
                    expires_at: parse_ts(row.get::<Option<String>, _>("expires_at")),
                })
            })
            .collect()
    }

    /// 从备份恢复一把密钥。返回是否实际插入（`key_hash` 已存在时跳过）。
    pub async fn restore(&self, owner_id: i64, key: &ExportedApiKey) -> Result<bool, StoreError> {
        let mut conn = self.db.pool().acquire().await?;
        Self::insert_restored(&mut conn, owner_id, key).await
    }

    /// 原子替换该所有者的全部密钥（导入的 replace 模式用）。
    ///
    /// 返回 `(imported, 跳过的密钥名)`。删旧与插新同一事务：任何一把密钥
    /// 无效则整体回滚，不会出现「密钥被清空但只恢复了一半」的中间态。
    pub async fn replace_all(
        &self,
        owner_id: i64,
        keys: &[ExportedApiKey],
    ) -> Result<(u32, Vec<String>), StoreError> {
        let mut tx = self.db.pool().begin().await?;
        sqlx::query("DELETE FROM api_keys WHERE owner_id = ?")
            .bind(owner_id)
            .execute(&mut *tx)
            .await?;
        let mut imported = 0_u32;
        let mut skipped = Vec::new();
        for key in keys {
            if Self::insert_restored(&mut tx, owner_id, key).await? {
                imported += 1;
            } else {
                skipped.push(key.name.clone());
            }
        }
        tx.commit().await?;
        Ok((imported, skipped))
    }

    async fn insert_restored(
        conn: &mut sqlx::SqliteConnection,
        owner_id: i64,
        key: &ExportedApiKey,
    ) -> Result<bool, StoreError> {
        if key.name.trim().is_empty() || key.key_hash.trim().is_empty() {
            return Err(StoreError::Invalid(
                "restored api key needs both a name and a key_hash".into(),
            ));
        }
        let affected = sqlx::query(
            "INSERT INTO api_keys \
             (owner_id, name, key_hash, key_prefix, enabled, allowed_models, allowed_tags, \
              quota, used_quota, expires_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT (key_hash) DO NOTHING",
        )
        .bind(owner_id)
        .bind(key.name.trim())
        .bind(&key.key_hash)
        .bind(&key.key_prefix)
        .bind(key.enabled)
        .bind(serde_json::to_string(&key.allowed_models).expect("models serialize"))
        .bind(serde_json::to_string(&key.allowed_tags).expect("tags serialize"))
        .bind(key.quota)
        .bind(key.used_quota)
        .bind(key.expires_at.map(|d| d.to_rfc3339()))
        .execute(&mut *conn)
        .await?
        .rows_affected();
        Ok(affected > 0)
    }

    /// 删除该所有者的全部密钥（导入的 replace 模式用）。
    pub async fn delete_all(&self, owner_id: i64) -> Result<u64, StoreError> {
        let affected = sqlx::query("DELETE FROM api_keys WHERE owner_id = ?")
            .bind(owner_id)
            .execute(self.db.pool())
            .await?
            .rows_affected();
        Ok(affected)
    }

    /// 记录一次使用：累加配额并更新最后使用时间。
    pub async fn record_usage(&self, id: i64, tokens: i64) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE api_keys SET used_quota = used_quota + ?, last_used_at = datetime('now') \
             WHERE id = ?",
        )
        .bind(tokens)
        .bind(id)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    fn from_row(row: &sqlx::sqlite::SqliteRow) -> Result<ApiKey, StoreError> {
        let allowed_models: String = row.get("allowed_models");
        let allowed_tags: String = row.get("allowed_tags");
        Ok(ApiKey {
            id: row.get("id"),
            owner_id: row.get("owner_id"),
            name: row.get("name"),
            key_prefix: row.get("key_prefix"),
            enabled: row.get("enabled"),
            allowed_models: serde_json::from_str(&allowed_models)
                .map_err(StoreError::json("api_keys.allowed_models"))?,
            allowed_tags: serde_json::from_str(&allowed_tags)
                .map_err(StoreError::json("api_keys.allowed_tags"))?,
            quota: row.get("quota"),
            used_quota: row.get("used_quota"),
            expires_at: parse_ts(row.get::<Option<String>, _>("expires_at")),
            last_used_at: parse_ts(row.get::<Option<String>, _>("last_used_at")),
            created_at: parse_ts(row.get::<Option<String>, _>("created_at"))
                .unwrap_or_else(Utc::now),
        })
    }
}

/// 解析 SQLite 存的时间戳。
///
/// SQLite 的 `datetime('now')` 产出 `YYYY-MM-DD HH:MM:SS`（UTC，无时区标记），
/// 而我们自己写入的是 RFC3339。两种都要能读。
pub(crate) fn parse_ts(raw: Option<String>) -> Option<DateTime<Utc>> {
    let raw = raw?;
    if let Ok(dt) = DateTime::parse_from_rfc3339(&raw) {
        return Some(dt.with_timezone(&Utc));
    }
    chrono::NaiveDateTime::parse_from_str(&raw, "%Y-%m-%d %H:%M:%S")
        .ok()
        .map(|naive| naive.and_utc())
}

#[cfg(test)]
mod tests {
    use super::*;
    use refract_core::DEFAULT_OWNER_ID;

    async fn repo() -> ApiKeyRepo {
        ApiKeyRepo::new(Database::open_in_memory().await.unwrap())
    }

    #[tokio::test]
    async fn export_then_restore_keeps_the_original_plaintext_working() {
        let source = repo().await;
        let (_, plaintext) = source
            .create(
                DEFAULT_OWNER_ID,
                NewApiKey {
                    name: "laptop".into(),
                    allowed_models: vec!["gpt-4o".into()],
                    quota: 1000,
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let exported = source.export(DEFAULT_OWNER_ID).await.unwrap();
        assert_eq!(exported.len(), 1);
        assert_eq!(exported[0].name, "laptop");
        assert!(!exported[0].key_hash.is_empty());

        // 在一个全新实例上恢复：原明文必须还能通过鉴权。
        let target = repo().await;
        assert!(
            target
                .restore(DEFAULT_OWNER_ID, &exported[0])
                .await
                .unwrap()
        );
        let found = target.find_by_plaintext(&plaintext).await.unwrap().unwrap();
        assert_eq!(found.name, "laptop");
        assert_eq!(found.allowed_models, vec!["gpt-4o".to_owned()]);
        assert_eq!(found.quota, 1000);

        // 重复恢复按 key_hash 去重，返回未插入。
        assert!(
            !target
                .restore(DEFAULT_OWNER_ID, &exported[0])
                .await
                .unwrap()
        );
        assert_eq!(target.list(DEFAULT_OWNER_ID).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn replace_all_swaps_keys_atomically() {
        let repo = repo().await;
        repo.create(
            DEFAULT_OWNER_ID,
            NewApiKey {
                name: "stale".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let incoming = vec![
            ExportedApiKey {
                name: "restored".into(),
                key_hash: "hash-1".into(),
                key_prefix: "sk-r1".into(),
                enabled: true,
                allowed_models: vec![],
                allowed_tags: vec![],
                quota: 0,
                used_quota: 0,
                expires_at: None,
            },
            // 与第一把 key_hash 相同：应被去重跳过，而不是报错。
            ExportedApiKey {
                name: "dup".into(),
                key_hash: "hash-1".into(),
                key_prefix: "sk-r2".into(),
                enabled: true,
                allowed_models: vec![],
                allowed_tags: vec![],
                quota: 0,
                used_quota: 0,
                expires_at: None,
            },
        ];
        let (imported, skipped) = repo.replace_all(DEFAULT_OWNER_ID, &incoming).await.unwrap();
        assert_eq!(imported, 1);
        assert_eq!(skipped, vec!["dup".to_owned()]);

        let all = repo.list(DEFAULT_OWNER_ID).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "restored");
    }

    #[tokio::test]
    async fn replace_all_rolls_back_on_invalid_key() {
        let repo = repo().await;
        repo.create(
            DEFAULT_OWNER_ID,
            NewApiKey {
                name: "keep-me".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let bad = ExportedApiKey {
            name: "".into(), // 无效：名字为空。
            key_hash: "h".into(),
            key_prefix: "p".into(),
            enabled: true,
            allowed_models: vec![],
            allowed_tags: vec![],
            quota: 0,
            used_quota: 0,
            expires_at: None,
        };
        let err = repo
            .replace_all(DEFAULT_OWNER_ID, &[bad])
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Invalid(_)), "{err:?}");

        // 回滚后旧密钥仍在。
        let all = repo.list(DEFAULT_OWNER_ID).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "keep-me");
    }

    #[tokio::test]
    async fn delete_all_clears_only_that_owner() {
        let repo = repo().await;
        repo.create(
            DEFAULT_OWNER_ID,
            NewApiKey {
                name: "a".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        repo.create(
            DEFAULT_OWNER_ID,
            NewApiKey {
                name: "b".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert_eq!(repo.delete_all(DEFAULT_OWNER_ID).await.unwrap(), 2);
        assert!(repo.list(DEFAULT_OWNER_ID).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn created_key_returns_plaintext_once_and_stores_only_hash() {
        let repo = repo().await;
        let (key, plaintext) = repo
            .create(
                DEFAULT_OWNER_ID,
                NewApiKey {
                    name: "laptop".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        assert!(plaintext.starts_with(KEY_PREFIX));
        assert!(key.key_prefix.len() <= 12);
        assert!(plaintext.starts_with(&key.key_prefix));

        // 库里不能有明文。
        let stored: (String,) = sqlx::query_as("SELECT key_hash FROM api_keys WHERE id = ?")
            .bind(key.id)
            .fetch_one(repo.db.pool())
            .await
            .unwrap();
        assert_ne!(stored.0, plaintext);
        assert_eq!(stored.0, ApiKeyRepo::hash(&plaintext));
    }

    #[tokio::test]
    async fn lookup_by_plaintext_finds_the_key() {
        let repo = repo().await;
        let (created, plaintext) = repo
            .create(
                DEFAULT_OWNER_ID,
                NewApiKey {
                    name: "cli".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let found = repo.find_by_plaintext(&plaintext).await.unwrap();
        assert_eq!(found.map(|k| k.id), Some(created.id));

        assert!(repo.find_by_plaintext("rk-wrong").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn generated_keys_are_unique() {
        let a = ApiKeyRepo::generate_plaintext();
        let b = ApiKeyRepo::generate_plaintext();
        assert_ne!(a, b);
        // 32 字节 base64url 无填充 = 43 字符，加前缀。
        assert_eq!(a.len(), KEY_PREFIX.len() + 43);
    }

    #[tokio::test]
    async fn empty_name_is_rejected() {
        let repo = repo().await;
        let err = repo
            .create(
                DEFAULT_OWNER_ID,
                NewApiKey {
                    name: "  ".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Invalid(_)));
    }

    #[test]
    fn usability_checks_enabled_expiry_and_quota() {
        let now = Utc::now();
        let base = ApiKey {
            id: 1,
            owner_id: 1,
            name: "k".into(),
            key_prefix: "rk-abc".into(),
            enabled: true,
            allowed_models: vec![],
            allowed_tags: vec![],
            quota: 0,
            used_quota: 0,
            expires_at: None,
            last_used_at: None,
            created_at: now,
        };
        assert!(base.is_usable(now));

        let disabled = ApiKey {
            enabled: false,
            ..base.clone()
        };
        assert!(!disabled.is_usable(now));

        let expired = ApiKey {
            expires_at: Some(now - chrono::Duration::hours(1)),
            ..base.clone()
        };
        assert!(!expired.is_usable(now));

        let future = ApiKey {
            expires_at: Some(now + chrono::Duration::hours(1)),
            ..base.clone()
        };
        assert!(future.is_usable(now));

        let exhausted = ApiKey {
            quota: 100,
            used_quota: 100,
            ..base.clone()
        };
        assert!(!exhausted.is_usable(now));

        let within = ApiKey {
            quota: 100,
            used_quota: 99,
            ..base
        };
        assert!(within.is_usable(now));
    }

    #[test]
    fn model_allowlist_empty_means_unrestricted() {
        let key = ApiKey {
            id: 1,
            owner_id: 1,
            name: "k".into(),
            key_prefix: String::new(),
            enabled: true,
            allowed_models: vec![],
            allowed_tags: vec![],
            quota: 0,
            used_quota: 0,
            expires_at: None,
            last_used_at: None,
            created_at: Utc::now(),
        };
        assert!(key.allows_model("anything"));

        let restricted = ApiKey {
            allowed_models: vec!["gpt-4o".into()],
            ..key
        };
        assert!(restricted.allows_model("gpt-4o"));
        assert!(!restricted.allows_model("claude-sonnet-4-6"));
    }

    #[tokio::test]
    async fn usage_accumulates_and_stamps_last_used() {
        let repo = repo().await;
        let (key, _) = repo
            .create(
                DEFAULT_OWNER_ID,
                NewApiKey {
                    name: "k".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(key.last_used_at.is_none());

        repo.record_usage(key.id, 120).await.unwrap();
        repo.record_usage(key.id, 30).await.unwrap();

        let after = repo.get(DEFAULT_OWNER_ID, key.id).await.unwrap();
        assert_eq!(after.used_quota, 150);
        assert!(after.last_used_at.is_some());
    }

    #[tokio::test]
    async fn allowlists_survive_roundtrip() {
        let repo = repo().await;
        let (key, _) = repo
            .create(
                DEFAULT_OWNER_ID,
                NewApiKey {
                    name: "scoped".into(),
                    allowed_models: vec!["gpt-4o".into(), "claude-sonnet-4-6".into()],
                    allowed_tags: vec!["prod".into()],
                    quota: 1_000,
                    expires_at: None,
                },
            )
            .await
            .unwrap();
        let fetched = repo.get(DEFAULT_OWNER_ID, key.id).await.unwrap();
        assert_eq!(fetched.allowed_models.len(), 2);
        assert_eq!(fetched.allowed_tags, vec!["prod".to_string()]);
        assert_eq!(fetched.quota, 1_000);
    }

    #[tokio::test]
    async fn disable_and_delete_work() {
        let repo = repo().await;
        let (key, _) = repo
            .create(
                DEFAULT_OWNER_ID,
                NewApiKey {
                    name: "k".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        repo.set_enabled(DEFAULT_OWNER_ID, key.id, false)
            .await
            .unwrap();
        assert!(!repo.get(DEFAULT_OWNER_ID, key.id).await.unwrap().enabled);

        repo.delete(DEFAULT_OWNER_ID, key.id).await.unwrap();
        assert!(repo.get(DEFAULT_OWNER_ID, key.id).await.is_err());
    }

    #[tokio::test]
    async fn owner_scoping_applies_to_keys() {
        let repo = repo().await;
        repo.create(
            2,
            NewApiKey {
                name: "other".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(repo.list(DEFAULT_OWNER_ID).await.unwrap().is_empty());
        assert_eq!(repo.list(2).await.unwrap().len(), 1);
    }

    #[test]
    fn timestamp_parser_accepts_both_sqlite_and_rfc3339() {
        assert!(parse_ts(Some("2026-08-10 12:34:56".into())).is_some());
        assert!(parse_ts(Some("2026-08-10T12:34:56+00:00".into())).is_some());
        assert!(parse_ts(None).is_none());
        assert!(parse_ts(Some("garbage".into())).is_none());
    }
}
