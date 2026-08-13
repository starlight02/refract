/**
 * 后端 API 客户端。
 *
 * 设计取舍：不引入 axios/ky 之类的库。这个前端只有一个后端、只发 JSON、
 * 只需要一层错误归一化 —— 原生 fetch 加三十行封装就够了，多一个依赖
 * 反而多一份升级负担和体积。
 *
 * 路径与方法必须与 `refract-api` 的 warp 过滤器逐字对齐；对不上时
 * 后端回的是 404/405，而前端会显示成「加载失败」，排查起来很费时间。
 */

import type {
  ApiKey,
  BreakerPolicy,
  Channel,
  ChannelTestResult,
  CreatedApiKey,
  EndpointHealth,
  KeyUsageStat,
  LogFilter,
  LogRetentionSetting,
  ModelStat,
  NewApiKey,
  ProbeResult,
  Protocol,
  RequestLog,
  RoutingPolicy,
  StatsSummary,
} from './types'

/** 后端返回的错误信封。 */
export interface ErrorEnvelope {
  code: string
  message: string
  detail?: string
}

/** API 调用失败。 */
export class ApiError extends Error {
  constructor(
    readonly status: number,
    readonly code: string,
    message: string,
    readonly detail?: string,
  ) {
    super(message)
    this.name = 'ApiError'
  }

  /** 是否是「没找到」—— UI 常需要把它和真实故障区别对待。 */
  get isNotFound(): boolean {
    return this.status === 404
  }

  /** 是否是鉴权问题，需要引导用户去填管理令牌。 */
  get isAuthError(): boolean {
    return this.status === 401 || this.status === 403
  }
}

/** 管理令牌存放的 localStorage 键。 */
const TOKEN_KEY = 'refract.admin_token'

/**
 * 管理 API 返回 401/403 时派发的事件名。
 *
 * 为什么用事件而不是在每个视图里处理：令牌失效是**全局**状态 —— 任何一个
 * 请求撞见 401，都意味着本浏览器保存的令牌无效或已被服务端更换。让 App.vue
 * 统一弹出令牌输入框，比每个页面各自显示「鉴权失败」再让用户去设置页找入口
 * 少绕一大圈。
 */
export const AUTH_REQUIRED_EVENT = 'refract:auth-required'

/** 读取已保存的管理令牌。 */
export function getAdminToken(): string | null {
  return localStorage.getItem(TOKEN_KEY)
}

/** 保存管理令牌；传 null 清除。 */
export function setAdminToken(token: string | null): void {
  if (token) localStorage.setItem(TOKEN_KEY, token)
  else localStorage.removeItem(TOKEN_KEY)
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

async function request<T>(method: string, path: string, body?: unknown): Promise<T> {
  const headers: Record<string, string> = {}
  if (body !== undefined) headers['content-type'] = 'application/json'

  const token = getAdminToken()
  if (token) headers['x-admin-token'] = token

  const response = await fetch(path, {
    method,
    headers,
    body: body === undefined ? undefined : JSON.stringify(body),
  })

  if (!response.ok) {
    // 管理 API 的 401/403 值得一个全局恢复入口，而不只是让当前页面报错。
    // 先派发再抛异常：监听者（App.vue）打开令牌弹窗，调用方照常拿到错误。
    if (response.status === 401 || response.status === 403) {
      window.dispatchEvent(new CustomEvent(AUTH_REQUIRED_EVENT))
    }

    // 后端总是回 ErrorEnvelope，但网络层故障（502 网关页、连接重置）
    // 可能给出 HTML。解析失败时退回状态码文本，不能让 UI 崩在这里。
    let envelope: ErrorEnvelope
    try {
      envelope = (await response.json()) as ErrorEnvelope
    } catch {
      envelope = {
        code: 'network_error',
        message: `${response.status} ${response.statusText}`,
      }
    }
    throw new ApiError(
      response.status,
      envelope.code ?? 'unknown',
      envelope.message ?? 'request failed',
      envelope.detail,
    )
  }

  if (response.status === 204) return undefined as T
  const text = await response.text()
  if (!text) return undefined as T

  const parsed = JSON.parse(text) as Envelope<T> | T
  // 管理 API 一律带 `data`；网关 API（/v1/models）不带。两者都要支持。
  return parsed && typeof parsed === 'object' && 'data' in parsed
    ? (parsed as Envelope<T>).data
    : (parsed as T)
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
  /** 发一个最小真实请求验证渠道可用。 */
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
}

/** 网关自身的 API 密钥。 */
export const keys = {
  list: () => request<ApiKey[]>('GET', '/api/keys'),
  /** 返回值里的 `plaintext` 只在创建时出现一次，之后无法再取回。 */
  create: (spec: NewApiKey) => request<CreatedApiKey>('POST', '/api/keys', spec),
  remove: (id: number) => request<{ deleted: number }>('DELETE', `/api/keys/${id}`),
  setEnabled: (id: number, enabled: boolean) =>
    request<{ id: number; enabled: boolean }>('POST', `/api/keys/${id}/enabled`, { enabled }),
}

/** 请求日志与统计。 */
export const logs = {
  query: (filter: LogFilter = {}) => request<RequestLog[]>('GET', `/api/logs${query(filter)}`),
  prune: (days: number) => request<{ removed: number }>('POST', '/api/logs/prune', { days }),
  summary: (hours = 24) => request<StatsSummary>('GET', `/api/stats${query({ hours })}`),
  byModel: (hours = 24) => request<ModelStat[]>('GET', `/api/stats/models${query({ hours })}`),
  byKey: (hours = 24) => request<KeyUsageStat[]>('GET', `/api/stats/keys${query({ hours })}`),
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
  /** 传 null 关闭管理鉴权。设置后无法读回，只能覆盖或清除。 */
  setAdminToken: (token: string | null) =>
    request<{ configured: boolean }>('PUT', '/api/settings/admin-token', { token }),
}

/** 渠道健康度与熔断。 */
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
