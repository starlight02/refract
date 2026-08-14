-- 请求成本：按落库当时生效的价表计算并固化。
-- 存快照而非查询时计算 —— 单价会变，但历史账单不应跟着变。
ALTER TABLE request_logs ADD COLUMN cost REAL NOT NULL DEFAULT 0;
