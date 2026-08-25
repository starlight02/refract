/**
 * 后端 API 客户端。
 *
 * 设计取舍：不引入 axios/ky 之类的库。这个前端只有一个后端、只发 JSON、
 * 只需要一层错误归一化 —— 原生 fetch 加三十行封装就够了，多一个依赖
 * 反而多一份升级负担和体积。
 *
 * 路径与方法必须与 `refract-api` 的路由逐字对齐；对不上时
 * 后端回的是 404/405，而前端会显示成「加载失败」，排查起来很费时间。
 */

import type {
  AffinitySettings,
  AffinityStatsResponse,
  ApiKey,
  BackupFile,
  BackupSettings,
  IpLimits,
  SecretConfigured,
  ChannelStat,
  EmptyResponseRetryPolicy,
  GlobalLimits,
  NotifySettings,
  TimeBucket,
  BreakerPolicy,
  Channel,
  ChannelTestResult,
  CreatedApiKey,
  EndpointHealth,
  KeyUsageStat,
  LogFilter,
  LogRetentionSetting,
  ModelPrice,
  ModelStat,
  NewApiKey,
  ProbeResult,
  Protocol,
  RequestLog,
  RoutingPolicy,
  StatsSummary,
  UpstreamAddress,
} from '@refract/contracts'
import { TaggedError } from 'effect/Data'
import {
  type Effect,
  catchTag,
  fail,
  gen,
  promise,
  retry,
  runPromise,
  tryPromise,
} from 'effect/Effect'
import { spaced } from 'effect/Schedule'
import { encryptPayload } from './crypto'
import { readErrorEnvelope } from '@/utils/error'

/** 后端返回的错误信封。 */
export interface ErrorEnvelope {
  code: string
  message: string
  detail?: string
}

/**
 * API 调用失败。
 *
 * 用 `TaggedError` 而不是裸 `Error` 子类：带上 `_tag` 才能被 `catchTag`
 * 精确捕获，让「哪种失败该重试」由类型系统而不是注释来保证。它仍然是真正的
 * `Error`（`instanceof Error` 成立、带 stack），所以既有的 `instanceof ApiError`
 * 判断和 devtools 体验都不变。
 */
export class ApiError extends TaggedError('ApiError')<{
  readonly status: number
  readonly code: string
  readonly message: string
  readonly detail?: string | undefined
}> {
  /** 是否是「没找到」—— UI 常需要把它和真实故障区别对待。 */
  get isNotFound(): boolean {
    return this.status === 404
  }

  /** 是否是鉴权问题，需要引导用户去填管理令牌。 */
  get isAuthError(): boolean {
    return this.status === 401 || this.status === 403
  }
}

/**
 * 「请求根本没送到」—— 全局唯一允许重试的失败。
 *
 * 为什么单独立一个标签，而不是在 ApiError 上加个布尔字段：重试策略靠标签判定，
 * 编译器于是能保证业务错误（无可用渠道、上游 5xx）永远不可能被误重试 ——
 * 这正是原先那个手写循环靠注释约束、却随时可能被改坏的地方。
 * 重试耗尽后由 `catchTag` 换回里面那个要给 UI 看的 ApiError。
 */
class BackendUnavailable extends TaggedError('BackendUnavailable')<{
  readonly api: ApiError
}> {}

/**
 * 管理 API 返回 401/403 时派发的事件名。
 *
 * 为什么用事件而不是在每个视图里处理：令牌失效是**全局**状态 —— 任何一个
 * 请求撞见 401，都意味着本浏览器保存的令牌无效或已被服务端更换。让 App.vue
 * 统一弹出令牌输入框，比每个页面各自显示「鉴权失败」再让用户去设置页找入口
 * 少绕一大圈。
 */
export const AUTH_REQUIRED_EVENT = 'refract:auth-required'

/**
 * 后端真的连不上时派发（dev 代理 503、或 fetch 网络失败）。
 * HTTP 业务错误（无渠道、上游 5xx 等）不走这里，直接把信封抛给调用方。
 */
export const BACKEND_DOWN_EVENT = 'refract:backend-down'
export const BACKEND_RESTORED_EVENT = 'refract:backend-restored'
/** GET 在后端不可达时的自动重试间隔与上限（约等 60 秒的编译窗口）。 */
const UNAVAILABLE_RETRY_MS = 1_500
const UNAVAILABLE_RETRY_MAX = 40

function announce(event: string): void {
  window.dispatchEvent(new CustomEvent(event))
}

/**
 * 管理 API 的成功响应统一被 `{ data: ... }` 包裹。
 *
 * 拆包放在这一层而不是每个调用点：调用方关心的是业务对象，
 * 信封是传输细节。
 */
interface Envelope<T> {
  data: T
}

/** fetch 自身失败（请求没送出去）时的归一化。 */
function fetchFailure(error: unknown): ApiError | BackendUnavailable | DOMException {
  // 中断不是失败：原样抛回，调用方既有的 AbortError 判断继续生效。
  if (error instanceof DOMException && error.name === 'AbortError') return error
  const message = error instanceof Error && error.message ? error.message : '网络请求失败'
  const api = new ApiError({
    status: 0,
    code: 'network_error',
    message,
    detail: String(error),
  })
  // TypeError 是浏览器对「连不上」的唯一信号。
  if (error instanceof TypeError) {
    announce(BACKEND_DOWN_EVENT)
    return new BackendUnavailable({ api })
  }
  return api
}

/** 非 2xx 响应 → 带标签的失败。 */
function httpFailure(response: Response): Effect<never, ApiError | BackendUnavailable> {
  return gen(function* () {
    if (response.status === 401 || response.status === 403) announce(AUTH_REQUIRED_EVENT)

    const envelope = readErrorEnvelope(
      yield* promise(() => response.text()),
      response.status,
      response.statusText,
    )
    const api = new ApiError({
      status: response.status,
      code: envelope.code,
      message: envelope.message,
      detail: envelope.detail,
    })

    // 仅 dev 代理的结构化 503 表示「进程不在」。生产 503（无可用渠道等）是业务错误。
    if (response.status === 503 && envelope.code === 'backend_unavailable') {
      announce(BACKEND_DOWN_EVENT)
      return yield* fail(new BackendUnavailable({ api }))
    }
    return yield* fail(api)
  })
}

async function request<T>(method: string, path: string, body?: unknown): Promise<T> {
  const headers: Record<string, string> = {}
  if (body !== undefined) headers['content-type'] = 'application/json'

  // 端到端传输加密：所有写操作的 Payload 自动走 Web Crypto 信封加密。
  // 放在重试之外 —— 密文和请求体一一对应，重算只是白烧 CPU。
  const payload =
    body !== undefined && (method === 'POST' || method === 'PUT' || method === 'PATCH')
      ? await encryptPayload(body)
      : body
  const serialized = payload === undefined ? undefined : JSON.stringify(payload)

  const exchange = gen(function* () {
    const response = yield* tryPromise({
      try: (signal) =>
        fetch(path, { method, headers, credentials: 'same-origin', body: serialized, signal }),
      catch: fetchFailure,
    })

    if (!response.ok) return yield* httpFailure(response)
    announce(BACKEND_RESTORED_EVENT)

    if (response.status === 204) return undefined as T
    const text = yield* promise(() => response.text())
    if (!text) return undefined as T

    const parsed = JSON.parse(text) as Envelope<T> | T
    return parsed && typeof parsed === 'object' && 'data' in parsed
      ? (parsed as Envelope<T>).data
      : (parsed as T)
  })

  // 只有 GET 才重试：已经拿到 HTTP 响应的业务错误必须立刻抛给 UI，
  // 而非幂等的写操作重放会产生重复副作用。
  const attempted =
    method === 'GET'
      ? exchange.pipe(
          retry({
            times: UNAVAILABLE_RETRY_MAX,
            while: (error) => error instanceof BackendUnavailable,
            schedule: spaced(UNAVAILABLE_RETRY_MS),
          }),
        )
      : exchange

  // 重试耗尽后把内部标签换回要展示给 UI 的那个错误。
  return runPromise(attempted.pipe(catchTag('BackendUnavailable', (failure) => fail(failure.api))))
}

/**
 * 带管理令牌的附件下载。
 *
 * 为什么不用 `<a href download>`：浏览器跳转不带自定义请求头，管理令牌
 * 一旦启用，裸链接就会 401 —— 导出/备份必须在 fetch 里走鉴权。
 *
 * 文件名优先取 `content-disposition`（服务端生成、含时间戳），
 * 拿不到时退回调用方给的默认名。
 */
export function download(path: string, fallbackName: string): Promise<void> {
  const save = gen(function* () {
    const response = yield* tryPromise({
      try: (signal) => fetch(path, { credentials: 'same-origin', signal }),
      catch: fetchFailure,
    })
    if (!response.ok) return yield* httpFailure(response)

    const disposition = response.headers.get('content-disposition') ?? ''
    const filename = /filename="?([^";]+)"?/.exec(disposition)?.[1] ?? fallbackName
    const url = URL.createObjectURL(yield* promise(() => response.blob()))
    const anchor = document.createElement('a')
    anchor.href = url
    anchor.download = filename
    anchor.click()
    // 下载是异步启动的：立刻 revoke 可能抢在浏览器读取 blob 之前。
    window.setTimeout(() => URL.revokeObjectURL(url), 10_000)
  })

  return runPromise(save.pipe(catchTag('BackendUnavailable', (failure) => fail(failure.api))))
}

/** 仅把 URL 查询串支持的标量写进去；拒绝对象被悄悄编码成 `[object Object]`。 */
function query(params: object): string {
  const search = new URLSearchParams()
  for (const [key, value] of Object.entries(params)) {
    // 显式跳过 undefined/null/空串：`?model=` 会被后端当成「筛选空模型名」。
    if (value === undefined || value === null || value === '') continue
    if (typeof value === 'string') search.set(key, value)
    else if (typeof value === 'number') search.set(key, value.toString())
    else if (typeof value === 'boolean') search.set(key, value ? 'true' : 'false')
  }
  const encoded = search.toString()
  return encoded ? `?${encoded}` : ''
}

/** 渠道管理。 */
export const channels = {
  list: () => request<Channel[]>('GET', '/api/channels'),
  get: (id: number) => request<Channel>('GET', `/api/channels/${id}`),
  create: (channel: Channel) => request<Channel>('POST', '/api/channels', channel),
  update: (channel: Channel) => request<Channel>('PUT', `/api/channels/${channel.id}`, channel),
  remove: (id: number) => request<{ deleted: number }>('DELETE', `/api/channels/${id}`),
  setEnabled: (id: number, enabled: boolean) =>
    request<{ id: number; enabled: boolean }>('POST', `/api/channels/${id}/enabled`, { enabled }),
  /** 拉取上游真实模型列表，用于一键同步。`protocol` 省略时用首选端点。 */
  probe: (id: number, protocol?: Protocol) =>
    request<ProbeResult>('POST', `/api/channels/${id}/probe`, { protocol: protocol ?? null }),
  /** 在未保存时直接按草稿参数探测上游真实模型列表。 */
  probeDirect: (spec: {
    protocol: Protocol
    address?: UpstreamAddress
    credential?: string | null
    proxy?: string | null
  }) => request<ProbeResult>('POST', '/api/channels/probe-direct', spec),
  test: (id: number, protocol?: Protocol, model?: string) =>
    request<ChannelTestResult>('POST', `/api/channels/${id}/test`, {
      protocol: protocol ?? null,
      model: model ?? null,
    }),
  /** 复制渠道。副本以禁用状态创建。 */
  duplicate: (id: number) => request<Channel>('POST', `/api/channels/${id}/duplicate`),
  /** 批量启用/禁用/删除。 */
  bulk: (ids: number[], action: 'enable' | 'disable' | 'delete') =>
    request<{ affected: number }>('POST', '/api/channels/bulk', { ids, action }),
  /** 查询上游余额（OpenAI 兼容 billing 端点）并缓存。 */
  balance: (id: number) =>
    request<{ id: number; balance: number }>('POST', `/api/channels/${id}/balance`),
}

/** 网关自身的 API 密钥。 */
export const keys = {
  list: () => request<ApiKey[]>('GET', '/api/keys'),
  /** 返回值里的 `plaintext` 只在创建时出现一次，之后无法再取回。 */
  create: (spec: NewApiKey) => request<CreatedApiKey>('POST', '/api/keys', spec),
  /** 编辑治理属性；密钥本体不变，客户端无需换钥匙。 */
  update: (id: number, spec: NewApiKey) => request<ApiKey>('PUT', `/api/keys/${id}`, spec),
  /** 已用配额清零。 */
  resetUsage: (id: number) =>
    request<{ id: number; used_quota: number }>('POST', `/api/keys/${id}/reset-usage`),
  remove: (id: number) => request<{ deleted: number }>('DELETE', `/api/keys/${id}`),
  setEnabled: (id: number, enabled: boolean) =>
    request<{ id: number; enabled: boolean }>('POST', `/api/keys/${id}/enabled`, { enabled }),
}

/** 请求日志与统计。 */
export const logs = {
  query: (filter: LogFilter = {}) => request<RequestLog[]>('GET', `/api/logs${query(filter)}`),
  /** 单条完整记录，含请求/响应正文快照。 */
  get: (id: number) => request<RequestLog>('GET', `/api/logs/${id}`),
  prune: (days: number) => request<{ removed: number }>('POST', '/api/logs/prune', { days }),
  summary: (hours = 24) => request<StatsSummary>('GET', `/api/stats${query({ hours })}`),
  byModel: (hours = 24) => request<ModelStat[]>('GET', `/api/stats/models${query({ hours })}`),
  byKey: (hours = 24) => request<KeyUsageStat[]>('GET', `/api/stats/keys${query({ hours })}`),
  byChannel: (hours = 24) =>
    request<ChannelStat[]>('GET', `/api/stats/channels${query({ hours })}`),
  timeseries: (hours = 24, bucket: 'hour' | 'day' = 'hour') =>
    request<TimeBucket[]>('GET', `/api/stats/timeseries${query({ hours, bucket })}`),
  /** 按当前筛选导出 NDJSON（带鉴权下载）。 */
  export: (filter: LogFilter = {}) =>
    download(`/api/logs/export${query(filter)}`, 'refract-logs.ndjson'),
}

/** 运行时设置。 */
export const settings = {
  routingPolicy: () => request<RoutingPolicy>('GET', '/api/settings/routing'),
  setRoutingPolicy: (policy: RoutingPolicy) =>
    request<RoutingPolicy>('PUT', '/api/settings/routing', policy),
  logRetention: () => request<LogRetentionSetting>('GET', '/api/settings/log-retention'),
  setLogRetention: (days: number) =>
    request<LogRetentionSetting>('PUT', '/api/settings/log-retention', { days }),
  breakerPolicy: () => request<BreakerPolicy>('GET', '/api/settings/breaker'),
  setBreakerPolicy: (policy: BreakerPolicy) =>
    request<BreakerPolicy>('PUT', '/api/settings/breaker', policy),
  pricing: () => request<ModelPrice[]>('GET', '/api/settings/pricing'),
  setPricing: (prices: ModelPrice[]) =>
    request<ModelPrice[]>('PUT', '/api/settings/pricing', prices),
  logBodies: () => request<{ enabled: boolean }>('GET', '/api/settings/log-bodies'),
  setLogBodies: (enabled: boolean) =>
    request<{ enabled: boolean }>('PUT', '/api/settings/log-bodies', { enabled }),
  globalLimits: () => request<GlobalLimits>('GET', '/api/settings/limits'),
  setGlobalLimits: (limits: GlobalLimits) =>
    request<GlobalLimits>('PUT', '/api/settings/limits', limits),
  ipLimits: () => request<IpLimits>('GET', '/api/settings/ip-limits'),
  setIpLimits: (limits: IpLimits) => request<IpLimits>('PUT', '/api/settings/ip-limits', limits),
  emptyResponseRetry: () =>
    request<EmptyResponseRetryPolicy>('GET', '/api/settings/empty-response-retry'),
  setEmptyResponseRetry: (policy: EmptyResponseRetryPolicy) =>
    request<EmptyResponseRetryPolicy>('PUT', '/api/settings/empty-response-retry', policy),
  notify: () => request<NotifySettings>('GET', '/api/settings/notify'),
  setNotify: (settings: NotifySettings) =>
    request<NotifySettings>('PUT', '/api/settings/notify', settings),
  testNotify: () => request<{ sent: boolean }>('POST', '/api/settings/notify/test'),
  /** Webhook 签名密钥；只回是否已配置，不回明文。传 null 清除。 */
  webhookSecret: () => request<SecretConfigured>('GET', '/api/settings/webhook-secret'),
  setWebhookSecret: (secret: string | null) =>
    request<SecretConfigured>('PUT', '/api/settings/webhook-secret', { secret }),
  /** 渠道亲和性设置；缺失时后端返回全默认（功能关闭）。 */
  affinity: () => request<AffinitySettings>('GET', '/api/settings/affinity'),
  setAffinity: (settings: AffinitySettings) =>
    request<AffinitySettings>('PUT', '/api/settings/affinity', settings),
  /** 清空已建立的绑定缓存；返回被清除的条目数。 */
  clearAffinity: () => request<{ cleared: number }>('POST', '/api/settings/affinity/clear'),
  /** 命中/未命中/记录/遗忘次数与活跃绑定数。 */
  affinityStats: () => request<AffinityStatsResponse>('GET', '/api/settings/affinity/stats'),
  /** 自动备份设置：目录、间隔（0 关闭）、保留份数。 */
  backupSettings: () => request<BackupSettings>('GET', '/api/settings/backup'),
  setBackupSettings: (settings: BackupSettings) =>
    request<BackupSettings>('PUT', '/api/settings/backup', settings),
  /** 凭据静态加密的主密钥；只回是否已配置，不回明文。传 null 清除。 */
  masterKey: () => request<SecretConfigured>('GET', '/api/settings/master-key'),
  setMasterKey: (key: string | null) =>
    request<SecretConfigured>('PUT', '/api/settings/master-key', { key }),
  /** 传 null 关闭管理鉴权。设置后无法读回，只能覆盖或清除。 */
  setAdminToken: (token: string | null) =>
    request<{ configured: boolean }>('PUT', '/api/settings/admin-token', { token }),
}

/**
 * 管理身份与会话认证。
 *
 * 采用 HttpOnly Session Cookie 进行会话保持，前端 JS 不在本地持久化明文令牌。
 */
export const auth = {
  session: () =>
    request<{ authenticated: boolean; configured: boolean; username: string | null }>(
      'GET',
      '/api/auth/session',
    ),
  login: (token: string) =>
    request<{ authenticated: boolean; username: string }>('POST', '/api/auth/login', { token }),
  logout: () => request<{ authenticated: boolean }>('POST', '/api/auth/logout'),
}
/**
 * 备份文件管理（自动备份与手动备份的产物）。
 *
 * 与 `data.backup`（在线 VACUUM INTO 热备、直接吐文件）不同：这里管理的是
 * 已落盘的备份文件列表 —— 可以按需下载或删除某一份。
 */
export const backups = {
  list: () => request<BackupFile[]>('GET', '/api/backups'),
  /** 立即生成一份备份。 */
  create: () => request<{ name: string }>('POST', '/api/backups'),
  /** 带管理令牌的下载；文件名由服务端 content-disposition 给出。 */
  download: (name: string) => download(`/api/backups/${encodeURIComponent(name)}`, name),
  remove: (name: string) =>
    request<{ deleted: boolean }>('DELETE', `/api/backups/${encodeURIComponent(name)}`),
}

/** 渠道健康度与熔断。 */
export const data = {
  stats: () =>
    request<{ db_bytes: number; log_rows: number; oldest_log_at: string | null }>(
      'GET',
      '/api/data/stats',
    ),
  /** 在线备份（带鉴权下载，VACUUM INTO 产物）。 */
  backup: () => download('/api/data/backup', 'refract-backup.db'),
}

export const health = {
  channels: () => request<EndpointHealth[]>('GET', '/api/health/channels'),
  reset: (channelId: number, protocol: Protocol) =>
    request<{ reset: number; protocol: Protocol }>(
      'POST',
      `/api/health/channels/${channelId}/${protocol}/reset`,
    ),
}

/** 派生的可用模型清单。 */
export const models = {
  list: () => request<string[]>('GET', '/api/models'),
}

/**
 * 模型调试台。
 *
 * 不走统一的 `request()`：流式响应需要调用方直接消费 ReadableStream，
 * JSON 拆包在这里没有意义。鉴权失败仍派发全局事件，让令牌弹窗出现。
 */
export const playground = {
  chat: (body: Record<string, unknown>, signal?: AbortSignal): Promise<Response> => {
    const headers: Record<string, string> = { 'content-type': 'application/json' }
    return fetch('/api/playground/chat', {
      method: 'POST',
      headers,
      credentials: 'same-origin',
      body: JSON.stringify(body),
      signal,
    }).then((response) => {
      if (response.status === 401 || response.status === 403) {
        window.dispatchEvent(new CustomEvent(AUTH_REQUIRED_EVENT))
      }
      return response
    })
  },
}

/** 导入结果统计。 */
export interface ImportResult {
  channels_imported: number
  channels_skipped: number
  keys_imported: number
  keys_skipped: number
  /** 因同名/同哈希被跳过的名单 —— 用户要知道的是「哪些没进来」。 */
  skipped_channels: string[]
  skipped_keys: string[]
}

/**
 * 配置备份。
 *
 * 导出文档在前端是不透明的：内容形状由后端定义并演进（带 version 字段），
 * 前端只负责下载与回传，不应该对其中的字段做任何假设。
 */
export const backup = {
  export: () => request<Record<string, unknown>>('GET', '/api/export'),
  import: (data: unknown, mode: 'merge' | 'replace') =>
    request<ImportResult>('POST', '/api/import', { mode, data }),
}
