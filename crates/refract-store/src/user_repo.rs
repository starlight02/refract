//! 用户账号仓储。

use std::str::FromStr;

use chrono::{DateTime, Utc};
use sqlx::Row;

use crate::db::{Database, StoreError, is_unique_violation};
use crate::key_repo::parse_ts;

/// 用户角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UserRole {
    /// 普通用户。
    User,
    /// 管理员。
    Admin,
}

impl UserRole {
    /// 数据库与 API 中的稳定字符串。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Admin => "admin",
        }
    }
}

impl FromStr for UserRole {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "user" => Ok(Self::User),
            "admin" => Ok(Self::Admin),
            other => Err(format!("unknown user role: {other}")),
        }
    }
}

/// 用户状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UserStatus {
    /// 已注册、尚未完成邮箱验证。
    #[serde(rename = "pending_verification")]
    PendingVerification,
    /// 可用。
    Active,
    /// 已禁用。
    Disabled,
}

impl UserStatus {
    /// 数据库与 API 中的稳定字符串。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PendingVerification => "pending_verification",
            Self::Active => "active",
            Self::Disabled => "disabled",
        }
    }
}

impl FromStr for UserStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pending_verification" => Ok(Self::PendingVerification),
            "active" => Ok(Self::Active),
            "disabled" => Ok(Self::Disabled),
            other => Err(format!("unknown user status: {other}")),
        }
    }
}

/// 一个用户账号。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct User {
    /// 主键。
    pub id: i64,
    /// 登录邮箱（大小写不敏感唯一）。
    pub email: String,
    /// Argon2id 密码哈希。
    pub password_hash: String,
    /// 展示名。
    pub display_name: String,
    /// 角色。
    pub role: UserRole,
    /// 状态。
    pub status: UserStatus,
    /// 邮箱验证时间。
    pub email_verified_at: Option<DateTime<Utc>>,
    /// 此时间之前签发的 session 一律作废。
    pub session_revoked_at: Option<DateTime<Utc>>,
    /// 创建时间。
    pub created_at: DateTime<Utc>,
    /// 更新时间。
    pub updated_at: DateTime<Utc>,
}

/// 新建用户的参数。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct NewUser {
    /// 登录邮箱。
    pub email: String,
    /// 密码哈希。
    pub password_hash: String,
    /// 展示名。
    pub display_name: String,
    /// 角色。
    pub role: UserRole,
}

/// 用户仓储。
#[derive(Debug, Clone)]
pub struct UserRepo {
    db: Database,
}

macro_rules! user_cols {
    () => {
        "id, email, password_hash, display_name, role, status, \
         email_verified_at, session_revoked_at, created_at, updated_at"
    };
}

impl UserRepo {
    /// 绑定到一个数据库。
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// 创建用户。email 大小写不敏感冲突时返回 [`StoreError::Conflict`]。
    pub async fn create(
        &self,
        email: &str,
        password_hash: &str,
        display_name: &str,
        role: UserRole,
    ) -> Result<User, StoreError> {
        let result = sqlx::query(
            "INSERT INTO users (email, password_hash, display_name, role) \
             VALUES (?, ?, ?, ?) RETURNING id",
        )
        .bind(email)
        .bind(password_hash)
        .bind(display_name)
        .bind(role.as_str())
        .fetch_one(self.db.pool())
        .await;

        let id: i64 = match result {
            Ok(row) => row.get(0),
            Err(err) if is_unique_violation(&err) => {
                return Err(StoreError::conflict("user", email));
            }
            Err(err) => return Err(err.into()),
        };
        self.find_by_id(id)
            .await?
            .ok_or_else(|| StoreError::not_found("user", id))
    }

    /// 按邮箱查找（大小写不敏感）。
    pub async fn find_by_email(&self, email: &str) -> Result<Option<User>, StoreError> {
        let row = sqlx::query(concat!(
            "SELECT ",
            user_cols!(),
            " FROM users WHERE email_normalized = lower(?)"
        ))
        .bind(email)
        .fetch_optional(self.db.pool())
        .await?;
        row.map(|r| Self::from_row(&r)).transpose()
    }

    /// 按 ID 查找。
    pub async fn find_by_id(&self, id: i64) -> Result<Option<User>, StoreError> {
        let row = sqlx::query(concat!("SELECT ", user_cols!(), " FROM users WHERE id = ?"))
            .bind(id)
            .fetch_optional(self.db.pool())
            .await?;
        row.map(|r| Self::from_row(&r)).transpose()
    }

    /// 设置状态。
    pub async fn set_status(&self, id: i64, status: UserStatus) -> Result<(), StoreError> {
        let affected =
            sqlx::query("UPDATE users SET status = ?, updated_at = datetime('now') WHERE id = ?")
                .bind(status.as_str())
                .bind(id)
                .execute(self.db.pool())
                .await?
                .rows_affected();
        crate::ensure_affected(affected, "user", id)
    }

    /// 标记邮箱已验证：写入 `email_verified_at`，若仍是待验证则转为 active。
    pub async fn mark_email_verified(&self, id: i64) -> Result<(), StoreError> {
        let affected = sqlx::query(
            "UPDATE users SET \
             email_verified_at = datetime('now'), \
             status = CASE WHEN status = 'pending_verification' THEN 'active' ELSE status END, \
             updated_at = datetime('now') \
             WHERE id = ?",
        )
        .bind(id)
        .execute(self.db.pool())
        .await?
        .rows_affected();
        crate::ensure_affected(affected, "user", id)
    }

    /// 更新密码哈希，并立刻作废该用户全部 session。
    pub async fn set_password_hash(&self, id: i64, hash: &str) -> Result<(), StoreError> {
        let affected = sqlx::query(
            "UPDATE users SET password_hash = ?, session_revoked_at = datetime('now'), \
             updated_at = datetime('now') WHERE id = ?",
        )
        .bind(hash)
        .bind(id)
        .execute(self.db.pool())
        .await?
        .rows_affected();
        crate::ensure_affected(affected, "user", id)
    }

    /// 作废该用户全部 session。
    pub async fn revoke_sessions(&self, id: i64) -> Result<(), StoreError> {
        let affected = sqlx::query(
            "UPDATE users SET session_revoked_at = datetime('now'), \
             updated_at = datetime('now') WHERE id = ?",
        )
        .bind(id)
        .execute(self.db.pool())
        .await?
        .rows_affected();
        crate::ensure_affected(affected, "user", id)
    }

    /// 更新展示名。
    pub async fn set_display_name(&self, id: i64, display_name: &str) -> Result<(), StoreError> {
        let affected = sqlx::query(
            "UPDATE users SET display_name = ?, updated_at = datetime('now') WHERE id = ?",
        )
        .bind(display_name)
        .bind(id)
        .execute(self.db.pool())
        .await?
        .rows_affected();
        crate::ensure_affected(affected, "user", id)
    }

    /// 设置角色。
    pub async fn set_role(&self, id: i64, role: UserRole) -> Result<(), StoreError> {
        let affected =
            sqlx::query("UPDATE users SET role = ?, updated_at = datetime('now') WHERE id = ?")
                .bind(role.as_str())
                .bind(id)
                .execute(self.db.pool())
                .await?
                .rows_affected();
        crate::ensure_affected(affected, "user", id)
    }

    /// 列出用户，可按状态与邮箱子串过滤，按 id 升序分页。
    pub async fn list_filtered(
        &self,
        status: Option<UserStatus>,
        email: Option<&str>,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<User>, StoreError> {
        let status_s = status.map(UserStatus::as_str);
        let email_like = email
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| format!("%{}%", s.to_ascii_lowercase()));
        let rows = sqlx::query(concat!(
            "SELECT ",
            user_cols!(),
            " FROM users \
             WHERE (? IS NULL OR status = ?) \
               AND (? IS NULL OR email_normalized LIKE ?) \
             ORDER BY id ASC LIMIT ? OFFSET ?"
        ))
        .bind(status_s)
        .bind(status_s)
        .bind(email_like.as_deref())
        .bind(email_like.as_deref())
        .bind(i64::from(limit.clamp(1, 200)))
        .bind(i64::from(offset))
        .fetch_all(self.db.pool())
        .await?;
        rows.iter().map(Self::from_row).collect()
    }

    /// 列出全部用户，按 id 升序。
    pub async fn list(&self) -> Result<Vec<User>, StoreError> {
        self.list_filtered(None, None, 200, 0).await
    }

    /// 用户总数。
    pub async fn count(&self) -> Result<i64, StoreError> {
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
            .fetch_one(self.db.pool())
            .await?;
        Ok(n)
    }

    /// 管理员人数。
    pub async fn count_admins(&self) -> Result<i64, StoreError> {
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE role = 'admin'")
            .fetch_one(self.db.pool())
            .await?;
        Ok(n)
    }

    /// 库中尚无用户时创建首位管理员并完成邮箱验证；已有用户则返回 `None`。
    pub async fn create_first_admin_if_empty(
        &self,
        email: &str,
        password_hash: &str,
    ) -> Result<Option<User>, StoreError> {
        if self.count().await? > 0 {
            return Ok(None);
        }
        match self
            .create(email, password_hash, "Admin", UserRole::Admin)
            .await
        {
            Ok(user) => {
                self.mark_email_verified(user.id).await?;
                self.find_by_id(user.id).await
            }
            Err(StoreError::Conflict { .. }) => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// 把用户提升为 admin（幂等）。
    pub async fn set_role_admin(&self, id: i64) -> Result<(), StoreError> {
        self.set_role(id, UserRole::Admin).await
    }

    /// 升级历史库：把全部旧行（owner_id=1, user_id IS NULL）归属到 bootstrap admin。
    ///
    /// - `channels.user_id`：仅回填 shared 渠；private 渠要求显式属主，不猜。
    /// - `api_keys.user_id`：全部归 admin（旧库只有一个用户）。
    /// - `request_logs.user_id`：`owner_id=1` 且未回填的行归 admin。
    pub async fn backfill_owner_columns(&self, admin_id: i64) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE channels SET user_id = ? WHERE user_id IS NULL AND visibility = 'shared'",
        )
        .bind(admin_id)
        .execute(self.db.pool())
        .await?;
        sqlx::query("UPDATE api_keys SET user_id = ? WHERE user_id IS NULL")
            .bind(admin_id)
            .execute(self.db.pool())
            .await?;
        sqlx::query("UPDATE request_logs SET user_id = ? WHERE owner_id = 1 AND user_id IS NULL")
            .bind(admin_id)
            .execute(self.db.pool())
            .await?;
        Ok(())
    }

    fn from_row(row: &sqlx::sqlite::SqliteRow) -> Result<User, StoreError> {
        let role: String = row.get("role");
        let status: String = row.get("status");
        Ok(User {
            id: row.get("id"),
            email: row.get("email"),
            password_hash: row.get("password_hash"),
            display_name: row.get("display_name"),
            role: role.parse().map_err(StoreError::Invalid)?,
            status: status.parse().map_err(StoreError::Invalid)?,
            email_verified_at: parse_ts(row.get::<Option<String>, _>("email_verified_at")),
            session_revoked_at: parse_ts(row.get::<Option<String>, _>("session_revoked_at")),
            created_at: parse_ts(row.get::<Option<String>, _>("created_at"))
                .unwrap_or_else(Utc::now),
            updated_at: parse_ts(row.get::<Option<String>, _>("updated_at"))
                .unwrap_or_else(Utc::now),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn repo() -> UserRepo {
        UserRepo::new(Database::open_in_memory().await.unwrap())
    }

    #[tokio::test]
    async fn email_nocase_conflict_returns_conflict() {
        let repo = repo().await;
        repo.create("Admin@example.com", "hash-a", "A", UserRole::User)
            .await
            .unwrap();
        let err = repo
            .create("admin@example.com", "hash-b", "B", UserRole::Admin)
            .await
            .unwrap_err();
        assert!(
            matches!(err, StoreError::Conflict { entity, ref id } if entity == "user" && id.eq_ignore_ascii_case("admin@example.com")),
            "{err:?}"
        );
        assert_eq!(repo.count().await.unwrap(), 1);
        let found = repo
            .find_by_email("ADMIN@example.com")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.email, "Admin@example.com");
        assert_eq!(found.role, UserRole::User);
    }

    #[tokio::test]
    async fn mark_email_verified_activates_pending_user() {
        let repo = repo().await;
        let user = repo
            .create("a@x.test", "h", "n", UserRole::User)
            .await
            .unwrap();
        assert_eq!(user.status, UserStatus::PendingVerification);
        assert!(user.email_verified_at.is_none());

        repo.mark_email_verified(user.id).await.unwrap();
        let after = repo.find_by_id(user.id).await.unwrap().unwrap();
        assert_eq!(after.status, UserStatus::Active);
        assert!(after.email_verified_at.is_some());

        repo.set_status(user.id, UserStatus::Disabled)
            .await
            .unwrap();
        repo.mark_email_verified(user.id).await.unwrap();
        let still = repo.find_by_id(user.id).await.unwrap().unwrap();
        assert_eq!(still.status, UserStatus::Disabled);
    }

    #[tokio::test]
    async fn set_password_hash_revokes_sessions() {
        let repo = repo().await;
        let user = repo
            .create("a@x.test", "old", "n", UserRole::User)
            .await
            .unwrap();
        assert!(user.session_revoked_at.is_none());
        repo.set_password_hash(user.id, "new").await.unwrap();
        let after = repo.find_by_id(user.id).await.unwrap().unwrap();
        assert_eq!(after.password_hash, "new");
        assert!(after.session_revoked_at.is_some());
    }

    #[tokio::test]
    async fn create_first_admin_if_empty_is_idempotent() {
        let repo = repo().await;
        let first = repo
            .create_first_admin_if_empty("admin@localhost", "hash")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.role, UserRole::Admin);
        assert_eq!(first.status, UserStatus::Active);
        assert!(first.email_verified_at.is_some());
        assert_eq!(repo.count_admins().await.unwrap(), 1);

        assert!(
            repo.create_first_admin_if_empty("other@localhost", "hash")
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(repo.count().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn list_is_sorted_by_id() {
        let repo = repo().await;
        repo.create("b@x.test", "h", "b", UserRole::User)
            .await
            .unwrap();
        repo.create("a@x.test", "h", "a", UserRole::Admin)
            .await
            .unwrap();
        let list = repo.list().await.unwrap();
        assert!(list[0].id < list[1].id);
        assert_eq!(list[0].email, "b@x.test");
    }

    #[test]
    fn role_and_status_roundtrip_as_str() {
        assert_eq!(
            UserRole::User.as_str().parse::<UserRole>().unwrap(),
            UserRole::User
        );
        assert_eq!(
            UserRole::Admin.as_str().parse::<UserRole>().unwrap(),
            UserRole::Admin
        );
        assert_eq!(
            UserStatus::PendingVerification.as_str(),
            "pending_verification"
        );
        assert_eq!(
            "pending_verification".parse::<UserStatus>().unwrap(),
            UserStatus::PendingVerification
        );
        assert!("nope".parse::<UserRole>().is_err());
    }
}
