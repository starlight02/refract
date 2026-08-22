import type { Ref } from 'vue'
import { toErrorMessage } from '@/utils/error'

export { toErrorMessage }

type CaptureOptions = { rethrow?: boolean }

/**
 * 把一次异步动作的失败写入 store.error。
 *
 * - 默认再抛出：表单/弹窗需要失败原因做内联提示。
 * - `{ rethrow: false }`：查询类动作吞掉错误，只让横幅显示。
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
  const rethrow = options.rethrow !== false
  error.value = null
  try {
    return await task()
  } catch (e) {
    error.value = toErrorMessage(e)
    if (rethrow) throw e
    return undefined
  }
}

/** 查询类动作：loading 包一层，失败只写 error、不抛。 */
export async function withLoading<T>(
  loading: Ref<boolean>,
  error: Ref<string | null>,
  task: () => Promise<T>,
): Promise<T | undefined> {
  loading.value = true
  try {
    return await withStoreError(error, task, { rethrow: false })
  } finally {
    loading.value = false
  }
}
