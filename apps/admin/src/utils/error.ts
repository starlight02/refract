/**
 * 把任意拒绝值收成一句可展示的文案。
 * Error（含 ApiError）透出 message；有独立 detail 时拼上，避免只剩空壳状态码。
 */
export function toErrorMessage(e: unknown, fallback = '发生未知错误'): string {
  if (e instanceof Error && e.message) {
    const detail = 'detail' in e && typeof e.detail === 'string' ? e.detail.trim() : ''
    if (detail && detail !== e.message && !e.message.includes(detail)) {
      return `${e.message}（${detail}）`
    }
    return e.message
  }
  return fallback
}

/** 管理信封或协议信封里抽出可展示的错误。 */
export function readErrorEnvelope(
  text: string,
  status: number,
  statusText: string,
): { code: string; message: string; detail?: string } {
  let parsed: unknown
  try {
    parsed = JSON.parse(text) as unknown
  } catch {
    return { code: 'http_error', message: `${status} ${statusText}`.trim() }
  }
  if (!parsed || typeof parsed !== 'object') {
    return { code: 'http_error', message: `${status} ${statusText}`.trim() }
  }
  const root = parsed as Record<string, unknown>
  if (typeof root.message === 'string' && root.message.trim()) {
    return {
      code: typeof root.code === 'string' && root.code ? root.code : 'unknown',
      message: root.message,
      detail: typeof root.detail === 'string' ? root.detail : undefined,
    }
  }
  const nested = root.error
  if (nested && typeof nested === 'object') {
    const err = nested as Record<string, unknown>
    if (typeof err.message === 'string' && err.message.trim()) {
      const code =
        (typeof root.code === 'string' && root.code) ||
        (typeof err.type === 'string' && err.type) ||
        (typeof err.status === 'string' && err.status) ||
        'unknown'
      return { code, message: err.message }
    }
  }
  return { code: 'http_error', message: `${status} ${statusText}`.trim() }
}
