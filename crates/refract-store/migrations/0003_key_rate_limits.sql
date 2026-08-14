-- 每密钥速率限制：每分钟请求数（RPM）与每分钟 token 数（TPM）。
-- 0 表示不限。计量窗口在网关内存中按自然分钟维护，数据库只存策略。
ALTER TABLE api_keys ADD COLUMN rpm_limit INTEGER NOT NULL DEFAULT 0;
ALTER TABLE api_keys ADD COLUMN tpm_limit INTEGER NOT NULL DEFAULT 0;
