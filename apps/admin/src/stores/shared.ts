import { ApiError } from '@/api/client'

/**
 * 把任意异常转成适合展示的文案。
 * ApiError 自带后端语义化消息，直接透出；未知异常给出兜底文案。
 * 各 store 统一走这里，保证错误提示的语气一致。
 */
export function toErrorMessage(e: unknown): string {
  if (e instanceof ApiError) return e.message
  if (e instanceof Error) return e.message
  return '发生未知错误'
}
