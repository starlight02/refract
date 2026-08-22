/**
 * 把任意拒绝值收成一句可展示的文案。
 * Error（含 ApiError）透出 message；空消息或非 Error 用 fallback。
 */
export function toErrorMessage(e: unknown, fallback = '发生未知错误'): string {
  if (e instanceof Error && e.message) return e.message
  return fallback
}
