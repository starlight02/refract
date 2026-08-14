-- 密钥备注：渠道早就有 note，密钥一直缺 —— 多把密钥分发给不同工具后，
-- 光靠名字回忆不起来「这把是给谁的、当时为什么限了 30 RPM」。
ALTER TABLE api_keys ADD COLUMN note TEXT;
