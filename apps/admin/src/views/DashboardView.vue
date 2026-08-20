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
import { useDashboardStore } from '@/stores/dashboard'

const store = useDashboardStore()

/** 仪表盘是常开的监控页，静默轮询保持数字新鲜；30s 对统计类数据足够。 */
const POLL_MS = 30_000
let pollTimer: ReturnType<typeof setInterval> | null = null

/** 时间窗口选项。1 小时用于排障，7 天用于看趋势。 */
const WINDOWS = [
  { hours: 1, label: '1 小时' },
  { hours: 24, label: '24 小时' },
  { hours: 168, label: '7 天' },
] as const

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
        <h1 class="text-2xl font-semibold">仪表盘</h1>
        <p class="mt-1 text-sm text-ink-faint">
          最近 {{ WINDOWS.find((w) => w.hours === activeWindow)?.label }}的网关活动
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
        <div class="text-xs font-medium text-ink-faint uppercase">请求总数</div>
        <div class="tabular mt-2 text-3xl font-semibold">
          {{ compact(store.summary?.requests ?? 0) }}
        </div>
        <div class="mt-1 text-xs text-ink-faint">
          <span class="tabular">{{ compact(store.summary?.transcoded ?? 0) }}</span> 次协议转换 ·
          <span class="tabular">{{ percent(transcodeRate) }}</span>
        </div>
      </div>

      <div class="glass glass-specular glass-interactive p-5">
        <div class="text-xs font-medium text-ink-faint uppercase">失败</div>
        <div
          class="tabular mt-2 text-3xl font-semibold"
          :class="(store.summary?.failures ?? 0) > 0 ? 'text-danger' : ''"
        >
          {{ compact(store.summary?.failures ?? 0) }}
        </div>
        <div class="mt-1 text-xs text-ink-faint">
          失败率 <span class="tabular">{{ percent(failureRate) }}</span>
        </div>
      </div>

      <div class="glass glass-specular glass-interactive p-5">
        <div class="text-xs font-medium text-ink-faint uppercase">平均延迟</div>
        <div class="tabular mt-2 text-3xl font-semibold">
          {{ ms(store.summary?.avg_duration_ms) }}
        </div>
        <div class="mt-1 text-xs text-ink-faint">
          首字 <span class="tabular">{{ ms(store.summary?.avg_ttfb_ms) }}</span>
        </div>
      </div>

      <div class="glass glass-specular glass-interactive p-5">
        <div class="text-xs font-medium text-ink-faint uppercase">Token</div>
        <div class="tabular mt-2 text-3xl font-semibold">{{ compact(store.totalTokens) }}</div>
        <div class="mt-1 text-xs text-ink-faint">
          入 <span class="tabular">{{ compact(store.summary?.input_tokens ?? 0) }}</span> · 出
          <span class="tabular">{{ compact(store.summary?.output_tokens ?? 0) }}</span>
          <template v-if="(store.summary?.cost ?? 0) > 0">
            · <span class="tabular">${{ (store.summary?.cost ?? 0).toFixed(4) }}</span>
          </template>
        </div>
      </div>
    </div>

    <!-- 趋势 -->
    <section v-if="chart" class="glass glass-specular mb-6 p-5">
      <div class="mb-3 flex items-baseline justify-between">
        <h2 class="text-sm font-semibold text-ink-soft uppercase">趋势</h2>
        <span class="text-xs text-ink-faint">
          <span class="mr-3 inline-flex items-center gap-1.5">
            <span class="inline-block h-0.5 w-4 rounded bg-accent"></span> 请求
          </span>
          <span class="inline-flex items-center gap-1.5">
            <span class="inline-block h-0.5 w-4 rounded bg-danger"></span> 失败
          </span>
        </span>
      </div>
      <svg
        :viewBox="`0 0 640 150`"
        class="h-36 w-full"
        preserveAspectRatio="none"
        role="img"
        aria-label="请求量与失败数趋势"
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
        <span class="tabular">峰值 {{ chart.maxRequests.toLocaleString() }} 次/桶</span>
        <span class="tabular">{{ chart.last }}</span>
      </div>
    </section>

    <!-- 按模型 -->
    <section class="glass glass-specular p-5">
      <h2 class="mb-4 text-sm font-semibold text-ink-soft uppercase">按模型</h2>

      <div v-if="store.loading && sortedModels.length === 0" class="py-12 text-center">
        <GlassSpinner size="md" label="正在汇总模型数据…" />
      </div>

      <div v-else-if="sortedModels.length === 0" class="py-12 text-center">
        <div class="shape-app-icon mx-auto grid size-12 place-items-center bg-ink/6 text-ink-faint">
          <AppIcon name="gauge" :size="22" />
        </div>
        <p class="mt-3 text-sm text-ink-faint">这个时间窗口内还没有请求</p>
      </div>

      <div v-else class="overflow-x-auto" tabindex="0" aria-label="按模型统计表">
        <table class="min-w-[560px] w-full text-sm">
          <thead>
            <tr class="border-b border-ink/10 text-left text-xs text-ink-faint">
              <th class="pb-2 font-medium">模型</th>
              <th class="pb-2 text-right font-medium">请求</th>
              <th class="pb-2 text-right font-medium">输入</th>
              <th class="pb-2 text-right font-medium">输出</th>
              <th class="pb-2 text-right font-medium">花费</th>
              <th class="pb-2 text-right font-medium">首字</th>
              <th class="pb-2 text-right font-medium">总耗时</th>
              <th class="pb-2 text-right font-medium">t/s</th>
              <th class="w-32 pb-2 pl-4 font-medium">占比</th>
            </tr>
          </thead>
          <tbody>
            <tr
              v-for="m in sortedModels"
              :key="m.model"
              class="border-b border-ink/5 last:border-0 transition-colors hover:bg-ink/[0.03]"
            >
              <td class="py-2.5 font-medium">{{ m.model }}</td>
              <td class="tabular py-2.5 text-right">{{ m.requests.toLocaleString() }}</td>
              <td class="tabular py-2.5 text-right text-ink-soft">{{ compact(m.input_tokens) }}</td>
              <td class="tabular py-2.5 text-right text-ink-soft">
                {{ compact(m.output_tokens) }}
              </td>
              <td class="tabular py-2.5 text-right text-ink-soft">
                {{ m.cost > 0 ? `$${m.cost.toFixed(4)}` : '—' }}
              </td>
              <td class="tabular py-2.5 text-right text-ink-soft">{{ ms(m.avg_ttfb_ms) }}</td>
              <td class="tabular py-2.5 text-right text-ink-soft">{{ ms(m.avg_duration_ms) }}</td>
              <td class="tabular py-2.5 text-right text-ink-soft">
                {{ m.tokens_per_sec != null ? m.tokens_per_sec.toFixed(1) : '—' }}
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
                      width: `${maxModelRequests ? (m.requests / maxModelRequests) * 100 : 0}%`,
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
    <section v-if="store.byChannel.length > 0" class="glass glass-specular mt-6 p-5">
      <h2 class="mb-4 text-sm font-semibold text-ink-soft uppercase">按渠道</h2>
      <div class="overflow-x-auto" tabindex="0" aria-label="按渠道统计表">
        <table class="w-full min-w-[560px] text-sm">
          <thead>
            <tr class="border-b border-ink/10 text-left text-xs text-ink-faint">
              <th class="pb-2 font-medium">渠道</th>
              <th class="pb-2 text-right font-medium">请求</th>
              <th class="pb-2 text-right font-medium">失败</th>
              <th class="pb-2 text-right font-medium">tokens</th>
              <th class="pb-2 text-right font-medium">花费</th>
              <th class="pb-2 text-right font-medium">首字</th>
              <th class="pb-2 text-right font-medium">总耗时</th>
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
