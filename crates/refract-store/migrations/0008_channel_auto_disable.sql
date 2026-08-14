-- 自动禁用标记：终态错误（401/403，key 废了）连续出现后由网关自动停用渠道。
-- 与手动禁用分开记 —— 自动禁用的渠道参与定时重测自愈，手动禁用的不碰。
ALTER TABLE channels ADD COLUMN auto_disabled INTEGER NOT NULL DEFAULT 0;
