<script setup lang="ts">
/**
 * 请求日志。
 *
 * 这一页存在的唯一理由是排障，所以默认列出的字段全部围绕「为什么这条请求
 * 是这个结果」：入站协议 → 上游协议（是否转换）、命中的渠道、重试次数、
 * 首字延迟。token 数放在后面，因为算账是次要目的。
 *
 * 展开行才显示错误详情：错误信息通常很长，平铺会把表格撑烂。
 */
import { computed, onMounted, onUnmounted, ref, watch } from 'vue'
import ProtocolBadge from '@/components/ProtocolBadge.vue'
import AppIcon from '@/components/AppIcon.vue'
import { useLogsStore } from '@/stores/logs'
import { useChannelsStore } from '@/stores/channels'
import { logs as logsApi } from '@/api/client'
import {
  DialogContent,
  DialogDescription,
  DialogOverlay,
  DialogPortal,
  DialogRoot,
  DialogTitle,
} from 'reka-ui'
import type { Protocol, RequestLog } from '@/api/types'

const store = useLogsStore()
const channelsStore = useChannelsStore()

// ── 自动刷新 ──
// 排障时最常见的动作是「重发请求 → 切回来看日志」，自动刷新省掉手动刷新。
// 固定 5 秒：再快后端压力大，再慢不如手动。离开页面时必须停表。

const AUTO_REFRESH_MS = 5_000
const autoRefresh = ref(false)
let refreshTimer: ReturnType<typeof setInterval> | null = null

watch(autoRefresh, (on) => {
  if (refreshTimer) {
    clearInterval(refreshTimer)
    refreshTimer = null
  }
  if (on) refreshTimer = setInterval(() => store.fetch(), AUTO_REFRESH_MS)
})

onUnmounted(() => {
  if (refreshTimer) clearInterval(refreshTimer)
})

/** 筛选条件的本地草稿，点「应用」才提交 —— 避免每敲一个字符打一次后端。 */
const draftModel = ref('')
const draftChannel = ref<number | ''>('')
const draftFailuresOnly = ref(false)
const draftRequestId = ref('')
const draftSince = ref('')
const draftUntil = ref('')
const timePreset = ref<'all' | '1h' | '6h' | '24h' | '7d' | 'custom'>('all')

function pad2(n: number) {
  return String(n).padStart(2, '0')
}

function toLocalInputString(d: Date): string {
  return `${d.getFullYear()}-${pad2(d.getMonth() + 1)}-${pad2(d.getDate())}T${pad2(d.getHours())}:${pad2(d.getMinutes())}`
}

function onTimePresetChange(val: string) {
  timePreset.value = val as typeof timePreset.value
  const now = Date.now()
  if (val === 'all') {
    draftSince.value = ''
    draftUntil.value = ''
  } else if (val === '1h') {
    draftSince.value = toLocalInputString(new Date(now - 3600 * 1000))
    draftUntil.value = toLocalInputString(new Date(now))
  } else if (val === '6h') {
    draftSince.value = toLocalInputString(new Date(now - 6 * 3600 * 1000))
    draftUntil.value = toLocalInputString(new Date(now))
  } else if (val === '24h') {
    draftSince.value = toLocalInputString(new Date(now - 24 * 3600 * 1000))
    draftUntil.value = toLocalInputString(new Date(now))
  } else if (val === '7d') {
    draftSince.value = toLocalInputString(new Date(now - 7 * 24 * 3600 * 1000))
    draftUntil.value = toLocalInputString(new Date(now))
  }
}

/** datetime-local（本地时区）转后端要的 UTC 秒级时间串。 */
function toUtcStamp(local: string): string | undefined {
  if (!local) return undefined
  const date = new Date(local)
  if (Number.isNaN(date.getTime())) return undefined
  return date.toISOString().replace('T', ' ').slice(0, 19)
}
/** 展开的行 id 集合。 */
const expanded = ref<Set<number>>(new Set())
/** 清理确认与结果提示。 */
const pruneDays = ref(30)
const pruning = ref(false)
const pruneNotice = ref<string | null>(null)
/** 导出失败提示。 */
const exportNotice = ref<string | null>(null)

const PROTOCOLS: Protocol[] = ['chat', 'responses', 'messages', 'gemini']

/** 后端返回的协议名一定在这四个里，但日志是历史数据，宽松处理未知值。 */
function asProtocol(raw: string): Protocol | null {
  return (PROTOCOLS as string[]).includes(raw) ? (raw as Protocol) : null
}

onMounted(() => {
  store.fetch()
  // 渠道列表用于筛选下拉与 id→名字回填；可能已被别的页面加载过。
  if (channelsStore.items.length === 0) channelsStore.fetch()
})

function applyFilter() {
  store.fetch({
    model: draftModel.value.trim() || undefined,
    channel_id: draftChannel.value === '' ? undefined : draftChannel.value,
    request_id: draftRequestId.value.trim() || undefined,
    since: toUtcStamp(draftSince.value),
    until: toUtcStamp(draftUntil.value),
    failures_only: draftFailuresOnly.value || undefined,
  })
}

function resetFilter() {
  draftModel.value = ''
  draftChannel.value = ''
  draftFailuresOnly.value = false
  draftRequestId.value = ''
  draftSince.value = ''
  draftUntil.value = ''
  timePreset.value = 'all'
  store.fetch({})
}

/** 按当前筛选下载全量 NDJSON（上限 5 万行，服务端拼装）。 */
async function exportAll() {
  exportNotice.value = null
  try {
    await logsApi.export(store.filter)
  } catch (e) {
    exportNotice.value = e instanceof Error ? `导出失败：${e.message}` : '导出失败'
  }
}

// ── 完整请求详情 ──
// 正文可能几十 KB，列表接口从不带它；点开时按 id 单独取。
const detail = ref<RequestLog | null>(null)
const detailLoading = ref(false)
const detailError = ref<string | null>(null)
const detailOpen = computed(
  () => detailLoading.value || detail.value !== null || detailError.value !== null,
)

async function openDetail(id: number) {
  detailLoading.value = true
  detailError.value = null
  detail.value = null
  try {
    detail.value = await logsApi.get(id)
  } catch (e) {
    detailError.value = e instanceof Error ? e.message : '加载失败'
  } finally {
    detailLoading.value = false
  }
}

function closeDetail() {
  detail.value = null
  detailError.value = null
  detailLoading.value = false
}

/** 尽力美化 JSON；不是 JSON（流式聚合文本）就原样展示。 */
function pretty(raw?: string | null): string {
  if (!raw) return ''
  try {
    return JSON.stringify(JSON.parse(raw), null, 2)
  } catch {
    return raw
  }
}

function toggleRow(id: number) {
  const next = new Set(expanded.value)
  if (next.has(id)) next.delete(id)
  else next.add(id)
  expanded.value = next
}

async function prune() {
  pruning.value = true
  pruneNotice.value = null
  try {
    const removed = await store.prune(pruneDays.value)
    pruneNotice.value = `已清理 ${removed} 条`
  } catch {
    pruneNotice.value = '清理失败'
  } finally {
    pruning.value = false
  }
}

/**
 * 导出当前列表为 JSON 文件。导的是**当前筛选下的当前页** —— 所见即所得，
 * 不悄悄去后端拉一份用户没看过的全量。JSON 而不是 CSV：错误详情是长文本，
 * 且字段还会演进，无损优先。
 */
function exportLogs() {
  const blob = new Blob([JSON.stringify(store.items, null, 2)], { type: 'application/json' })
  const url = URL.createObjectURL(blob)
  const a = document.createElement('a')
  a.href = url
  a.download = `refract-logs-${new Date().toISOString().replace(/[:.]/g, '-')}.json`
  a.click()
  // 下载由浏览器异步启动，立即 revoke 可能抢在它读取 blob 之前。
  window.setTimeout(() => URL.revokeObjectURL(url), 10_000)
}

/** 翻页：limit 固定，只推 offset。 */
const pageSize = computed(() => store.filter.limit ?? 100)
const offset = computed(() => store.filter.offset ?? 0)
const hasPrev = computed(() => offset.value > 0)
/** 拿满一页就假定还有下一页 —— 后端不返回总数，这是最省的判断。 */
const hasMore = computed(() => store.items.length === pageSize.value)

function go(delta: number) {
  const next = Math.max(0, offset.value + delta * pageSize.value)
  store.fetch({ ...store.filter, offset: next })
}

function statusTone(status: number): string {
  if (status === 0) return 'text-ink-faint'
  if (status < 300) return 'text-success'
  if (status < 500) return 'text-warning'
  return 'text-danger'
}

function channelLabel(log: RequestLog): string {
  if (log.channel_name) return log.channel_name
  if (log.channel_id === null || log.channel_id === undefined) return '—'
  return channelsStore.byId.get(log.channel_id)?.name ?? `#${log.channel_id}`
}

/** 只显示时分秒 —— 日志基本都是「刚才」发生的，日期是噪音。 */
function shortTime(iso: string): string {
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return iso
  return d.toLocaleTimeString('zh-CN', { hour12: false })
}

function fullTime(iso: string): string {
  const d = new Date(iso)
  return Number.isNaN(d.getTime()) ? iso : d.toLocaleString('zh-CN', { hour12: false })
}
</script>

<template>
  <div class="flex flex-col gap-5">
    <header class="flex flex-wrap items-end justify-between gap-4">
      <div>
        <h1 class="text-2xl font-semibold">请求日志</h1>
        <p class="mt-1 text-sm text-ink-faint">点任意一行展开完整的错误与 token 明细。</p>
      </div>

      <div class="flex flex-wrap items-center gap-2.5">
        <label class="glass-pill h-[34px] cursor-pointer gap-2 px-3 text-xs text-ink-soft">
          <input v-model="autoRefresh" type="checkbox" class="accent-[var(--color-accent)]" />
          <span>自动刷新</span>
          <span v-if="autoRefresh" class="text-ink-faint">5s</span>
        </label>

        <button
          type="button"
          class="glass-button-ghost"
          :disabled="store.items.length === 0"
          @click="exportLogs"
        >
          <AppIcon name="download" :size="14" />
          导出本页
        </button>

        <button
          type="button"
          class="glass-button-ghost"
          title="按当前筛选导出 NDJSON（上限 5 万行）"
          @click="exportAll"
        >
          <AppIcon name="upload" :size="14" />
          导出全量
        </button>

        <div class="glass-pill glass-pill-danger h-[34px] gap-1.5 px-2.5">
          <span class="text-xs text-ink-soft">清理</span>
          <input
            v-model.number="pruneDays"
            type="number"
            min="1"
            class="glass-field tabular !h-[24px] !w-12 !px-1 text-center text-xs outline-none"
          />
          <span class="text-xs text-ink-soft">天前</span>
          <button
            type="button"
            class="glass-button-ghost glass-button-ghost-danger !h-[24px] !px-2 text-xs font-medium"
            :disabled="pruning"
            @click="prune"
          >
            {{ pruning ? '…' : '执行' }}
          </button>
        </div>
        <span v-if="pruneNotice" class="text-xs text-ink-faint">{{ pruneNotice }}</span>
        <span v-if="exportNotice" class="text-xs text-danger">{{ exportNotice }}</span>
      </div>
    </header>

    <!-- 筛选 -->
    <section class="glass glass-specular p-4">
      <div class="grid grid-cols-1 gap-3 sm:grid-cols-2 lg:grid-cols-4">
        <label class="flex flex-col gap-1.5">
          <span class="text-xs font-medium text-ink-soft">模型</span>
          <input
            v-model="draftModel"
            type="text"
            placeholder="精确匹配 / 全部模型"
            class="glass-field w-full outline-none"
            @keydown.enter="applyFilter"
          />
        </label>

        <label class="flex flex-col gap-1.5">
          <span class="text-xs font-medium text-ink-soft">渠道</span>
          <select v-model="draftChannel" class="glass-field w-full outline-none">
            <option value="">全部渠道</option>
            <option v-for="o in channelsStore.options" :key="o.id" :value="o.id">
              {{ o.name }}
            </option>
          </select>
        </label>

        <label class="flex flex-col gap-1.5">
          <span class="text-xs font-medium text-ink-soft">Request ID</span>
          <input
            v-model="draftRequestId"
            type="text"
            placeholder="按请求 ID 检索"
            class="glass-field w-full font-mono text-xs outline-none"
            @keydown.enter="applyFilter"
          />
        </label>

        <label class="flex flex-col gap-1.5">
          <span class="text-xs font-medium text-ink-soft">状态过滤</span>
          <select v-model="draftFailuresOnly" class="glass-field w-full outline-none">
            <option :value="false">全部状态（包含成功与失败）</option>
            <option :value="true">仅看失败请求</option>
          </select>
        </label>
      </div>

      <div
        class="mt-3 flex flex-wrap items-center justify-between gap-3 border-t border-ink/8 pt-3"
      >
        <div class="flex flex-wrap items-center gap-3">
          <label class="flex items-center gap-2">
            <span class="text-xs font-medium text-ink-soft">时间范围</span>
            <select
              :value="timePreset"
              class="glass-field w-36 outline-none"
              @change="onTimePresetChange(($event.target as HTMLSelectElement).value)"
            >
              <option value="all">全部时间</option>
              <option value="1h">最近 1 小时</option>
              <option value="6h">最近 6 小时</option>
              <option value="24h">最近 24 小时</option>
              <option value="7d">最近 7 天</option>
              <option value="custom">自定义范围…</option>
            </select>
          </label>

          <template v-if="timePreset === 'custom'">
            <div class="flex items-center gap-1.5">
              <span class="text-xs text-ink-faint">从</span>
              <input
                v-model="draftSince"
                type="datetime-local"
                aria-label="起始时间"
                class="glass-field w-48 outline-none"
              />
            </div>
            <div class="flex items-center gap-1.5">
              <span class="text-xs text-ink-faint">到</span>
              <input
                v-model="draftUntil"
                type="datetime-local"
                aria-label="截止时间"
                class="glass-field w-48 outline-none"
              />
            </div>
          </template>
        </div>

        <div class="flex items-center gap-2">
          <button type="button" class="glass-button-primary px-4 font-medium" @click="applyFilter">
            <AppIcon name="search" :size="14" />
            应用筛选
          </button>
          <button type="button" class="glass-button-ghost px-3.5" @click="resetFilter">重置</button>
        </div>
      </div>
    </section>

    <p v-if="store.error" class="glass border-danger/30 p-4 text-sm text-danger">
      {{ store.error }}
    </p>

    <!-- 表格 -->
    <section class="glass glass-specular overflow-x-auto" tabindex="0" aria-label="请求日志表">
      <div
        v-if="store.loading && store.items.length === 0"
        class="py-16 text-center text-sm text-ink-faint"
      >
        加载中…
      </div>

      <div v-else-if="store.items.length === 0" class="py-16 text-center">
        <p class="text-sm text-ink-faint">没有匹配的日志</p>
      </div>

      <table v-else class="min-w-[900px] w-full text-sm">
        <thead>
          <tr class="border-b border-ink/10 text-left text-xs text-ink-faint">
            <th class="px-4 py-2.5 font-medium">时间</th>
            <th class="px-4 py-2.5 font-medium">协议</th>
            <th class="px-4 py-2.5 font-medium">模型</th>
            <th class="px-4 py-2.5 font-medium">渠道</th>
            <th class="px-4 py-2.5 text-right font-medium">状态</th>
            <th class="px-4 py-2.5 text-right font-medium">首字</th>
            <th class="px-4 py-2.5 text-right font-medium">耗时</th>
            <th class="px-4 py-2.5 text-right font-medium">tokens</th>
          </tr>
        </thead>
        <tbody>
          <template v-for="log in store.items" :key="log.id">
            <tr
              class="cursor-pointer border-b border-ink/5 transition-colors duration-150 last:border-0 hover:bg-ink/[0.03]"
              @click="toggleRow(log.id)"
            >
              <td
                class="px-4 py-2.5 tabular whitespace-nowrap text-ink-soft"
                :title="fullTime(log.created_at)"
              >
                {{ shortTime(log.created_at) }}
              </td>

              <td class="px-4 py-2.5 whitespace-nowrap">
                <span class="inline-flex items-center gap-1">
                  <ProtocolBadge
                    v-if="asProtocol(log.inbound_protocol)"
                    :protocol="asProtocol(log.inbound_protocol)!"
                    compact
                  />
                  <span v-else class="text-xs text-ink-faint">{{ log.inbound_protocol }}</span>

                  <template v-if="log.transcoded">
                    <span class="text-ink-faint">→</span>
                    <ProtocolBadge
                      v-if="asProtocol(log.upstream_protocol)"
                      :protocol="asProtocol(log.upstream_protocol)!"
                      compact
                    />
                    <span v-else class="text-xs text-ink-faint">{{ log.upstream_protocol }}</span>
                  </template>
                </span>
              </td>

              <td class="px-4 py-2.5 font-mono text-xs whitespace-nowrap">
                {{ log.model }}
                <span v-if="log.upstream_model !== log.model" class="text-ink-faint">
                  →{{ log.upstream_model }}
                </span>
              </td>

              <td class="px-4 py-2.5 whitespace-nowrap text-ink-soft">
                <div class="flex items-center gap-1.5">
                  {{ channelLabel(log) }}
                  <span
                    v-if="log.affinity_rule"
                    class="rounded bg-accent/10 px-1.5 py-0.5 text-[10px] font-medium text-accent"
                    :title="`命中亲和规则：${log.affinity_rule}`"
                  >
                    {{ log.affinity_rule }}
                  </span>
                </div>
                <p
                  v-if="log.credential_hint"
                  class="mt-0.5 font-mono text-[10px] text-ink-faint"
                  title="本次请求实际使用的上游密钥（脱敏）"
                >
                  {{ log.credential_hint }}
                </p>
              </td>

              <td
                class="px-4 py-2.5 text-right tabular whitespace-nowrap"
                :class="statusTone(log.status)"
              >
                {{ log.status || '—' }}
                <span
                  v-if="log.retries > 0"
                  class="ml-1 inline-flex items-center gap-0.5 align-middle text-xs text-warning"
                  :title="`重试 ${log.retries} 次`"
                >
                  <AppIcon name="refresh" :size="11" />{{ log.retries }}
                </span>
              </td>

              <td class="px-4 py-2.5 text-right tabular whitespace-nowrap text-ink-soft">
                {{ log.ttfb_ms !== null && log.ttfb_ms !== undefined ? `${log.ttfb_ms}ms` : '—' }}
              </td>

              <td class="px-4 py-2.5 text-right tabular whitespace-nowrap text-ink-soft">
                {{ log.duration_ms }}ms
              </td>

              <td class="px-4 py-2.5 text-right tabular whitespace-nowrap text-ink-soft">
                {{ log.input_tokens }}<span class="text-ink-faint">/</span>{{ log.output_tokens }}
              </td>
            </tr>

            <tr
              v-if="expanded.has(log.id)"
              class="border-b border-ink/5 bg-ink/[0.02] last:border-0"
            >
              <td colspan="8" class="px-4 py-3">
                <dl class="grid grid-cols-2 gap-x-6 gap-y-1.5 text-xs sm:grid-cols-4">
                  <div>
                    <dt class="text-ink-faint">request id</dt>
                    <dd class="font-mono">{{ log.request_id }}</dd>
                  </div>
                  <div>
                    <dt class="text-ink-faint">时间</dt>
                    <dd class="tabular">{{ fullTime(log.created_at) }}</dd>
                  </div>
                  <div>
                    <dt class="text-ink-faint">流式</dt>
                    <dd>{{ log.stream ? '是' : '否' }}</dd>
                  </div>
                  <div>
                    <dt class="text-ink-faint">协议转换</dt>
                    <dd>{{ log.transcoded ? '是' : '原生' }}</dd>
                  </div>
                  <div>
                    <dt class="text-ink-faint">缓存读 tokens</dt>
                    <dd class="tabular">{{ log.cached_tokens }}</dd>
                  </div>
                  <div>
                    <dt class="text-ink-faint">缓存写 tokens</dt>
                    <dd class="tabular">{{ log.cache_write_tokens }}</dd>
                  </div>
                  <div>
                    <dt class="text-ink-faint">推理 tokens</dt>
                    <dd class="tabular">{{ log.reasoning_tokens }}</dd>
                  </div>
                  <div>
                    <dt class="text-ink-faint">首字延迟</dt>
                    <dd class="tabular">{{ log.ttfb_ms != null ? `${log.ttfb_ms}ms` : '—' }}</dd>
                  </div>
                  <div>
                    <dt class="text-ink-faint">生成耗时</dt>
                    <dd class="tabular">
                      {{ log.ttfb_ms != null ? `${log.duration_ms - log.ttfb_ms}ms` : '—' }}
                    </dd>
                  </div>
                  <div>
                    <dt class="text-ink-faint">重试</dt>
                    <dd class="tabular">{{ log.retries }}</dd>
                  </div>
                  <div>
                    <dt class="text-ink-faint">密钥</dt>
                    <dd class="tabular">{{ log.api_key_id ?? '—' }}</dd>
                  </div>
                  <div>
                    <dt class="text-ink-faint">上游密钥</dt>
                    <dd class="font-mono">{{ log.credential_hint ?? '—' }}</dd>
                  </div>
                  <div>
                    <dt class="text-ink-faint">亲和规则</dt>
                    <dd>{{ log.affinity_rule ?? '—' }}</dd>
                  </div>
                </dl>

                <div
                  v-if="log.error_kind || log.error_message"
                  class="mt-3 rounded-lg bg-danger/8 p-3"
                >
                  <p class="text-xs font-medium text-danger">{{ log.error_kind ?? '错误' }}</p>
                  <p
                    v-if="log.error_message"
                    class="mt-1 font-mono text-xs break-all text-ink-soft"
                  >
                    {{ log.error_message }}
                  </p>
                </div>

                <div class="mt-3">
                  <button
                    type="button"
                    class="glass-button-ghost px-3 py-1.5 text-xs"
                    @click.stop="openDetail(log.id)"
                  >
                    查看完整请求
                  </button>
                </div>
              </td>
            </tr>
          </template>
        </tbody>
      </table>
    </section>

    <!-- 翻页 -->
    <div v-if="store.items.length > 0" class="flex items-center justify-between text-sm">
      <span class="text-xs text-ink-faint">
        第 {{ offset + 1 }}–{{ offset + store.items.length }} 条
      </span>
      <div class="flex items-center gap-2">
        <button
          type="button"
          class="glass-button-ghost px-3 py-2 text-sm"
          :disabled="!hasPrev || store.loading"
          @click="go(-1)"
        >
          上一页
        </button>
        <button
          type="button"
          class="glass-button-ghost px-3 py-2 text-sm"
          :disabled="!hasMore || store.loading"
          @click="go(1)"
        >
          下一页
        </button>
      </div>
    </div>

    <!-- 完整请求详情弹窗 -->
    <DialogRoot :open="detailOpen" @update:open="(open) => !open && closeDetail()">
      <DialogPortal>
        <DialogOverlay
          class="fixed inset-0 z-50 bg-ink/25 backdrop-blur-sm data-[state=closed]:opacity-0 data-[state=open]:opacity-100"
        />
        <DialogContent
          class="glass-thick glass-specular fixed top-1/2 left-1/2 z-50 flex max-h-[85vh] w-[calc(100%-2rem)] max-w-3xl -translate-x-1/2 -translate-y-1/2 flex-col p-6 outline-none"
        >
          <DialogTitle class="text-lg font-semibold">完整请求</DialogTitle>
          <DialogDescription class="mt-1 font-mono text-xs text-ink-faint">
            {{ detail?.request_id ?? '' }}
          </DialogDescription>

          <div v-if="detailLoading" class="py-10 text-center text-sm text-ink-faint">加载中…</div>
          <p v-else-if="detailError" class="mt-4 text-sm text-danger">{{ detailError }}</p>

          <div v-else-if="detail" class="mt-4 flex min-h-0 flex-1 flex-col gap-4 overflow-y-auto">
            <section>
              <h3 class="mb-1.5 text-xs font-semibold text-ink-soft uppercase">请求</h3>
              <pre
                v-if="detail.request_body"
                class="max-h-72 overflow-auto rounded-lg bg-ink/6 p-3 font-mono text-xs leading-relaxed whitespace-pre-wrap"
                >{{ pretty(detail.request_body) }}</pre>
              <p v-else class="text-xs text-ink-faint">
                未记录 —— 正文快照未开启，或该请求是二进制表单。
              </p>
            </section>

            <section>
              <h3 class="mb-1.5 text-xs font-semibold text-ink-soft uppercase">
                响应{{ detail.stream ? '（流式聚合文本）' : '' }}
              </h3>
              <pre
                v-if="detail.response_body"
                class="max-h-72 overflow-auto rounded-lg bg-ink/6 p-3 font-mono text-xs leading-relaxed whitespace-pre-wrap"
                >{{ pretty(detail.response_body) }}</pre>
              <p v-else class="text-xs text-ink-faint">未记录。</p>
            </section>
          </div>

          <div class="mt-4 flex justify-end">
            <button type="button" class="glass-button-ghost px-4 py-2 text-sm" @click="closeDetail">
              关闭
            </button>
          </div>
        </DialogContent>
      </DialogPortal>
    </DialogRoot>
  </div>
</template>
