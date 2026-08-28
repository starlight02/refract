//! 用户钱包与账本仓储。

use std::str::FromStr;

use chrono::{DateTime, Utc};
use sqlx::Row;

use crate::db::{Database, StoreError, is_unique_violation};
use crate::key_repo::parse_ts;

/// 账本条目种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LedgerKind {
    /// 充值。
    Topup,
    /// 用量扣款。
    Charge,
    /// 退款。
    Refund,
    /// 管理员调整。
    Adjust,
}

impl LedgerKind {
    /// 数据库与 API 中的稳定字符串。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Topup => "topup",
            Self::Charge => "charge",
            Self::Refund => "refund",
            Self::Adjust => "adjust",
        }
    }
}

impl FromStr for LedgerKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "topup" => Ok(Self::Topup),
            "charge" => Ok(Self::Charge),
            "refund" => Ok(Self::Refund),
            "adjust" => Ok(Self::Adjust),
            other => Err(format!("unknown ledger kind: {other}")),
        }
    }
}

/// 用户钱包。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Wallet {
    /// 所属用户。
    pub user_id: i64,
    /// 当前余额。
    pub balance: f64,
    /// 币种。
    pub currency: String,
    /// 更新时间。
    pub updated_at: DateTime<Utc>,
}

/// 一条账本记录。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LedgerEntry {
    /// 主键。
    pub id: i64,
    /// 所属用户。
    pub user_id: i64,
    /// 余额变化量（扣款为负）。
    pub delta: f64,
    /// 记账后余额。
    pub balance_after: f64,
    /// 种类。
    pub kind: LedgerKind,
    /// 幂等键（扣款时为 request_id）。
    pub ref_id: Option<String>,
    /// 备注。
    pub note: String,
    /// 记账时间。
    pub created_at: DateTime<Utc>,
}

/// 钱包仓储。
#[derive(Debug, Clone)]
pub struct WalletRepo {
    db: Database,
}

impl WalletRepo {
    /// 绑定到一个数据库。
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// 读取余额；尚无钱包行时先插入 0 余额行。
    pub async fn balance(&self, user_id: i64) -> Result<f64, StoreError> {
        Ok(self.wallet(user_id).await?.balance)
    }

    /// 读取钱包；尚无行时先插入 0 余额行。
    pub async fn wallet(&self, user_id: i64) -> Result<Wallet, StoreError> {
        self.ensure_row(user_id).await?;
        let row = sqlx::query(
            "SELECT user_id, balance, currency, updated_at FROM wallets WHERE user_id = ?",
        )
        .bind(user_id)
        .fetch_optional(self.db.pool())
        .await?
        .ok_or_else(|| StoreError::not_found("wallet", user_id))?;
        Self::wallet_from_row(&row)
    }

    /// 列出已存在钱包，用于低频 Prometheus 余额 gauge 抓取。
    /// 不为没有任何余额活动的用户创建空钱包行。
    pub async fn all_wallets(&self) -> Result<Vec<Wallet>, StoreError> {
        let rows = sqlx::query(
            "SELECT user_id, balance, currency, updated_at FROM wallets ORDER BY user_id ASC",
        )
        .fetch_all(self.db.pool())
        .await?;
        rows.iter().map(Self::wallet_from_row).collect()
    }

    /// 单事务记账：确保钱包行 → 更新余额 → 写账本。
    ///
    /// `ref_id` 非空且撞上 `idx_ledger_idem` 时返回 `Ok(false)`（已记账，不重复）。
    pub async fn apply(
        &self,
        user_id: i64,
        delta: f64,
        kind: LedgerKind,
        ref_id: Option<&str>,
        note: &str,
    ) -> Result<bool, StoreError> {
        let mut tx = self.db.pool().begin().await?;
        sqlx::query("INSERT OR IGNORE INTO wallets (user_id) VALUES (?)")
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
        let balance: f64 = sqlx::query_scalar(
            "UPDATE wallets SET balance = balance + ?, updated_at = datetime('now') \
             WHERE user_id = ? RETURNING balance",
        )
        .bind(delta)
        .bind(user_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| StoreError::not_found("wallet", user_id))?;

        let inserted = sqlx::query(
            "INSERT INTO wallet_ledger (user_id, delta, balance_after, kind, ref_id, note) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(user_id)
        .bind(delta)
        .bind(balance)
        .bind(kind.as_str())
        .bind(ref_id)
        .bind(note)
        .execute(&mut *tx)
        .await;

        match inserted {
            Ok(_) => {
                tx.commit().await?;
                Ok(true)
            }
            Err(err) if is_unique_violation(&err) => Ok(false),
            Err(err) => Err(err.into()),
        }
    }

    /// 分页查询账本，按 `created_at DESC, id DESC`。`since`/`until` 为闭区间。
    pub async fn ledger(
        &self,
        user_id: i64,
        limit: u32,
        offset: u32,
        kind: Option<LedgerKind>,
        since: Option<DateTime<Utc>>,
        until: Option<DateTime<Utc>>,
    ) -> Result<Vec<LedgerEntry>, StoreError> {
        let kind_s = kind.map(LedgerKind::as_str);
        let since_s = since.map(sqlite_ts);
        let until_s = until.map(sqlite_ts);
        let rows = sqlx::query(
            "SELECT id, user_id, delta, balance_after, kind, ref_id, note, created_at \
             FROM wallet_ledger \
             WHERE user_id = ? \
               AND (? IS NULL OR kind = ?) \
               AND (? IS NULL OR created_at >= ?) \
               AND (? IS NULL OR created_at <= ?) \
             ORDER BY created_at DESC, id DESC \
             LIMIT ? OFFSET ?",
        )
        .bind(user_id)
        .bind(kind_s)
        .bind(kind_s)
        .bind(since_s.as_deref())
        .bind(since_s.as_deref())
        .bind(until_s.as_deref())
        .bind(until_s.as_deref())
        .bind(i64::from(limit))
        .bind(i64::from(offset))
        .fetch_all(self.db.pool())
        .await?;
        rows.iter().map(Self::ledger_from_row).collect()
    }

    /// 导出用账本：同过滤，上限 50000，按 `created_at ASC`。
    pub async fn ledger_all_for_export(
        &self,
        user_id: i64,
        kind: Option<LedgerKind>,
        since: Option<DateTime<Utc>>,
        until: Option<DateTime<Utc>>,
    ) -> Result<Vec<LedgerEntry>, StoreError> {
        let kind_s = kind.map(LedgerKind::as_str);
        let since_s = since.map(sqlite_ts);
        let until_s = until.map(sqlite_ts);
        let rows = sqlx::query(
            "SELECT id, user_id, delta, balance_after, kind, ref_id, note, created_at \
             FROM wallet_ledger \
             WHERE user_id = ? \
               AND (? IS NULL OR kind = ?) \
               AND (? IS NULL OR created_at >= ?) \
               AND (? IS NULL OR created_at <= ?) \
             ORDER BY created_at ASC, id ASC \
             LIMIT 50000",
        )
        .bind(user_id)
        .bind(kind_s)
        .bind(kind_s)
        .bind(since_s.as_deref())
        .bind(since_s.as_deref())
        .bind(until_s.as_deref())
        .bind(until_s.as_deref())
        .fetch_all(self.db.pool())
        .await?;
        rows.iter().map(Self::ledger_from_row).collect()
    }

    async fn ensure_row(&self, user_id: i64) -> Result<(), StoreError> {
        sqlx::query("INSERT OR IGNORE INTO wallets (user_id) VALUES (?)")
            .bind(user_id)
            .execute(self.db.pool())
            .await?;
        Ok(())
    }

    fn wallet_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<Wallet, StoreError> {
        Ok(Wallet {
            user_id: row.get("user_id"),
            balance: row.get("balance"),
            currency: row.get("currency"),
            updated_at: parse_ts(row.get::<Option<String>, _>("updated_at"))
                .unwrap_or_else(Utc::now),
        })
    }

    fn ledger_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<LedgerEntry, StoreError> {
        let kind: String = row.get("kind");
        Ok(LedgerEntry {
            id: row.get("id"),
            user_id: row.get("user_id"),
            delta: row.get("delta"),
            balance_after: row.get("balance_after"),
            kind: kind.parse().map_err(StoreError::Invalid)?,
            ref_id: row.get("ref_id"),
            note: row.get("note"),
            created_at: parse_ts(row.get::<Option<String>, _>("created_at"))
                .unwrap_or_else(Utc::now),
        })
    }
}

fn sqlite_ts(dt: DateTime<Utc>) -> String {
    dt.format("%Y-%m-%d %H:%M:%S").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::user_repo::{UserRepo, UserRole};
    use std::collections::HashSet;

    async fn setup() -> (UserRepo, WalletRepo, i64) {
        let db = Database::open_in_memory().await.unwrap();
        let users = UserRepo::new(db.clone());
        let wallets = WalletRepo::new(db);
        let user = users
            .create("w@x.test", "h", "w", UserRole::User)
            .await
            .unwrap();
        (users, wallets, user.id)
    }

    #[tokio::test]
    async fn balance_creates_zero_row() {
        let (_users, wallets, uid) = setup().await;
        assert_eq!(wallets.balance(uid).await.unwrap(), 0.0);
        let w = wallets.wallet(uid).await.unwrap();
        assert_eq!(w.currency, "USD");
        assert_eq!(w.balance, 0.0);
    }

    #[tokio::test]
    async fn apply_idempotent_replay_does_not_double_charge() {
        let (_users, wallets, uid) = setup().await;
        assert!(
            wallets
                .apply(uid, 10.0, LedgerKind::Topup, Some("ref-1"), "first")
                .await
                .unwrap()
        );
        assert!(
            !wallets
                .apply(uid, 10.0, LedgerKind::Topup, Some("ref-1"), "replay")
                .await
                .unwrap()
        );
        assert_eq!(wallets.balance(uid).await.unwrap(), 10.0);
        let entries = wallets.ledger(uid, 10, 0, None, None, None).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].note, "first");
        assert_eq!(entries[0].balance_after, 10.0);
    }

    #[tokio::test]
    async fn concurrent_apply_same_ref_id_only_one_succeeds() {
        let (_users, wallets, uid) = setup().await;
        let a = wallets.clone();
        let b = wallets.clone();
        let (r1, r2) = tokio::join!(
            a.apply(uid, 5.0, LedgerKind::Charge, Some("req-42"), "n"),
            b.apply(uid, 5.0, LedgerKind::Charge, Some("req-42"), "n"),
        );
        let set: HashSet<bool> = [r1.unwrap(), r2.unwrap()].into_iter().collect();
        assert_eq!(set, HashSet::from([true, false]));
        assert_eq!(wallets.balance(uid).await.unwrap(), 5.0);
        assert_eq!(
            wallets
                .ledger(uid, 10, 0, None, None, None)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn ledger_orders_newest_first_export_oldest_first() {
        let (_users, wallets, uid) = setup().await;
        wallets
            .apply(uid, 1.0, LedgerKind::Topup, Some("a"), "first")
            .await
            .unwrap();
        wallets
            .apply(uid, 2.0, LedgerKind::Topup, Some("b"), "second")
            .await
            .unwrap();
        wallets
            .apply(uid, -0.5, LedgerKind::Charge, Some("c"), "third")
            .await
            .unwrap();

        let desc = wallets.ledger(uid, 10, 0, None, None, None).await.unwrap();
        assert_eq!(
            desc.iter().map(|e| e.note.as_str()).collect::<Vec<_>>(),
            vec!["third", "second", "first"]
        );
        assert!(desc[0].id > desc[1].id && desc[1].id > desc[2].id);

        let asc = wallets
            .ledger_all_for_export(uid, None, None, None)
            .await
            .unwrap();
        assert_eq!(
            asc.iter().map(|e| e.note.as_str()).collect::<Vec<_>>(),
            vec!["first", "second", "third"]
        );

        let charges = wallets
            .ledger(uid, 10, 0, Some(LedgerKind::Charge), None, None)
            .await
            .unwrap();
        assert_eq!(charges.len(), 1);
        assert_eq!(charges[0].kind, LedgerKind::Charge);
    }

    #[test]
    fn ledger_kind_roundtrip() {
        for kind in [
            LedgerKind::Topup,
            LedgerKind::Charge,
            LedgerKind::Refund,
            LedgerKind::Adjust,
        ] {
            assert_eq!(kind.as_str().parse::<LedgerKind>().unwrap(), kind);
        }
    }
}
