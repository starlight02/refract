import { ref } from 'vue'
import { defineStore } from 'pinia'
import { logs } from '@/api/client'
import type { RequestLog, LogFilter } from '@/api/types'
import { toErrorMessage } from './shared'

/** 日志页默认一次取 100 条：再多滚动会卡，再少又不够一屏扫读。 */
const DEFAULT_LIMIT = 100

/**
 * 请求日志。筛选条件存在 store 里而非视图里，
 * 这样「清理旧日志」之后能用同一套条件自动重查，不必让视图重新拼参数。
 */
export const useLogsStore = defineStore('logs', () => {
  const items = ref<RequestLog[]>([])
  const loading = ref(false)
  const error = ref<string | null>(null)
  /** 最近一次生效的筛选条件。 */
  const filter = ref<LogFilter>({ limit: DEFAULT_LIMIT, offset: 0 })

  /**
   * 请求序号。用户连续切换筛选/翻页时会有多个查询在飞，
   * 只有最新一次的结果允许落地 —— 否则先发慢回的旧结果会覆盖新结果。
   */
  let fetchSeq = 0

  /** 传 filter 则替换当前条件；不传则沿用上次条件重查。 */
  async function fetch(next?: LogFilter) {
    if (next !== undefined) filter.value = { limit: DEFAULT_LIMIT, offset: 0, ...next }
    const seq = ++fetchSeq
    loading.value = true
    error.value = null
    try {
      const rows = await logs.query(filter.value)
      if (seq !== fetchSeq) return
      items.value = rows
    } catch (e) {
      if (seq !== fetchSeq) return
      error.value = toErrorMessage(e)
    } finally {
      if (seq === fetchSeq) loading.value = false
    }
  }

  /** 清理 N 天前的日志，返回删除条数，并按当前条件刷新列表。 */
  async function prune(days: number): Promise<number> {
    error.value = null
    try {
      const { removed } = await logs.prune(days)
      await fetch()
      return removed
    } catch (e) {
      error.value = toErrorMessage(e)
      throw e
    }
  }

  return { items, loading, error, filter, fetch, prune }
})
