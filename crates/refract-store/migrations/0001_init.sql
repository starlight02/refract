-- Refract 初始 schema。
--
-- 设计要点：
-- 1. 所有业务表都带 `owner_id`，当前恒为 1。加多用户时只需放开来源，不动表结构。
-- 2. 渠道的协议端点独立成表，而非塞进 JSON 列 —— 端点是路由的原子单位，
--    需要能被索引与单独更新。
-- 3. 模型列表存在端点表的 JSON 列里：它总是与端点整体读写，拆表只会增加
--    join 成本而没有查询收益（模型名的查找由内存索引负责，不走 SQL）。

CREATE TABLE IF NOT EXISTS channels (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    owner_id        INTEGER NOT NULL DEFAULT 1,
    name            TEXT    NOT NULL,
    kind            TEXT    NOT NULL,
    enabled         INTEGER NOT NULL DEFAULT 1,
    priority        INTEGER NOT NULL DEFAULT 0,
    weight          INTEGER NOT NULL DEFAULT 1,
    credential      TEXT    NOT NULL DEFAULT '',
    address         TEXT    NOT NULL DEFAULT '{}',
    tags            TEXT    NOT NULL DEFAULT '[]',
    timeout_secs    INTEGER NOT NULL DEFAULT 0,
    proxy           TEXT,
    param_override  TEXT,
    note            TEXT,
    created_at      TEXT    NOT NULL DEFAULT (datetime('now')),
    updated_at      TEXT    NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_channels_owner_enabled
    ON channels (owner_id, enabled);

CREATE TABLE IF NOT EXISTS channel_endpoints (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    channel_id    INTEGER NOT NULL REFERENCES channels (id) ON DELETE CASCADE,
    protocol      TEXT    NOT NULL,
    sort_order    INTEGER NOT NULL DEFAULT 0,
    enabled       INTEGER NOT NULL DEFAULT 1,
    address       TEXT    NOT NULL DEFAULT '{}',
    credential    TEXT,
    models        TEXT    NOT NULL DEFAULT '[]',
    transcode     TEXT    NOT NULL DEFAULT '{}',
    UNIQUE (channel_id, protocol)
);

CREATE INDEX IF NOT EXISTS idx_endpoints_channel
    ON channel_endpoints (channel_id, sort_order);

-- 网关自身的 API 密钥（客户端拿它来调用本网关）。
CREATE TABLE IF NOT EXISTS api_keys (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    owner_id     INTEGER NOT NULL DEFAULT 1,
    name         TEXT    NOT NULL,
    key_hash     TEXT    NOT NULL UNIQUE,
    key_prefix   TEXT    NOT NULL,
    enabled      INTEGER NOT NULL DEFAULT 1,
    -- 允许访问的模型白名单，空数组表示不限制。
    allowed_models TEXT  NOT NULL DEFAULT '[]',
    -- 允许访问的渠道标签，空数组表示不限制。
    allowed_tags TEXT    NOT NULL DEFAULT '[]',
    quota        INTEGER NOT NULL DEFAULT 0,
    used_quota   INTEGER NOT NULL DEFAULT 0,
    expires_at   TEXT,
    last_used_at TEXT,
    created_at   TEXT    NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_api_keys_hash ON api_keys (key_hash);

-- 请求日志。
CREATE TABLE IF NOT EXISTS request_logs (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    owner_id           INTEGER NOT NULL DEFAULT 1,
    request_id         TEXT    NOT NULL,
    created_at         TEXT    NOT NULL DEFAULT (datetime('now')),
    api_key_id         INTEGER,
    channel_id         INTEGER,
    channel_name       TEXT,
    -- 客户端打进来用的协议。
    inbound_protocol   TEXT    NOT NULL,
    -- 上游端点的原生协议。
    upstream_protocol  TEXT    NOT NULL,
    -- 是否发生了协议转换。
    transcoded         INTEGER NOT NULL DEFAULT 0,
    model              TEXT    NOT NULL,
    upstream_model     TEXT    NOT NULL,
    stream             INTEGER NOT NULL DEFAULT 0,
    status             INTEGER NOT NULL,
    -- 首字节延迟（毫秒），流式请求才有意义。
    ttfb_ms            INTEGER,
    duration_ms        INTEGER NOT NULL,
    input_tokens       INTEGER NOT NULL DEFAULT 0,
    output_tokens      INTEGER NOT NULL DEFAULT 0,
    cached_tokens      INTEGER NOT NULL DEFAULT 0,
    reasoning_tokens   INTEGER NOT NULL DEFAULT 0,
    retries            INTEGER NOT NULL DEFAULT 0,
    error_kind         TEXT,
    error_message      TEXT
);

CREATE INDEX IF NOT EXISTS idx_logs_owner_time
    ON request_logs (owner_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_logs_channel
    ON request_logs (channel_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_logs_model
    ON request_logs (model, created_at DESC);

-- 渠道健康度。与 channels 分表：它是高频写入的运行时状态，
-- 和低频写入的配置放一起会让配置读取被写锁拖慢。
CREATE TABLE IF NOT EXISTS channel_health (
    channel_id        INTEGER NOT NULL,
    protocol          TEXT    NOT NULL,
    consecutive_fails INTEGER NOT NULL DEFAULT 0,
    total_requests    INTEGER NOT NULL DEFAULT 0,
    total_failures    INTEGER NOT NULL DEFAULT 0,
    last_success_at   TEXT,
    last_failure_at   TEXT,
    last_error        TEXT,
    -- 熔断到期时间；为空表示未熔断。
    suspended_until   TEXT,
    avg_latency_ms    INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (channel_id, protocol),
    -- 渠道删除后健康行必须一起消失：SQLite 会复用自增 ID，残留的熔断状态
    -- 会让新建的同 ID 渠道一出生就是「已熔断」。
    FOREIGN KEY (channel_id) REFERENCES channels (id) ON DELETE CASCADE
);

-- 运行时可调配置，键值对。
CREATE TABLE IF NOT EXISTS settings (
    key        TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
