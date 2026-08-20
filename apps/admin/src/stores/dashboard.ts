import { ref, computed } from 'vue'
import { defineStore } from 'pinia'
import { logs } from '@/api/client'
import type { ChannelStat, ModelStat, StatsSummary, TimeBucket } from '@refract/contracts'
import { toErrorMessage } from './shared'

/**
 * 仪表盘统计：请求量、令牌数、成功率，以及按模型拆分的明细。
 * 两个数据源相互独立，所以分开的 fetch* 动作各自维护 loading，
 * 刷新时并行拉取而不是串行等待。
 */
export const useDashboardStore = defineStore('dashboard', () => {
  /** 汇总统计；null 表示尚未加载过（而不是「全是 0」）。 */
  const summary = ref<StatsSummary | null>(null)
  const byModel = ref<ModelStat[]>([])
  const byChannel = ref<ChannelStat[]>([])
  const timeseries = ref<TimeBucket[]>([])
  /** 在飞请求数。两个数据源并行拉取，任何一个在飞都算 loading。 */
  const inflight = ref(0)
  const loading = computed(() => inflight.value > 0)
  const error = ref<string | null>(null)
  /** 最近一次使用的时间窗口，便于页面切换时保持一致。 */
  const hours = ref(24)

  /** 各数据源的请求序号：快速切换时间窗口时，只让最新一次的结果落地。 */
  let summarySeq = 0
  let modelSeq = 0
  let channelSeq = 0
  let seriesSeq = 0

  /** 成功率 0..1；无请求时视为 1，避免仪表盘出现 NaN。 */
  const successRate = computed(() => {
    const total = summary.value?.requests ?? 0
    return total === 0 ? 1 : (total - (summary.value?.failures ?? 0)) / total
  })

  const totalTokens = computed(
    () => (summary.value?.input_tokens ?? 0) + (summary.value?.output_tokens ?? 0),
  )

  async function fetchSummary(windowHours?: number) {
    if (windowHours !== undefined) hours.value = windowHours
    const seq = ++summarySeq
    inflight.value += 1
    error.value = null
    try {
      const data = await logs.summary(hours.value)
      if (seq === summarySeq) summary.value = data
    } catch (e) {
      if (seq === summarySeq) error.value = toErrorMessage(e)
    } finally {
      inflight.value -= 1
    }
  }

  async function fetchByModel(windowHours?: number) {
    if (windowHours !== undefined) hours.value = windowHours
    const seq = ++modelSeq
    inflight.value += 1
    error.value = null
    try {
      const data = await logs.byModel(hours.value)
      if (seq === modelSeq) byModel.value = data
    } catch (e) {
      if (seq === modelSeq) error.value = toErrorMessage(e)
    } finally {
      inflight.value -= 1
    }
  }

  async function fetchByChannel(windowHours?: number) {
    if (windowHours !== undefined) hours.value = windowHours
    const seq = ++channelSeq
    inflight.value += 1
    error.value = null
    try {
      const data = await logs.byChannel(hours.value)
      if (seq === channelSeq) byChannel.value = data
    } catch (e) {
      if (seq === channelSeq) error.value = toErrorMessage(e)
    } finally {
      inflight.value -= 1
    }
  }

  async function fetchTimeseries(windowHours?: number) {
    if (windowHours !== undefined) hours.value = windowHours
    const seq = ++seriesSeq
    inflight.value += 1
    error.value = null
    try {
      // 窗口超过 48h 按天分桶，否则按小时 —— 桶数保持在可读范围。
      const bucket = hours.value > 48 ? 'day' : 'hour'
      const data = await logs.timeseries(hours.value, bucket)
      if (seq === seriesSeq) timeseries.value = data
    } catch (e) {
      if (seq === seriesSeq) error.value = toErrorMessage(e)
    } finally {
      inflight.value -= 1
    }
  }

  /** 页面挂载时调用：全部维度并行刷新。 */
  async function refresh(windowHours?: number) {
    await Promise.all([
      fetchSummary(windowHours),
      fetchByModel(windowHours),
      fetchByChannel(windowHours),
      fetchTimeseries(windowHours),
    ])
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
})
