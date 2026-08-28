import { ref, computed } from 'vue'
import { defineStore } from 'pinia'
import { type ApiScope, scopedLogs } from '@/api/client'
import type { ChannelStat, ModelStat, StatsSummary, TimeBucket } from '@refract/contracts'
import { isSuccess, settled } from '@/utils/effect'
import { toErrorMessage } from './shared'

/**
 * 仪表盘统计：请求量、令牌数、成功率，以及按模型拆分的明细。
 * 两个数据源相互独立，所以分开的 fetch* 动作各自维护 loading，
 * 刷新时并行拉取而不是串行等待。
 */
export function useDashboardStore(scope: ApiScope = 'admin') {
  return defineStore(`dashboard:${scope}`, () => {
    const api = scopedLogs(scope)
    const summary = ref<StatsSummary | null>(null)
    const byModel = ref<ModelStat[]>([])
    const byChannel = ref<ChannelStat[]>([])
    const timeseries = ref<TimeBucket[]>([])
    const inflight = ref(0)
    const loading = computed(() => inflight.value > 0)
    const error = ref<string | null>(null)
    const hours = ref(24)

    const summarySeq = { n: 0 }
    const modelSeq = { n: 0 }
    const channelSeq = { n: 0 }
    const seriesSeq = { n: 0 }

    const successRate = computed(() => {
      const total = summary.value?.requests ?? 0
      return total === 0 ? 1 : (total - (summary.value?.failures ?? 0)) / total
    })

    const totalTokens = computed(
      () => (summary.value?.input_tokens ?? 0) + (summary.value?.output_tokens ?? 0),
    )

    async function fetchLatest<T>(
      seq: { n: number },
      task: () => Promise<T>,
      apply: (data: T) => void,
    ) {
      const token = ++seq.n
      inflight.value += 1
      error.value = null
      const outcome = await settled(task)
      inflight.value -= 1
      if (token !== seq.n) return
      if (isSuccess(outcome)) apply(outcome.success)
      else error.value = toErrorMessage(outcome.failure)
    }

    async function fetchSummary(windowHours?: number) {
      if (windowHours !== undefined) hours.value = windowHours
      await fetchLatest(
        summarySeq,
        () => api.summary(hours.value),
        (data) => {
          summary.value = data
        },
      )
    }

    async function fetchByModel(windowHours?: number) {
      if (windowHours !== undefined) hours.value = windowHours
      await fetchLatest(
        modelSeq,
        () => api.byModel(hours.value),
        (data) => {
          byModel.value = data
        },
      )
    }

    async function fetchByChannel(windowHours?: number) {
      if (windowHours !== undefined) hours.value = windowHours
      await fetchLatest(
        channelSeq,
        () => api.byChannel(hours.value),
        (data) => {
          byChannel.value = data
        },
      )
    }

    async function fetchTimeseries(windowHours?: number) {
      if (windowHours !== undefined) hours.value = windowHours
      const bucket = hours.value > 48 ? 'day' : 'hour'
      await fetchLatest(
        seriesSeq,
        () => api.timeseries(hours.value, bucket),
        (data) => {
          timeseries.value = data
        },
      )
    }

    async function refresh(windowHours?: number) {
      const tasks = [
        fetchSummary(windowHours),
        fetchByModel(windowHours),
        fetchTimeseries(windowHours),
      ]
      if (scope === 'admin') tasks.push(fetchByChannel(windowHours))
      await Promise.all(tasks)
    }

    return {
      summary,
      byModel,
      byChannel,
      timeseries,
      loading,
      error,
      hours,
      successRate,
      totalTokens,
      fetchSummary,
      fetchByModel,
      fetchByChannel,
      fetchTimeseries,
      refresh,
    }
  })()
}
