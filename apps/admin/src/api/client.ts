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
  BreakerPolicy,
  Channel,
  ChannelStat,
  ChannelTestResult,
  CreatedApiKey,
  EmptyResponseRetryPolicy,
  EndpointHealth,
  GlobalLimits,
  ImportResult,
  IpLimits,
  KeyUsageStat,
  LedgerEntry,
  LedgerKind,
  LogFilter,
  LogRetentionSetting,
  LoginResponse,
  ModelPrice,
  ModelStat,
  NewApiKey,
  NotifySettings,
  ProbeResult,
  Protocol,
  RegisterResponse,
  RequestLog,
  RoutingPolicy,
  SecretConfigured,
  SessionResponse,
  SessionUser,
  StatsSummary,
  TimeBucket,
  UpstreamAddress,
  User,
  UserListItem,
  UserRole,
  UserStatus,
  Wallet,
} from '@refract/contracts'
import * as m from '@/paraglide/messages'
import { encryptPayload } from './crypto'
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
  const message = error instanceof Error && error.message ? error.message : m.err_network()
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

function httpFailure(
  response: Response,
  path: string,
): Effect<never, ApiError | BackendUnavailable> {
  return gen(function* () {
    const skipAuthEvent = path.startsWith('/api/auth/') || path === '/api/me/password'
    if (response.status === 401 && !skipAuthEvent) {
      announce(AUTH_REQUIRED_EVENT)
    }

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

    if (!response.ok) return yield* httpFailure(response, path)
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
    if (!response.ok) return yield* httpFailure(response, path)

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

/** 管理区走 `/api/admin`；自助区走 `/api/me`。 */
export type ApiScope = 'admin' | 'me'

export function scopePrefix(scope: ApiScope): string {
  return scope === 'me' ? '/api/me' : '/api/admin'
}

function makeChannelsApi(base: string) {
  return {
    list: () => request<Channel[]>('GET', `${base}/channels`),
    get: (id: number) => request<Channel>('GET', `${base}/channels/${id}`),
    create: (channel: Channel) => request<Channel>('POST', `${base}/channels`, channel),
    update: (channel: Channel) =>
      request<Channel>('PUT', `${base}/channels/${channel.id}`, channel),
    remove: (id: number) => request<{ deleted: number }>('DELETE', `${base}/channels/${id}`),
    setEnabled: (id: number, enabled: boolean) =>
      request<{ id: number; enabled: boolean }>('POST', `${base}/channels/${id}/enabled`, {
        enabled,
      }),
    probe: (id: number, protocol?: Protocol) =>
      request<ProbeResult>('POST', `${base}/channels/${id}/probe`, { protocol: protocol ?? null }),
    probeDirect: (spec: {
      protocol: Protocol
      address?: UpstreamAddress
      credential?: string | null
      proxy?: string | null
    }) => request<ProbeResult>('POST', `${base}/channels/probe-direct`, spec),
    test: (id: number, protocol?: Protocol, model?: string) =>
      request<ChannelTestResult>('POST', `${base}/channels/${id}/test`, {
        protocol: protocol ?? null,
        model: model ?? null,
      }),
    duplicate: (id: number) => request<Channel>('POST', `${base}/channels/${id}/duplicate`),
    bulk: (ids: number[], action: 'enable' | 'disable' | 'delete') =>
      request<{ affected: number }>('POST', `${base}/channels/bulk`, { ids, action }),
    balance: (id: number) =>
      request<{ id: number; balance: number }>('POST', `${base}/channels/${id}/balance`),
  }
}

function makeKeysApi(base: string) {
  return {
    list: () => request<ApiKey[]>('GET', `${base}/keys`),
    create: (spec: NewApiKey) => request<CreatedApiKey>('POST', `${base}/keys`, spec),
    update: (id: number, spec: NewApiKey) => request<ApiKey>('PUT', `${base}/keys/${id}`, spec),
    resetUsage: (id: number) =>
      request<{ id: number; used_quota: number }>('POST', `${base}/keys/${id}/reset-usage`),
    remove: (id: number) => request<{ deleted: number }>('DELETE', `${base}/keys/${id}`),
    setEnabled: (id: number, enabled: boolean) =>
      request<{ id: number; enabled: boolean }>('POST', `${base}/keys/${id}/enabled`, { enabled }),
  }
}

function makeLogsApi(base: string) {
  return {
    query: (filter: LogFilter = {}) => request<RequestLog[]>('GET', `${base}/logs${query(filter)}`),
    get: (id: number) => request<RequestLog>('GET', `${base}/logs/${id}`),
    prune: (days: number) => request<{ removed: number }>('POST', `${base}/logs/prune`, { days }),
    summary: (hours = 24) => request<StatsSummary>('GET', `${base}/stats${query({ hours })}`),
    byModel: (hours = 24) => request<ModelStat[]>('GET', `${base}/stats/models${query({ hours })}`),
    byKey: (hours = 24) => request<KeyUsageStat[]>('GET', `${base}/stats/keys${query({ hours })}`),
    byChannel: (hours = 24) =>
      request<ChannelStat[]>('GET', `${base}/stats/channels${query({ hours })}`),
    timeseries: (hours = 24, bucket: 'hour' | 'day' = 'hour') =>
      request<TimeBucket[]>('GET', `${base}/stats/timeseries${query({ hours, bucket })}`),
    export: (filter: LogFilter = {}) =>
      download(`${base}/logs/export${query(filter)}`, 'refract-logs.ndjson'),
  }
}

/** 渠道管理（管理区）。 */
export const channels = makeChannelsApi('/api/admin')
/** 网关自身的 API 密钥（管理区）。 */
export const keys = makeKeysApi('/api/admin')
/** 请求日志与统计（管理区）。 */
export const logs = makeLogsApi('/api/admin')

export function scopedChannels(scope: ApiScope) {
  return makeChannelsApi(scopePrefix(scope))
}
export function scopedKeys(scope: ApiScope) {
  return makeKeysApi(scopePrefix(scope))
}
export function scopedLogs(scope: ApiScope) {
  return makeLogsApi(scopePrefix(scope))
}

/** 运行时设置。 */
export const settings = {
  routingPolicy: () => request<RoutingPolicy>('GET', '/api/admin/settings/routing'),
  setRoutingPolicy: (policy: RoutingPolicy) =>
    request<RoutingPolicy>('PUT', '/api/admin/settings/routing', policy),
  logRetention: () => request<LogRetentionSetting>('GET', '/api/admin/settings/log-retention'),
  setLogRetention: (days: number) =>
    request<LogRetentionSetting>('PUT', '/api/admin/settings/log-retention', { days }),
  breakerPolicy: () => request<BreakerPolicy>('GET', '/api/admin/settings/breaker'),
  setBreakerPolicy: (policy: BreakerPolicy) =>
    request<BreakerPolicy>('PUT', '/api/admin/settings/breaker', policy),
  pricing: () => request<ModelPrice[]>('GET', '/api/admin/settings/pricing'),
  setPricing: (prices: ModelPrice[]) =>
    request<ModelPrice[]>('PUT', '/api/admin/settings/pricing', prices),
  logBodies: () => request<{ enabled: boolean }>('GET', '/api/admin/settings/log-bodies'),
  setLogBodies: (enabled: boolean) =>
    request<{ enabled: boolean }>('PUT', '/api/admin/settings/log-bodies', { enabled }),
  globalLimits: () => request<GlobalLimits>('GET', '/api/admin/settings/limits'),
  setGlobalLimits: (limits: GlobalLimits) =>
    request<GlobalLimits>('PUT', '/api/admin/settings/limits', limits),
  ipLimits: () => request<IpLimits>('GET', '/api/admin/settings/ip-limits'),
  setIpLimits: (limits: IpLimits) =>
    request<IpLimits>('PUT', '/api/admin/settings/ip-limits', limits),
  emptyResponseRetry: () =>
    request<EmptyResponseRetryPolicy>('GET', '/api/admin/settings/empty-response-retry'),
  setEmptyResponseRetry: (policy: EmptyResponseRetryPolicy) =>
    request<EmptyResponseRetryPolicy>('PUT', '/api/admin/settings/empty-response-retry', policy),
  notify: () => request<NotifySettings>('GET', '/api/admin/settings/notify'),
  setNotify: (settings: NotifySettings) =>
    request<NotifySettings>('PUT', '/api/admin/settings/notify', settings),
  testNotify: () => request<{ sent: boolean }>('POST', '/api/admin/settings/notify/test'),
  webhookSecret: () => request<SecretConfigured>('GET', '/api/admin/settings/webhook-secret'),
  setWebhookSecret: (secret: string | null) =>
    request<SecretConfigured>('PUT', '/api/admin/settings/webhook-secret', { secret }),
  affinity: () => request<AffinitySettings>('GET', '/api/admin/settings/affinity'),
  setAffinity: (settings: AffinitySettings) =>
    request<AffinitySettings>('PUT', '/api/admin/settings/affinity', settings),
  clearAffinity: () => request<{ cleared: number }>('POST', '/api/admin/settings/affinity/clear'),
  affinityStats: () => request<AffinityStatsResponse>('GET', '/api/admin/settings/affinity/stats'),
  backupSettings: () => request<BackupSettings>('GET', '/api/admin/settings/backup'),
  setBackupSettings: (settings: BackupSettings) =>
    request<BackupSettings>('PUT', '/api/admin/settings/backup', settings),
  masterKey: () => request<SecretConfigured>('GET', '/api/admin/settings/master-key'),
  setMasterKey: (key: string | null) =>
    request<SecretConfigured>('PUT', '/api/admin/settings/master-key', { key }),
  setAdminToken: (token: string | null) =>
    request<{ configured: boolean }>('PUT', '/api/admin/settings/admin-token', { token }),
}

/**
 * 身份与会话认证。
 *
 * 采用 HttpOnly Session Cookie 进行会话保持，前端 JS 不在本地持久化明文令牌。
 */
export const auth = {
  session: () => request<SessionResponse>('GET', '/api/auth/session'),
  login: (body: { token: string } | { email: string; password: string }) =>
    request<LoginResponse>('POST', '/api/auth/login', body),
  logout: () => request<{ authenticated: boolean }>('POST', '/api/auth/logout'),
  register: (body: { email: string; password: string; display_name?: string }) =>
    request<RegisterResponse>('POST', '/api/auth/register', body),
  verifyEmail: (body: { email: string; code: string }) =>
    request<{ verified: boolean }>('POST', '/api/auth/verify-email', body),
  resendVerification: (email: string) =>
    request<{ sent: boolean }>('POST', '/api/auth/resend-verification', { email }),
  requestPasswordReset: (email: string) =>
    request<{ sent: boolean }>('POST', '/api/auth/password-reset/request', { email }),
  confirmPasswordReset: (body: { email: string; code: string; new_password: string }) =>
    request<{ reset: boolean }>('POST', '/api/auth/password-reset/confirm', body),
  devCodes: (email: string) =>
    request<{ code: string | null }>('GET', `/api/auth/dev-codes${query({ email })}`),
}

export const me = {
  profile: () => request<User>('GET', '/api/me/profile'),
  updateProfile: (display_name: string) =>
    request<User>('PUT', '/api/me/profile', { display_name }),
  changePassword: (old_password: string, new_password: string) =>
    request<{ ok: boolean }>('POST', '/api/me/password', { old_password, new_password }),
  wallet: () => request<Wallet>('GET', '/api/me/wallet'),
  ledger: (
    params: {
      limit?: number
      offset?: number
      kind?: LedgerKind
      since?: string
      until?: string
    } = {},
  ) => request<LedgerEntry[]>('GET', `/api/me/wallet/ledger${query(params)}`),
  exportLedger: (format: 'csv' | 'ndjson' = 'csv') =>
    download(`/api/me/wallet/ledger/export${query({ format })}`, `refract-ledger.${format}`),
  models: () => request<string[]>('GET', '/api/me/models'),
}

export const users = {
  list: (params: { status?: UserStatus; email?: string; limit?: number; offset?: number } = {}) =>
    request<UserListItem[]>('GET', `/api/admin/users${query(params)}`),
  get: (id: number) => request<UserListItem>('GET', `/api/admin/users/${id}`),
  create: (body: {
    email: string
    password: string
    display_name?: string
    role?: UserRole
    initial_balance?: number
  }) => request<UserListItem>('POST', '/api/admin/users', body),
  update: (id: number, body: { display_name?: string; role?: UserRole; status?: UserStatus }) =>
    request<UserListItem>('PUT', `/api/admin/users/${id}`, body),
  disable: (id: number) =>
    request<{ id: number; status: UserStatus }>('POST', `/api/admin/users/${id}/disable`),
  enable: (id: number) =>
    request<{ id: number; status: UserStatus }>('POST', `/api/admin/users/${id}/enable`),
  wallet: (id: number) => request<Wallet>('GET', `/api/admin/users/${id}/wallet`),
  ledger: (id: number, params: { limit?: number; offset?: number; kind?: LedgerKind } = {}) =>
    request<LedgerEntry[]>('GET', `/api/admin/users/${id}/wallet/ledger${query(params)}`),
  topup: (id: number, amount: number, note: string) =>
    request<{ balance: number }>('POST', `/api/admin/users/${id}/wallet/topup`, { amount, note }),
  adjust: (id: number, delta: number, note: string) =>
    request<{ balance: number }>('POST', `/api/admin/users/${id}/wallet/adjust`, { delta, note }),
  refund: (id: number, amount: number, note: string) =>
    request<{ balance: number }>('POST', `/api/admin/users/${id}/wallet/refund`, { amount, note }),
}

export const backups = {
  list: () => request<BackupFile[]>('GET', '/api/admin/backups'),
  create: () => request<{ name: string }>('POST', '/api/admin/backups'),
  download: (name: string) => download(`/api/admin/backups/${encodeURIComponent(name)}`, name),
  remove: (name: string) =>
    request<{ deleted: boolean }>('DELETE', `/api/admin/backups/${encodeURIComponent(name)}`),
}

export const data = {
  stats: () =>
    request<{ db_bytes: number; log_rows: number; oldest_log_at: string | null }>(
      'GET',
      '/api/admin/data/stats',
    ),
  backup: () => download('/api/admin/data/backup', 'refract-backup.db'),
}

export const health = {
  channels: () => request<EndpointHealth[]>('GET', '/api/admin/health/channels'),
  reset: (channelId: number, protocol: Protocol) =>
    request<{ reset: number; protocol: Protocol }>(
      'POST',
      `/api/admin/health/channels/${channelId}/${protocol}/reset`,
    ),
}

export const models = {
  list: (scope: ApiScope = 'admin') => request<string[]>('GET', `${scopePrefix(scope)}/models`),
}

export const playground = {
  chat: (body: Record<string, unknown>, signal?: AbortSignal): Promise<Response> => {
    const headers: Record<string, string> = { 'content-type': 'application/json' }
    return fetch('/api/admin/playground/chat', {
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

export type { ImportResult, SessionUser }

export const backup = {
  export: () => request<Record<string, unknown>>('GET', '/api/admin/export'),
  import: (data: unknown, mode: 'merge' | 'replace') =>
    request<ImportResult>('POST', '/api/admin/import', { mode, data }),
}
