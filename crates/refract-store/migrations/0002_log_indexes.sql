-- 日志查询与清理的索引补齐。
--
-- 列表查询是 `WHERE owner_id = ? ORDER BY id DESC LIMIT ?`，而 0001 只有
-- (owner_id, created_at DESC) —— 它给不出 id 序，大表上每次翻页都要整体排序。
-- channel / model 过滤的列表查询同理。summary / by_model 是 created_at 范围
-- 聚合，继续使用 0001 的 idx_logs_owner_time。

DROP INDEX IF EXISTS idx_logs_channel;
DROP INDEX IF EXISTS idx_logs_model;

CREATE INDEX IF NOT EXISTS idx_logs_owner_rowid
    ON request_logs (owner_id, id DESC);
CREATE INDEX IF NOT EXISTS idx_logs_channel
    ON request_logs (channel_id, id DESC);
CREATE INDEX IF NOT EXISTS idx_logs_model
    ON request_logs (model, id DESC);

-- prune 按 created_at 删除且不带 owner 条件；没有这个索引每次清理都全表扫。
CREATE INDEX IF NOT EXISTS idx_logs_created
    ON request_logs (created_at);
