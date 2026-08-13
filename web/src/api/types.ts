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
  address: UpstreamAddress
  endpoints: ChannelEndpoint[]
  tags: string[]
  timeout_secs: number
  proxy?: string | null
  param_override?: Record<string, unknown> | null
  note?: string | null
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
  expires_at?: string | null
  last_used_at?: string | null
  created_at: string
}

export interface NewApiKey {
  name: string
  allowed_models?: string[]
  allowed_tags?: string[]
  quota?: number
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
  reasoning_tokens: number
  retries: number
  error_kind?: string | null
  error_message?: string | null
}

export interface LogFilter {
  model?: string
  channel_id?: number
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
}

export interface ModelStat {
  model: string
  requests: number
  input_tokens: number
  output_tokens: number
}

/** 按网关密钥聚合的用量。 */
export interface KeyUsageStat {
  api_key_id: number
  requests: number
  failures: number
  input_tokens: number
  output_tokens: number
}

// ── 路由策略 ──

export type SelectionMode = 'weighted_random' | 'round_robin' | 'first'

export interface RoutingPolicy {
  native_first: boolean
  selection: SelectionMode
  max_attempts: number
  retry_same_channel: boolean
}

/** 自动清理请求日志的保留周期。 */
export interface LogRetentionSetting {
  days: number
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
}
