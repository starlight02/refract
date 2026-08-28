<script setup lang="ts">
/**
 * 仪表盘：一屏看清网关在干什么。
 *
 * 指标的选择标准是「异常时会先在哪一个上体现」：失败率反映渠道健康，
 * 首字延迟反映上游拥塞，转换占比反映配置是否符合预期。
 */
import { computed, onMounted, onUnmounted, ref } from 'vue'
import AppIcon from '@/components/AppIcon.vue'
import GlassSpinner from '@/components/GlassSpinner.vue'
import * as m from '@/paraglide/messages'
import { useDashboardStore } from '@/stores/dashboard'
import type { ApiScope } from '@/api/client'

const props = withDefaults(defineProps<{ scope?: ApiScope }>(), { scope: 'admin' })
const store = useDashboardStore(props.scope)

/** 仪表盘是常开的监控页，静默轮询保持数字新鲜；30s 对统计类数据足够。 */
const POLL_MS = 30_000
let pollTimer: ReturnType<typeof setInterval> | null = null

/** 时间窗口选项。1 小时用于排障，7 天用于看趋势。 */
const WINDOWS = computed(() => [
  { hours: 1, label: m.dash_win_1h() },
  { hours: 24, label: m.dash_win_24h() },
  { hours: 168, label: m.dash_win_7d() },
])

const activeWindow = ref<number>(24)

async function pick(hours: number) {
  activeWindow.value = hours
  await store.refresh(hours)
}

onMounted(() => {
  store.refresh(activeWindow.value)
  pollTimer = setInterval(() => store.refresh(), POLL_MS)
})

onUnmounted(() => {
  if (pollTimer) clearInterval(pollTimer)
})

const failureRate = computed(() => {
  const total = store.summary?.requests ?? 0
  if (total === 0) return 0
  return (store.summary?.failures ?? 0) / total
})

const transcodeRate = computed(() => {
  const total = store.summary?.requests ?? 0
  if (total === 0) return 0
  return (store.summary?.transcoded ?? 0) / total
})

/** 大数字缩写：日志页要精确值，仪表盘要一眼看懂量级。 */
function compact(n: number): string {
  if (n < 1000) return String(n)
  if (n < 1_000_000) return `${(n / 1000).toFixed(n < 10_000 ? 1 : 0)}K`
  return `${(n / 1_000_000).toFixed(1)}M`
}

function percent(v: number): string {
  return `${(v * 100).toFixed(v < 0.1 && v > 0 ? 1 : 0)}%`
}

function ms(v: number | null | undefined): string {
  if (v === null || v === undefined) return '—'
  return v >= 1000 ? `${(v / 1000).toFixed(2)}s` : `${Math.round(v)}ms`
}

/** 按模型统计里最大的请求数，用来画占比条。 */
/** 趋势折线图的几何参数。纯 SVG 手绘 —— 一张折线图不值得引图表库。 */
const CHART_W = 640
const CHART_H = 150
const CHART_PAD = 8

interface ChartLine {
  points: string
  tone: string
}

const chart = computed(() => {
  const buckets = store.timeseries
  if (buckets.length < 2) return null
  const maxRequests = Math.max(...buckets.map((b) => b.requests), 1)
  const step = (CHART_W - CHART_PAD * 2) / (buckets.length - 1)
  const y = (value: number) =>
    CHART_H - CHART_PAD - (value / maxRequests) * (CHART_H - CHART_PAD * 2)
  const line = (pick: (b: (typeof buckets)[number]) => number): string =>
    buckets.map((b, i) => `${(CHART_PAD + i * step).toFixed(1)},${y(pick(b)).toFixed(1)}`).join(' ')
  const lines: ChartLine[] = [
    { points: line((b) => b.requests), tone: 'var(--color-accent)' },
    { points: line((b) => b.failures), tone: 'var(--color-danger)' },
  ]
  return {
    lines,
    maxRequests,
    first: buckets[0]!.bucket,
    last: buckets[buckets.length - 1]!.bucket,
    totalCost: buckets.reduce((acc, b) => acc + b.cost, 0),
  }
})

const maxModelRequests = computed(() =>
  store.byModel.reduce((max, m) => Math.max(max, m.requests), 0),
)

const sortedModels = computed(() => [...store.byModel].sort((a, b) => b.requests - a.requests))
</script>

<template>
  <div class="mx-auto max-w-6xl">
    <header class="mb-6 flex flex-wrap items-end justify-between gap-4">
      <div>
        <h1 class="text-2xl font-semibold">{{ m.dash_title() }}</h1>
        <p class="mt-1 text-sm text-ink-faint">
          {{
            m.dash_subtitle({ window: WINDOWS.find((w) => w.hours === activeWindow)?.label ?? '' })
          }}
        </p>
      </div>

      <div class="segmented-control">
        <button
          v-for="w in WINDOWS"
          :key="w.hours"
          type="button"
          class="segmented-item"
          :class="activeWindow === w.hours ? 'segmented-item-active' : ''"
          @click="pick(w.hours)"
        >
          {{ w.label }}
        </button>
      </div>
    </header>

    <p v-if="store.error" class="glass mb-4 border-danger/30 p-4 text-sm text-danger">
      {{ store.error }}
    </p>

    <!-- 四个核心指标 -->
    <div class="mb-6 grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-4">
      <div class="glass glass-specular glass-interactive p-5">
        <div class="text-xs font-medium text-ink-faint uppercase">
          {{ m.dash_total_requests() }}
        </div>
        <div class="tabular mt-2 text-3xl font-semibold">
          {{ compact(store.summary?.requests ?? 0) }}
        </div>
        <div class="mt-1 text-xs text-ink-faint">
          {{
            m.dash_transcode_stat({
              count: compact(store.summary?.transcoded ?? 0),
              rate: percent(transcodeRate),
            })
          }}
        </div>
      </div>

      <div class="glass glass-specular glass-interactive p-5">
        <div class="text-xs font-medium text-ink-faint uppercase">{{ m.dash_failures() }}</div>
        <div
          class="tabular mt-2 text-3xl font-semibold"
          :class="(store.summary?.failures ?? 0) > 0 ? 'text-danger' : ''"
        >
          {{ compact(store.summary?.failures ?? 0) }}
        </div>
        <div class="mt-1 text-xs text-ink-faint">
          {{ m.dash_failure_rate({ rate: percent(failureRate) }) }}
        </div>
      </div>

      <div class="glass glass-specular glass-interactive p-5">
        <div class="text-xs font-medium text-ink-faint uppercase">{{ m.dash_avg_duration() }}</div>
        <div class="tabular mt-2 text-3xl font-semibold">
          {{ ms(store.summary?.avg_duration_ms) }}
        </div>
        <div class="mt-1 text-xs text-ink-faint">
          {{ m.dash_first_token({ time: ms(store.summary?.avg_ttfb_ms) }) }}
        </div>
      </div>

      <div class="glass glass-specular glass-interactive p-5">
        <div class="text-xs font-medium text-ink-faint uppercase">{{ m.dash_token_usage() }}</div>
        <div class="tabular mt-2 text-3xl font-semibold">{{ compact(store.totalTokens) }}</div>
        <div class="mt-1 text-xs text-ink-faint">
          {{
            m.dash_token_in_out({
              input: compact(store.summary?.input_tokens ?? 0),
              output: compact(store.summary?.output_tokens ?? 0),
            })
          }}
          <template v-if="(store.summary?.cost ?? 0) > 0">
            · <span class="tabular">${{ (store.summary?.cost ?? 0).toFixed(4) }}</span>
          </template>
        </div>
      </div>
    </div>

    <!-- 趋势 -->
    <section v-if="chart" class="glass glass-specular mb-6 p-5">
      <div class="mb-3 flex items-baseline justify-between">
        <h2 class="text-sm font-semibold text-ink-soft uppercase">{{ m.dash_trend() }}</h2>
        <span class="text-xs text-ink-faint">
          <span class="mr-3 inline-flex items-center gap-1.5">
            <span class="inline-block h-0.5 w-4 rounded bg-accent"></span>
            {{ m.dash_trend_requests() }}
          </span>
          <span class="inline-flex items-center gap-1.5">
            <span class="inline-block h-0.5 w-4 rounded bg-danger"></span>
            {{ m.dash_trend_failures() }}
          </span>
        </span>
      </div>
      <svg
        :viewBox="`0 0 640 150`"
        class="h-36 w-full"
        preserveAspectRatio="none"
        role="img"
        :aria-label="m.dash_trend_aria()"
      >
        <polyline
          v-for="(line, i) in chart.lines"
          :key="i"
          :points="line.points"
          fill="none"
          :stroke="line.tone"
          stroke-width="2"
          stroke-linejoin="round"
          stroke-linecap="round"
          vector-effect="non-scaling-stroke"
        />
      </svg>
      <div class="mt-1.5 flex justify-between text-[0.65rem] text-ink-faint">
        <span class="tabular">{{ chart.first }}</span>
        <span class="tabular">{{
          m.dash_trend_peak({ peak: chart.maxRequests.toLocaleString() })
        }}</span>
        <span class="tabular">{{ chart.last }}</span>
      </div>
    </section>

    <!-- 按模型 -->
    <section class="glass glass-specular p-5">
      <h2 class="mb-4 text-sm font-semibold text-ink-soft uppercase">{{ m.dash_by_model() }}</h2>

      <div v-if="store.loading && sortedModels.length === 0" class="py-12 text-center">
        <GlassSpinner size="md" :label="m.dash_by_model_loading()" />
      </div>

      <div v-else-if="sortedModels.length === 0" class="py-12 text-center">
        <div class="shape-app-icon mx-auto grid size-12 place-items-center bg-ink/6 text-ink-faint">
          <AppIcon name="gauge" :size="22" />
        </div>
        <p class="mt-3 text-sm text-ink-faint">{{ m.dash_by_model_empty() }}</p>
      </div>

      <div v-else class="overflow-x-auto" tabindex="0" :aria-label="m.dash_by_model_aria()">
        <table class="min-w-[560px] w-full text-sm">
          <thead>
            <tr class="border-b border-ink/10 text-left text-xs text-ink-faint">
              <th class="pb-2 font-medium">{{ m.dash_th_model() }}</th>
              <th class="pb-2 text-right font-medium">{{ m.dash_th_requests() }}</th>
              <th class="pb-2 text-right font-medium">{{ m.dash_th_input() }}</th>
              <th class="pb-2 text-right font-medium">{{ m.dash_th_output() }}</th>
              <th class="pb-2 text-right font-medium">{{ m.dash_th_cost() }}</th>
              <th class="pb-2 text-right font-medium">{{ m.dash_th_ttfb() }}</th>
              <th class="pb-2 text-right font-medium">{{ m.dash_th_duration() }}</th>
              <th class="pb-2 text-right font-medium">t/s</th>
              <th class="w-32 pb-2 pl-4 font-medium">{{ m.dash_th_share() }}</th>
            </tr>
          </thead>
          <tbody>
            <tr
              v-for="modelItem in sortedModels"
              :key="modelItem.model"
              class="border-b border-ink/5 last:border-0 transition-colors hover:bg-ink/[0.03]"
            >
              <td class="py-2.5 font-medium">{{ modelItem.model }}</td>
              <td class="tabular py-2.5 text-right">{{ modelItem.requests.toLocaleString() }}</td>
              <td class="tabular py-2.5 text-right text-ink-soft">
                {{ compact(modelItem.input_tokens) }}
              </td>
              <td class="tabular py-2.5 text-right text-ink-soft">
                {{ compact(modelItem.output_tokens) }}
              </td>
              <td class="tabular py-2.5 text-right text-ink-soft">
                {{ modelItem.cost > 0 ? `$${modelItem.cost.toFixed(4)}` : '—' }}
              </td>
              <td class="tabular py-2.5 text-right text-ink-soft">
                {{ ms(modelItem.avg_ttfb_ms) }}
              </td>
              <td class="tabular py-2.5 text-right text-ink-soft">
                {{ ms(modelItem.avg_duration_ms) }}
              </td>
              <td class="tabular py-2.5 text-right text-ink-soft">
                {{ modelItem.tokens_per_sec != null ? modelItem.tokens_per_sec.toFixed(1) : '—' }}
              </td>
              <td class="py-2.5 pl-4">
                <div class="h-1.5 overflow-hidden rounded-full bg-ink/8">
                  <div
                    class="h-full rounded-full transition-[width] duration-500"
                    style="
                      background: linear-gradient(
                        90deg,
                        var(--color-accent-soft),
                        var(--color-accent)
                      );
                    "
                    :style="{
                      width: `${maxModelRequests ? (modelItem.requests / maxModelRequests) * 100 : 0}%`,
                    }"
                  />
                </div>
              </td>
            </tr>
          </tbody>
        </table>
      </div>
    </section>

    <!-- 按渠道 -->
    <section
      v-if="scope === 'admin' && store.byChannel.length > 0"
      class="glass glass-specular mt-6 p-5"
    >
      <h2 class="mb-4 text-sm font-semibold text-ink-soft uppercase">{{ m.dash_by_channel() }}</h2>
      <div class="overflow-x-auto" tabindex="0" :aria-label="m.dash_by_channel_aria()">
        <table class="w-full min-w-[560px] text-sm">
          <thead>
            <tr class="border-b border-ink/10 text-left text-xs text-ink-faint">
              <th class="pb-2 font-medium">{{ m.dash_th_channel() }}</th>
              <th class="pb-2 text-right font-medium">{{ m.dash_th_requests() }}</th>
              <th class="pb-2 text-right font-medium">{{ m.dash_th_failures() }}</th>
              <th class="pb-2 text-right font-medium">tokens</th>
              <th class="pb-2 text-right font-medium">{{ m.dash_th_cost() }}</th>
              <th class="pb-2 text-right font-medium">{{ m.dash_th_ttfb() }}</th>
              <th class="pb-2 text-right font-medium">{{ m.dash_th_duration() }}</th>
            </tr>
          </thead>
          <tbody>
            <tr
              v-for="c in store.byChannel"
              :key="`${c.channel_id}-${c.channel_name}`"
              class="border-b border-ink/5 transition-colors last:border-0 hover:bg-ink/[0.03]"
            >
              <td class="py-2.5 font-medium">{{ c.channel_name }}</td>
              <td class="tabular py-2.5 text-right">{{ c.requests.toLocaleString() }}</td>
              <td
                class="tabular py-2.5 text-right"
                :class="c.failures > 0 ? 'text-danger' : 'text-ink-soft'"
              >
                {{ c.failures.toLocaleString() }}
              </td>
              <td class="tabular py-2.5 text-right text-ink-soft">
                {{ compact(c.input_tokens + c.output_tokens) }}
              </td>
              <td class="tabular py-2.5 text-right text-ink-soft">
                {{ c.cost > 0 ? `$${c.cost.toFixed(4)}` : '—' }}
              </td>
              <td class="tabular py-2.5 text-right text-ink-soft">{{ ms(c.avg_ttfb_ms) }}</td>
              <td class="tabular py-2.5 text-right text-ink-soft">{{ ms(c.avg_duration_ms) }}</td>
            </tr>
          </tbody>
        </table>
      </div>
    </section>
  </div>
</template>
