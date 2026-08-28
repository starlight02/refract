//! 邮箱验证 / 密码重置码仓储。

use std::str::FromStr;

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use sqlx::Row;

use crate::db::{Database, StoreError};
use crate::key_repo::parse_ts;

/// 验证码用途。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodePurpose {
    /// 注册邮箱验证。
    VerifyEmail,
    /// 密码重置。
    ResetPassword,
}

impl CodePurpose {
    /// 数据库与 API 中的稳定字符串。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::VerifyEmail => "verify_email",
            Self::ResetPassword => "reset_password",
        }
    }
}

impl FromStr for CodePurpose {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "verify_email" => Ok(Self::VerifyEmail),
            "reset_password" => Ok(Self::ResetPassword),
            other => Err(format!("unknown code purpose: {other}")),
        }
    }
}

/// 校验结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeVerifyOutcome {
    /// 校验通过，码已消费。
    Ok,
    /// 码不存在或哈希不匹配。
    Invalid,
    /// 已过期。
    Expired,
    /// 尝试次数用尽。
    Locked,
}

/// 一条验证码记录。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct VerificationCode {
    /// 主键。
    pub id: i64,
    /// 所属用户。
    pub user_id: i64,
    /// 用途。
    pub purpose: CodePurpose,
    /// `SHA-256("{user_id}:{code}")` 的 hex。
    pub code_hash: String,
    /// 已尝试次数。
    pub attempts: i64,
    /// 过期时间。
    pub expires_at: DateTime<Utc>,
    /// 消费时间。
    pub consumed_at: Option<DateTime<Utc>>,
    /// 创建时间。
    pub created_at: DateTime<Utc>,
}

/// 验证码仓储。
#[derive(Debug, Clone)]
pub struct VerificationRepo {
    db: Database,
}

impl VerificationRepo {
    /// 绑定到一个数据库。
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// `SHA-256("{user_id}:{code}")` 的小写 hex。
    pub fn hash_code(user_id: i64, code: &str) -> String {
        let digest = Sha256::digest(format!("{user_id}:{code}").as_bytes());
        digest.iter().fold(String::with_capacity(64), |mut acc, b| {
            acc.push_str(&format!("{b:02x}"));
            acc
        })
    }

    /// 作废该 (user, purpose) 下全部未消费码，再插入新码。默认 TTL 由调用方传入（计划为 10 分钟）。
    pub async fn issue(
        &self,
        user_id: i64,
        purpose: CodePurpose,
        code: &str,
        ttl_minutes: i64,
    ) -> Result<(), StoreError> {
        let expires_at = Utc::now() + chrono::Duration::minutes(ttl_minutes);
        let hash = Self::hash_code(user_id, code);
        let mut tx = self.db.pool().begin().await?;
        sqlx::query(
            "UPDATE verification_codes SET consumed_at = datetime('now') \
             WHERE user_id = ? AND purpose = ? AND consumed_at IS NULL",
        )
        .bind(user_id)
        .bind(purpose.as_str())
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO verification_codes (user_id, purpose, code_hash, expires_at) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(user_id)
        .bind(purpose.as_str())
        .bind(&hash)
        .bind(expires_at.to_rfc3339())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// 该 (user, purpose) 最新一条（含已消费）的 `created_at`，用于重发冷却。
    pub async fn latest_created_at(
        &self,
        user_id: i64,
        purpose: CodePurpose,
    ) -> Result<Option<DateTime<Utc>>, StoreError> {
        let raw: Option<String> = sqlx::query_scalar(
            "SELECT created_at FROM verification_codes \
             WHERE user_id = ? AND purpose = ? \
             ORDER BY id DESC LIMIT 1",
        )
        .bind(user_id)
        .bind(purpose.as_str())
        .fetch_optional(self.db.pool())
        .await?;
        Ok(parse_ts(raw))
    }

    /// 该用户最新一条未消费码的哈希。仅供 dev-mode 测试钩子反查。
    pub async fn latest_code_hash_for_dev(
        &self,
        user_id: i64,
    ) -> Result<Option<String>, StoreError> {
        let hash: Option<String> = sqlx::query_scalar(
            "SELECT code_hash FROM verification_codes \
             WHERE user_id = ? AND consumed_at IS NULL \
             ORDER BY id DESC LIMIT 1",
        )
        .bind(user_id)
        .fetch_optional(self.db.pool())
        .await?;
        Ok(hash)
    }

    /// 校验最新未消费码。
    pub async fn verify(
        &self,
        user_id: i64,
        purpose: CodePurpose,
        code: &str,
        now: DateTime<Utc>,
    ) -> Result<CodeVerifyOutcome, StoreError> {
        let row = sqlx::query(
            "SELECT id, code_hash, attempts, expires_at FROM verification_codes \
             WHERE user_id = ? AND purpose = ? AND consumed_at IS NULL \
             ORDER BY id DESC LIMIT 1",
        )
        .bind(user_id)
        .bind(purpose.as_str())
        .fetch_optional(self.db.pool())
        .await?;
        let Some(row) = row else {
            return Ok(CodeVerifyOutcome::Invalid);
        };
        let id: i64 = row.get("id");
        let attempts: i64 = row.get("attempts");
        if attempts >= 5 {
            return Ok(CodeVerifyOutcome::Locked);
        }
        let expires_at =
            parse_ts(row.get::<Option<String>, _>("expires_at")).unwrap_or_else(Utc::now);
        if expires_at <= now {
            return Ok(CodeVerifyOutcome::Expired);
        }
        let stored: String = row.get("code_hash");
        if stored != Self::hash_code(user_id, code) {
            let new_attempts: i64 = sqlx::query_scalar(
                "UPDATE verification_codes SET attempts = attempts + 1 WHERE id = ? \
                 RETURNING attempts",
            )
            .bind(id)
            .fetch_one(self.db.pool())
            .await?;
            return Ok(if new_attempts >= 5 {
                CodeVerifyOutcome::Locked
            } else {
                CodeVerifyOutcome::Invalid
            });
        }
        sqlx::query("UPDATE verification_codes SET consumed_at = datetime('now') WHERE id = ?")
            .bind(id)
            .execute(self.db.pool())
            .await?;
        Ok(CodeVerifyOutcome::Ok)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::user_repo::{UserRepo, UserRole};

    async fn setup() -> (VerificationRepo, i64) {
        let db = Database::open_in_memory().await.unwrap();
        let users = UserRepo::new(db.clone());
        let codes = VerificationRepo::new(db);
        let user = users
            .create("v@x.test", "h", "v", UserRole::User)
            .await
            .unwrap();
        (codes, user.id)
    }

    #[tokio::test]
    async fn five_wrong_attempts_lock_the_code() {
        let (codes, uid) = setup().await;
        codes
            .issue(uid, CodePurpose::VerifyEmail, "123456", 10)
            .await
            .unwrap();
        let now = Utc::now();
        for i in 0..4 {
            let outcome = codes
                .verify(uid, CodePurpose::VerifyEmail, "000000", now)
                .await
                .unwrap();
            assert_eq!(outcome, CodeVerifyOutcome::Invalid, "attempt {i}");
        }
        let locked = codes
            .verify(uid, CodePurpose::VerifyEmail, "000000", now)
            .await
            .unwrap();
        assert_eq!(locked, CodeVerifyOutcome::Locked);
        let still = codes
            .verify(uid, CodePurpose::VerifyEmail, "123456", now)
            .await
            .unwrap();
        assert_eq!(still, CodeVerifyOutcome::Locked);
    }

    #[tokio::test]
    async fn correct_code_consumes_and_issue_invalidates_old() {
        let (codes, uid) = setup().await;
        codes
            .issue(uid, CodePurpose::VerifyEmail, "111111", 10)
            .await
            .unwrap();
        codes
            .issue(uid, CodePurpose::VerifyEmail, "222222", 10)
            .await
            .unwrap();
        let now = Utc::now();
        assert_eq!(
            codes
                .verify(uid, CodePurpose::VerifyEmail, "111111", now)
                .await
                .unwrap(),
            CodeVerifyOutcome::Invalid
        );
        assert_eq!(
            codes
                .verify(uid, CodePurpose::VerifyEmail, "222222", now)
                .await
                .unwrap(),
            CodeVerifyOutcome::Ok
        );
        assert_eq!(
            codes
                .verify(uid, CodePurpose::VerifyEmail, "222222", now)
                .await
                .unwrap(),
            CodeVerifyOutcome::Invalid
        );
        assert!(
            codes
                .latest_created_at(uid, CodePurpose::VerifyEmail)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn expired_code_is_expired_not_invalid() {
        let (codes, uid) = setup().await;
        codes
            .issue(uid, CodePurpose::ResetPassword, "999999", 10)
            .await
            .unwrap();
        let later = Utc::now() + chrono::Duration::hours(1);
        assert_eq!(
            codes
                .verify(uid, CodePurpose::ResetPassword, "999999", later)
                .await
                .unwrap(),
            CodeVerifyOutcome::Expired
        );
    }

    #[test]
    fn hash_code_is_sha256_hex_of_user_and_code() {
        let hash = VerificationRepo::hash_code(1, "123456");
        assert_eq!(hash.len(), 64);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(hash, VerificationRepo::hash_code(2, "123456"));
    }
}
