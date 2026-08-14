-- 金额配额：token 配额对贵贱模型一视同仁（1M token 的 mini 与 opus 成本差
-- 百倍），个人预算天然以钱计。费用已逐请求算好（cost 列），这里把它累计到
-- 密钥上并参与鉴权判定。
ALTER TABLE api_keys ADD COLUMN budget REAL NOT NULL DEFAULT 0;
ALTER TABLE api_keys ADD COLUMN used_budget REAL NOT NULL DEFAULT 0;
