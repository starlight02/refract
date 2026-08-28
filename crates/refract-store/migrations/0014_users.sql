-- 多用户：账号表。email 大小写不敏感（D1），角色/状态走 CHECK（D5）。
CREATE TABLE users (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  email TEXT NOT NULL COLLATE NOCASE,
  email_normalized TEXT GENERATED ALWAYS AS (lower(email)) VIRTUAL,
  password_hash TEXT NOT NULL,
  display_name TEXT NOT NULL DEFAULT '',
  role TEXT NOT NULL DEFAULT 'user' CHECK(role IN ('user','admin')),
  status TEXT NOT NULL DEFAULT 'pending_verification' CHECK(status IN ('pending_verification','active','disabled')),
  email_verified_at TEXT,
  created_at TEXT NOT NULL DEFAULT (datetime('now')),
  updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE UNIQUE INDEX idx_users_email ON users(email_normalized);
CREATE INDEX idx_users_status ON users(status);
