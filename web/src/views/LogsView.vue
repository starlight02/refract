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

/** 展开的行 id 集合。 */
const expanded = ref<Set<number>>(new Set())
/** 清理确认与结果提示。 */
const pruneDays = ref(30)
const pruning = ref(false)
const pruneNotice = ref<string | null>(null)

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
    failures_only: draftFailuresOnly.value || undefined,
  })
}

function resetFilter() {
  draftModel.value = ''
  draftChannel.value = ''
  draftFailuresOnly.value = false
  store.fetch({})
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
  URL.revokeObjectURL(url)
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

      <div class="flex flex-wrap items-center gap-2">
        <label class="flex cursor-pointer items-center gap-1.5 text-xs text-ink-soft">
          <input v-model="autoRefresh" type="checkbox" class="accent-[var(--color-accent)]" />
          自动刷新
          <span v-if="autoRefresh" class="text-ink-faint">5s</span>
        </label>

        <button
          type="button"
          class="glass-button-ghost flex items-center gap-1.5 px-3 py-2 text-sm"
          :disabled="store.items.length === 0"
          @click="exportLogs"
        >
          <AppIcon name="download" :size="14" />
          导出本页
        </button>

        <label class="flex items-center gap-1.5 text-xs text-ink-soft">
          清理
          <input
            v-model.number="pruneDays"
            type="number"
            min="1"
            class="glass-field tabular w-16 px-2 py-1 text-sm outline-none"
          />
          天前
        </label>
        <button
          type="button"
          class="glass-button-ghost glass-button-ghost-danger px-3 py-2 text-sm"
          :disabled="pruning"
          @click="prune"
        >
          {{ pruning ? '清理中…' : '执行' }}
        </button>
        <span v-if="pruneNotice" class="text-xs text-ink-faint">{{ pruneNotice }}</span>
      </div>
    </header>

    <!-- 筛选 -->
    <section class="glass glass-specular flex flex-wrap items-end gap-3 p-4">
      <label class="flex flex-col gap-1.5">
        <span class="text-xs font-medium text-ink-soft">模型</span>
        <input
          v-model="draftModel"
          type="text"
          placeholder="精确匹配"
          class="glass-field w-40 px-3 py-1.5 font-mono text-sm outline-none"
          @keydown.enter="applyFilter"
        />
      </label>

      <label class="flex flex-col gap-1.5">
        <span class="text-xs font-medium text-ink-soft">渠道</span>
        <select v-model="draftChannel" class="glass-field w-44 px-3 py-1.5 text-sm outline-none">
          <option value="">全部</option>
          <option v-for="o in channelsStore.options" :key="o.id" :value="o.id">{{ o.name }}</option>
        </select>
      </label>

      <label class="flex cursor-pointer items-center gap-2 pb-2 text-sm">
        <input v-model="draftFailuresOnly" type="checkbox" class="accent-[var(--color-accent)]" />
        只看失败
      </label>

      <div class="flex w-full items-center gap-2 sm:ml-auto sm:w-auto">
        <button
          type="button"
          class="glass-button-primary px-4 py-2 text-sm font-medium"
          @click="applyFilter"
        >
          应用
        </button>
        <button type="button" class="glass-button-ghost px-3 py-2 text-sm" @click="resetFilter">
          重置
        </button>
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

              <td class="px-4 py-2.5 whitespace-nowrap text-ink-soft">{{ channelLabel(log) }}</td>

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
                    <dt class="text-ink-faint">缓存 tokens</dt>
                    <dd class="tabular">{{ log.cached_tokens }}</dd>
                  </div>
                  <div>
                    <dt class="text-ink-faint">推理 tokens</dt>
                    <dd class="tabular">{{ log.reasoning_tokens }}</dd>
                  </div>
                  <div>
                    <dt class="text-ink-faint">重试</dt>
                    <dd class="tabular">{{ log.retries }}</dd>
                  </div>
                  <div>
                    <dt class="text-ink-faint">密钥</dt>
                    <dd class="tabular">{{ log.api_key_id ?? '—' }}</dd>
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
  </div>
</template>
