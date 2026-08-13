/**
 * 协议相关的共享常量与集合工具。
 *
 * 后端在内存里用 u8 位图表示协议集合，但 serde 把它序列化成协议名数组
 * （见 refract-core/src/protocol.rs，`protocol_set_serde_is_a_json_array`
 * 测试背书）。前端一律按 `Protocol[]` 处理：多一层位图转换不会让任何
 * 事情变快 —— 集合最多 4 个元素 —— 只会多一处能写错的地方。
 */

import type { Protocol } from '@/api/types'

/** 协议的稳定展示顺序，与后端 `Protocol::ALL` 保持一致。 */
export const PROTOCOL_ORDER: readonly Protocol[] = ['chat', 'responses', 'messages', 'gemini']

/** 各协议的展示名。 */
export const PROTOCOL_LABEL: Record<Protocol, string> = {
  chat: 'Chat',
  responses: 'Responses',
  messages: 'Messages',
  gemini: 'Gemini',
}

/** 协议 → Tailwind 文本色类。协议色 token 由 main.css 的 @theme 生成。 */
export const PROTOCOL_COLOR_CLASS: Record<Protocol, string> = {
  chat: 'text-proto-chat',
  responses: 'text-proto-responses',
  messages: 'text-proto-messages',
  gemini: 'text-proto-gemini',
}

/**
 * 切换集合成员，返回新数组（不原地改，便于 Vue 侦测）。
 * 结果按 PROTOCOL_ORDER 排序，这样 UI 上的顺序和提交的 JSON 都稳定。
 */
export function toggleProtocol(set: readonly Protocol[], p: Protocol): Protocol[] {
  const next = set.includes(p) ? set.filter((x) => x !== p) : [...set, p]
  return PROTOCOL_ORDER.filter((x) => next.includes(x))
}

/** 从集合中移除某个协议。用于「原生协议不能是自己的转换目标」这条约束。 */
export function withoutProtocol(set: readonly Protocol[], p: Protocol): Protocol[] {
  return set.filter((x) => x !== p)
}
