-- 预付费钱包与账本（D8/D9）。部分唯一索引兜底 charge 幂等（D10）。
CREATE TABLE wallets (
  user_id INTEGER PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
  balance REAL NOT NULL DEFAULT 0,
  currency TEXT NOT NULL DEFAULT 'USD',
  updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE TABLE wallet_ledger (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  delta REAL NOT NULL,
  balance_after REAL NOT NULL,
  kind TEXT NOT NULL CHECK(kind IN ('topup','charge','refund','adjust')),
  ref_id TEXT,
  note TEXT NOT NULL DEFAULT '',
  created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE UNIQUE INDEX idx_ledger_idem ON wallet_ledger(user_id, kind, ref_id) WHERE ref_id IS NOT NULL;
CREATE INDEX idx_ledger_user_time ON wallet_ledger(user_id, created_at DESC);
