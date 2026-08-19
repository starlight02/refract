/**
 * 与后端 Rust struct 1:1 对齐的类型定义。
 *
 * 设计约束：字段名必须与后端 serde 序列化名一致（snake_case），
 * 因为前后端之间没有任何 DTO 转换层 —— 前端直接 JSON.parse。
 */

// ── 协议 ──

export type Protocol = 'chat' | 'responses' | 'messages' | 'gemini'

// ── 地址 ──

export interface UpstreamAddress {
  unofficial: boolean
  full_address: boolean
  base_url?: string | null
  version_prefix?: string | null
  path?: string | null
}

// ── 凭据 ──

/** 后端 Credential 被序列化为透明字符串。 */
export type Credential = string

/** 渠道多密钥池的使用策略。 */
export type KeyStrategy = 'sticky' | 'round_robin' | 'random'

// ── 协议转换策略 ──

export interface TranscodePolicy {
  enabled: boolean
  accepted: ProtocolSet
}

/**
 * 协议集合。
 *
 * 后端在内存里是 u8 位图，但 serde 把它序列化成协议名数组
 * （refract-core/src/protocol.rs 的 `impl Serialize for ProtocolSet` 用
 * `collect_seq`，反序列化走 `Vec::<Protocol>::deserialize`）。
 * 线上格式是数组，前端就按数组处理 —— 送数字会被 serde 拒掉。
 */
export type ProtocolSet = Protocol[]

// ── 模型条目 ──

export interface ModelEntry {
  name: string
  upstream?: string | null
}

// ── 协议端点 ──

export interface ChannelEndpoint {
  protocol: Protocol
  order: number
  enabled: boolean
  address: UpstreamAddress
  credential?: Credential | null
  models: ModelEntry[]
  transcode: TranscodePolicy
}

export interface EmptyResponseRetryOverride {
  window_secs: number | null
  max_retries: number | null
}

// ── 渠道 ──

export interface Channel {
  id: number
  owner_id: number
  name: string
  kind: ChannelKind
  enabled: boolean
  priority: number
  weight: number
  credential: Credential
  /** 附加密钥池：每行一把钥匙，与主密钥一起构成轮换池（为空时后端省略该字段）。 */
  credentials?: Credential[]
  /** 多密钥池策略。 */
  key_strategy: KeyStrategy
  address: UpstreamAddress
  endpoints: ChannelEndpoint[]
  tags: string[]
  timeout_secs: number
  proxy?: string | null
  param_override?: Record<string, unknown> | null
  note?: string | null
  /** 因终态错误被网关自动禁用（保留定时重测自愈资格）。 */
  auto_disabled?: boolean
  /** 上游余额缓存。 */
  balance?: number | null
  /** 余额最后刷新时间。 */
  balance_updated_at?: string | null
  /** 注入到上游请求的自定义头。 */
  extra_headers?: [string, string][]
  /** 连通性测试/定时重测使用的模型。 */
  test_model?: string | null
  /** HTTP 200 空回复重试覆盖；null 表示继承全局值。 */
  empty_response_retry: EmptyResponseRetryOverride
}

// ── 渠道类型 ──

/** 单协议渠道的值为协议名字符串，聚合渠道为 'aggregate'。 */
export type ChannelKind = Protocol | 'aggregate'

// ── API 密钥 ──

export interface ApiKey {
  id: number
  owner_id: number
  name: string
  key_prefix: string
  enabled: boolean
  allowed_models: string[]
  allowed_tags: string[]
  quota: number
  used_quota: number
  rpm_limit: number
  tpm_limit: number
  budget: number
  used_budget: number
  note?: string | null
  expires_at?: string | null
  last_used_at?: string | null
  created_at: string
}

export interface NewApiKey {
  name: string
  allowed_models?: string[]
  allowed_tags?: string[]
  quota?: number
  rpm_limit?: number
  tpm_limit?: number
  budget?: number
  note?: string | null
  expires_at?: string | null
}

/** 创建密钥时返回，含一次性明文。 */
export interface CreatedApiKey {
  key: ApiKey
  plaintext: string
}

// ── 请求日志 ──

export interface RequestLog {
  id: number
  request_id: string
  created_at: string
  api_key_id?: number | null
  channel_id?: number | null
  channel_name?: string | null
  inbound_protocol: string
  upstream_protocol: string
  transcoded: boolean
  model: string
  upstream_model: string
  stream: boolean
  status: number
  ttfb_ms?: number | null
  duration_ms: number
  input_tokens: number
  output_tokens: number
  cached_tokens: number
  cache_write_tokens: number
  reasoning_tokens: number
  retries: number
  cost: number
  error_kind?: string | null
  error_message?: string | null
  /** 实际使用的上游钥匙的脱敏提示（如 `sk-a…9f2c`）。 */
  credential_hint?: string | null
  /** 命中并已使用的亲和规则名。 */
  affinity_rule?: string | null
  /** 请求正文快照。仅单条详情接口返回。 */
  request_body?: string | null
  /** 响应正文快照（流式为聚合文本）。仅单条详情接口返回。 */
  response_body?: string | null
}

export interface LogFilter {
  model?: string
  channel_id?: number
  api_key_id?: number
  request_id?: string
  since?: string
  until?: string
  failures_only?: boolean
  limit?: number
  offset?: number
}

// ── 统计 ──

export interface StatsSummary {
  requests: number
  failures: number
  input_tokens: number
  output_tokens: number
  avg_duration_ms: number
  avg_ttfb_ms?: number | null
  transcoded: number
  cost: number
}

export interface ModelStat {
  model: string
  requests: number
  input_tokens: number
  output_tokens: number
  cost: number
  avg_ttfb_ms?: number | null
  avg_duration_ms: number
  tokens_per_sec?: number | null
}

/** 按渠道聚合的用量。 */
export interface ChannelStat {
  channel_id?: number | null
  channel_name: string
  requests: number
  failures: number
  input_tokens: number
  output_tokens: number
  cost: number
  avg_ttfb_ms?: number | null
  avg_duration_ms: number
}

/** 一个时间桶的聚合。 */
export interface TimeBucket {
  bucket: string
  requests: number
  failures: number
  input_tokens: number
  output_tokens: number
  cost: number
}

/** 网关级全局限制。所有字段 0 = 不限。 */
export interface GlobalLimits {
  /** 每分钟请求数上限。 */
  rpm: number
  /** 每分钟 token 数上限；挡 RPM 挡不住的「少量请求 × 巨大上下文」。 */
  tpm: number
  /** 同时在途请求上限。 */
  max_concurrency: number
}

/** 按来源 IP 的限制。 */
export interface IpLimits {
  /** 单 IP 每分钟请求数上限；0 = 不限。 */
  rpm: number
}

export interface EmptyResponseRetryPolicy {
  window_secs: number
  max_retries: number
  reject_nonstandard_200: boolean
}

/** 按网关密钥聚合的用量。 */
export interface KeyUsageStat {
  api_key_id: number
  requests: number
  failures: number
  input_tokens: number
  output_tokens: number
  cost: number
}

// ── 路由策略 ──

export type SelectionMode = 'weighted_random' | 'round_robin' | 'first'

export interface RoutingPolicy {
  native_first: boolean
  selection: SelectionMode
  max_attempts: number
  retry_same_channel: boolean
  /** 单请求允许的上游调用总次数（含重试）；0 = 不限，后端缺省 8。 */
  max_upstream_calls: number
}

/** 自动清理请求日志的保留周期。 */
export interface LogRetentionSetting {
  days: number
}

/** 通知与自愈设置。 */
export interface NotifySettings {
  webhook_url?: string | null
  retest_minutes: number
}

/** 一条模型计价规则。pattern 为精确名或 `*` 结尾的前缀通配。 */
export interface ModelPrice {
  pattern: string
  input_per_m: number
  output_per_m: number
  /** 缓存命中价（每百万）。缺省按输入价。 */
  cached_input_per_m?: number | null
  /** 缓存写入价（每百万）。缺省按输入价。 */
  cache_write_per_m?: number | null
}

/** 熔断策略。threshold 为 0 表示关闭熔断。 */
export interface BreakerPolicy {
  failure_threshold: number
  base_cooldown_secs: number
  max_cooldown_secs: number
}

// ── 端点健康度 ──

export interface EndpointHealth {
  channel_id: number
  protocol: Protocol
  consecutive_fails: number
  total_requests: number
  total_failures: number
  last_success_at?: string | null
  last_failure_at?: string | null
  last_error?: string | null
  suspended_until?: string | null
  avg_latency_ms: number
}

// ── 模型探测 ──

export interface ModelProbe {
  id: string
  display_name?: string | null
}

export interface ProbeResult {
  models: ModelProbe[]
}

export interface ChannelTestResult {
  success: boolean
  message: string
  /** 上游返回的 HTTP 状态；连接层面就失败时为 null。 */
  upstream_status?: number | null
  /** 测试请求耗时（毫秒）。 */
  latency_ms?: number
}

// ── 渠道亲和性 ──

/** 身份值来源。与后端 `AffinityKeySource` 的内部标记表示一致。 */
export type AffinityKeySource =
  | { kind: 'api_key_id' }
  | { kind: 'header'; name: string }
  | { kind: 'body'; path: string }

/** 一条亲和规则。 */
export interface AffinityRule {
  /** 规则名：缓存键的一部分，保存时要求唯一。 */
  name: string
  /** 仅对匹配的模型生效（正则）。空串 = 全部模型。 */
  model_regex: string
  /** 仅对匹配的入站路径生效（正则）。空串 = 全部路径。 */
  path_regex: string
  /** 身份值来源，按顺序求值，首个能取到非空值者生效。 */
  sources: AffinityKeySource[]
  /** 对取到的身份值再做一次正则筛选（空串 = 不过滤）。 */
  value_regex: string
  /** 绑定存活秒数；缺省用全局 default_ttl_secs。 */
  ttl_secs?: number | null
  /** 缓存键是否包含模型名（默认开）。 */
  include_model: boolean
  /** 钉住渠道失败时不再重试其他渠道。 */
  skip_retry_on_failure: boolean
}

/** 渠道亲和性总开关与全局参数。 */
export interface AffinitySettings {
  enabled: boolean
  /** 最终由非钉住渠道成功时，把身份重新绑到新渠道。 */
  switch_on_success: boolean
  /** 钉住的渠道被停用后保留绑定。 */
  keep_on_channel_disabled: boolean
  /** 缓存最大条目数，超出按 LRU 淘汰。 */
  max_entries: number
  /** 规则未自带 TTL 时的默认秒数。 */
  default_ttl_secs: number
  rules: AffinityRule[]
}

/** 亲和引擎运行统计。 */
export interface AffinityStats {
  hits: number
  misses: number
  records: number
  forgets: number
  evictions: number
  entries: number
}

export interface AffinityStatsResponse {
  /** 总开关开且规则非空。 */
  active: boolean
  stats: AffinityStats
}

// ── 备份与凭据加密 ──

/** 自动备份设置。 */
export interface BackupSettings {
  /** 备份目录；null 使用内置默认目录。 */
  directory?: string | null
  /** 备份间隔小时数；0 = 关闭自动备份。 */
  interval_hours: number
  /** 保留的备份份数。 */
  keep: number
}

/** 备份文件列表条目。 */
export interface BackupFile {
  name: string
  size_bytes: number
  created_at: string
}

/**
 * 密钥类设置的只写状态。
 *
 * 服务端只保存哈希或密文，GET 永远不回明文；PUT 传 null 清除。
 */
export interface SecretConfigured {
  configured: boolean
}
