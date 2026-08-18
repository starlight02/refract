-- 渠道多密钥池（一行一把）与密钥使用策略；请求日志补充「用了哪把钥匙」
-- （脱敏 hint）与「命中哪条亲和规则」两个排障字段。
ALTER TABLE channels ADD COLUMN credentials TEXT;
ALTER TABLE channels ADD COLUMN key_strategy TEXT NOT NULL DEFAULT 'round_robin';
ALTER TABLE request_logs ADD COLUMN credential_hint TEXT;
ALTER TABLE request_logs ADD COLUMN affinity_rule TEXT;
