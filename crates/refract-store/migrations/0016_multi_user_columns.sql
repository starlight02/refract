-- 渠道可见性 + 密钥/日志的用户归属（D6/D11）。request_logs.user_id 不加 FK，
-- 用户删除后日志仍可被管理员审计。
ALTER TABLE channels ADD COLUMN visibility TEXT NOT NULL DEFAULT 'shared' CHECK(visibility IN ('shared','private'));
ALTER TABLE channels ADD COLUMN user_id INTEGER REFERENCES users(id) ON DELETE CASCADE; -- 私有渠属主；shared 时 NULL
ALTER TABLE api_keys ADD COLUMN user_id INTEGER REFERENCES users(id) ON DELETE CASCADE;
ALTER TABLE request_logs ADD COLUMN user_id INTEGER; -- 不 FK，日志保留期可能长于用户
CREATE INDEX idx_channels_visibility ON channels(visibility, user_id);
CREATE INDEX idx_api_keys_user ON api_keys(user_id);
CREATE INDEX idx_logs_user_time ON request_logs(user_id, created_at DESC);
-- 现有数据归 admin（user_id 在 admin 用户创建后回填，见 Step 1.3）
