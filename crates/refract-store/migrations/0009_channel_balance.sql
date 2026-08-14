-- 上游余额缓存：多中转站自用的每日之问是「这个站还剩多少钱」。
-- 查询走 OpenAI 兼容的 /v1/dashboard/billing/* 端点（中转站事实标准），
-- 结果缓存在这里，渠道列表直接显示，手动/定时刷新。
ALTER TABLE channels ADD COLUMN balance REAL;
ALTER TABLE channels ADD COLUMN balance_updated_at TEXT;
