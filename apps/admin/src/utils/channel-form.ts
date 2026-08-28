/**
 * 渠道编辑器的空白表单与地址预览。
 *
 * `blankChannel()` 必须带一个 chat 端点种子：模型输入框渲染在端点循环里，
 * 空 `endpoints` 会让 e2e「Chat 模型输入」找不到。
 */
import type { Channel, ChannelEndpoint, Protocol, UpstreamAddress } from '@refract/contracts'

/** 协议默认地址段，用作输入框 placeholder —— 让用户知道留空会得到什么。 */
export const PROTO_DEFAULTS: Record<Protocol, { base: string; prefix: string; path: string }> = {
  chat: { base: 'https://api.openai.com', prefix: '/v1', path: '/chat/completions' },
  responses: { base: 'https://api.openai.com', prefix: '/v1', path: '/responses' },
  messages: { base: 'https://api.anthropic.com', prefix: '/v1', path: '/messages' },
  gemini: {
    base: 'https://generativelanguage.googleapis.com',
    prefix: '/v1beta',
    path: '/models/{model}:{action}',
  },
}

export function emptyAddress(): UpstreamAddress {
  return {
    unofficial: false,
    full_address: false,
    base_url: null,
    version_prefix: null,
    path: null,
  }
}

export function newEndpoint(protocol: Protocol, order = 0): ChannelEndpoint {
  return {
    protocol,
    order,
    enabled: true,
    address: emptyAddress(),
    credential: null,
    models: [],
    transcode: { enabled: false, accepted: [] },
  }
}

export function blankChannel(): Channel {
  return {
    id: 0,
    owner_id: 1,
    name: '',
    kind: 'chat',
    enabled: true,
    priority: 0,
    weight: 1,
    credential: '',
    credentials: [],
    key_strategy: 'round_robin',
    endpoints: [newEndpoint('chat')],
    address: emptyAddress(),
    tags: [],
    timeout_secs: 0,
    proxy: null,
    param_override: null,
    note: null,
    empty_response_retry: { window_secs: null, max_retries: null },
    visibility: 'shared',
    user_id: null,
  }
}

/**
 * 值是否是后端返回的脱敏占位符（`sk-a…9f2c` / `••••`）。
 * 真实密钥是 ASCII，这两个字符只可能来自掩码。
 */
export function looksMasked(value: string | null | undefined): boolean {
  return !!value && (value.includes('…') || value.includes('•'))
}

/** 端点是否自定义了地址 —— 全空即继承渠道默认。 */
export function hasOwnAddress(ep: ChannelEndpoint): boolean {
  const a = ep.address
  return a.unofficial || a.full_address || !!a.base_url || !!a.version_prefix || !!a.path
}

export function parseAndAddModels(ep: ChannelEndpoint, rawText: string): void {
  const trimmed = rawText.trim()
  if (!trimmed) return

  const items = trimmed.split(/[\n,，;； \t]+/).filter(Boolean)
  const existing = new Set(ep.models.map((m) => m.name))

  for (const item of items) {
    const [name, upstream] = item.includes('=') ? item.split('=', 2) : [item, undefined]
    const trimmedName = name!.trim()
    if (!trimmedName || existing.has(trimmedName)) continue
    ep.models.push({ name: trimmedName, upstream: upstream?.trim() || null })
    existing.add(trimmedName)
  }
}

/**
 * 与后端 `join_segments` 相同的拼接语义：base 去尾斜杠，
 * 每段 trim、空段跳过、缺前导斜杠则补、尾斜杠去掉。
 */
export function joinSegments(base: string, segments: (string | null | undefined)[]): string {
  let out = base.replace(/\/+$/, '')
  for (const raw of segments) {
    const seg = raw?.trim()
    if (!seg) continue
    if (!seg.startsWith('/')) out += '/'
    out += seg.replace(/\/+$/, '')
  }
  return out
}

export function previewUrl(ep: ChannelEndpoint, channelAddress: UpstreamAddress): string {
  const a = hasOwnAddress(ep) ? ep.address : channelAddress
  const d = PROTO_DEFAULTS[ep.protocol]
  if (!a.unofficial) return joinSegments(d.base, [d.prefix, d.path])
  if (a.full_address) return a.base_url?.trim() || '（未填完整地址）'
  const base = a.base_url?.trim()
  if (!base) return '（未填 base URL）'
  return joinSegments(base, [a.version_prefix?.trim() || d.prefix, a.path?.trim() || d.path])
}

export function poolCredentials(credentialsText: string): string[] {
  return credentialsText
    .split('\n')
    .map((line) => line.trim())
    .filter(Boolean)
}
