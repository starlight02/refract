import { ref } from 'vue'
import { defineStore } from 'pinia'
import { type ApiScope, scopedLogs } from '@/api/client'
import type { RequestLog, LogFilter } from '@refract/contracts'
import { isSuccess, settled } from '@/utils/effect'
import { toErrorMessage, withStoreError } from './shared'

/** 日志页默认一次取 100 条：再多滚动会卡，再少又不够一屏扫读。 */
const DEFAULT_LIMIT = 100

/**
 * 请求日志。筛选条件存在 store 里而非视图里，
 * 这样「清理旧日志」之后能用同一套条件自动重查，不必让视图重新拼参数。
 */
export function useLogsStore(scope: ApiScope = 'admin') {
  return defineStore(`logs:${scope}`, () => {
    const api = scopedLogs(scope)
    const items = ref<RequestLog[]>([])
    const loading = ref(false)
    const error = ref<string | null>(null)
    const filter = ref<LogFilter>({ limit: DEFAULT_LIMIT, offset: 0 })

    let fetchSeq = 0

    async function fetch(next?: LogFilter) {
      if (next !== undefined) filter.value = { limit: DEFAULT_LIMIT, offset: 0, ...next }
      const seq = ++fetchSeq
      loading.value = true
      error.value = null
      const outcome = await settled(() => api.query(filter.value))
      if (seq !== fetchSeq) return
      loading.value = false
      if (isSuccess(outcome)) items.value = outcome.success
      else error.value = toErrorMessage(outcome.failure)
    }

    async function prune(days: number): Promise<number> {
      const { removed } = await withStoreError(error, () => api.prune(days))
      await fetch()
      return removed
    }

    return { items, loading, error, filter, fetch, prune }
  })()
}
