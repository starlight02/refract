import type { Ref } from 'vue'
import { squash } from 'effect/Cause'
import { runPromiseExit, tryPromise } from 'effect/Effect'
import { isSuccess } from 'effect/Exit'
import { toErrorMessage } from '@/utils/error'

export { toErrorMessage }

type CaptureOptions = { rethrow?: boolean }

/**
 * 把一次异步动作的失败写入 store.error。
 *
 * - 默认再抛出：表单/弹窗需要失败原因做内联提示。
 * - `{ rethrow: false }`：查询类动作吞掉错误，只让横幅显示。
 *
 * 内部用 Effect 把拒绝收成 Exit 再决定去向，所以整个 store 层没有一处 try/catch；
 * 抛出时给回的是**原始**拒绝值（`Cause.squash` 保持同一性），
 * 调用方既有的 `instanceof ApiError` 判断照旧成立。
 */
export async function withStoreError<T>(
  error: Ref<string | null>,
  task: () => Promise<T>,
  options: { rethrow: false },
): Promise<T | undefined>
export async function withStoreError<T>(
  error: Ref<string | null>,
  task: () => Promise<T>,
  options?: { rethrow?: true },
): Promise<T>
export async function withStoreError<T>(
  error: Ref<string | null>,
  task: () => Promise<T>,
  options: CaptureOptions = {},
): Promise<T | undefined> {
  error.value = null

  const outcome = await runPromiseExit(tryPromise({ try: task, catch: (rejection) => rejection }))
  if (isSuccess(outcome)) return outcome.value

  const rejection = squash(outcome.cause)
  error.value = toErrorMessage(rejection)
  if (options.rethrow !== false) throw rejection
  return undefined
}

/** 查询类动作：loading 包一层，失败只写 error、不抛。 */
export async function withLoading<T>(
  loading: Ref<boolean>,
  error: Ref<string | null>,
  task: () => Promise<T>,
): Promise<T | undefined> {
  loading.value = true
  const value = await withStoreError(error, task, { rethrow: false })
  loading.value = false
  return value
}
