-- 渠道高级配置：自定义请求头（中转站的自有鉴权头/机房路由头是刚需，
-- param_override 只管 body 管不了 header）与测试模型（连通性测试和
-- 定时重测统一用它，而不是碰运气拿端点第一个模型）。
ALTER TABLE channels ADD COLUMN extra_headers TEXT;
ALTER TABLE channels ADD COLUMN test_model TEXT;
