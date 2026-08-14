-- 请求/响应正文快照：排障时「网关到底收到了什么、回了什么」的唯一凭据。
-- 大对象截断到网关侧上限后落库；列表查询不取这两列，只有单条详情才读。
ALTER TABLE request_logs ADD COLUMN request_body TEXT;
ALTER TABLE request_logs ADD COLUMN response_body TEXT;
