-- 邮箱验证 / 密码重置码（D3/D20）。session_revoked_at 用于改密后使旧 ticket 失效（Step 2.3）。
CREATE TABLE verification_codes (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  purpose TEXT NOT NULL CHECK(purpose IN ('verify_email','reset_password')),
  code_hash TEXT NOT NULL,
  attempts INTEGER NOT NULL DEFAULT 0,
  expires_at TEXT NOT NULL,
  consumed_at TEXT,
  created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX idx_codes_user_purpose ON verification_codes(user_id, purpose);
ALTER TABLE users ADD COLUMN session_revoked_at TEXT;
