-- 缓存写入 token（Anthropic cache_creation_input_tokens，实价 1.25 倍输入价）。
-- 之前这个数在 IR 里有、落库时被丢弃 —— Claude 重度缓存负载的账单因此失真。
ALTER TABLE request_logs ADD COLUMN cache_write_tokens INTEGER NOT NULL DEFAULT 0;
