<script setup lang="ts">
/**
 * 渠道列表。
 *
 * 这一页的核心是让用户**一眼看出路由会怎么走**：优先级、权重、端点协议、
 * 转换开关全部直接可见，而不是藏在编辑页里。new-api 的列表页只显示名字和
 * 状态，结果每次排查「为什么走了这个渠道」都要点进去看。
 */
import { computed, onMounted, ref } from 'vue'
import { useRouter } from 'vue-router'
import ProtocolBadge from '@/components/ProtocolBadge.vue'
import GlassSwitch from '@/components/GlassSwitch.vue'
import AppIcon from '@/components/AppIcon.vue'
import { useChannelsStore } from '@/stores/channels'
import { channels as channelsApi, health as healthApi, settings } from '@/api/client'
import { settle } from '@/utils/async'
import { toErrorMessage } from '@/utils/error'
import type {
  Channel,
  ChannelTestResult,
  EndpointHealth,
  Protocol,
  RoutingPolicy,
} from '@refract/contracts'

const router = useRouter()
const store = useChannelsStore()

/** 「原生优先」是全局路由开关（需求 6），放在列表页顶部因为它影响整列的排序语义。 */
const policy = ref<RoutingPolicy | null>(null)
const policySaving = ref(false)
const policyError = ref<string | null>(null)

/** 每个渠道的测试结果，点了才有。 */
const testResults = ref<Record<number, ChannelTestResult | 'pending'>>({})

/** 待确认删除的渠道 id —— 二次确认避免误删。 */
const pendingDelete = ref<number | null>(null)

const showDisabled = ref(true)

/**
 * 端点健康度。熔断中的端点会被路由排到最后 —— 用户必须能在这里看到
 * 「为什么这个渠道明明启用却没被命中」，以及手动解除熔断。
 */
const healthItems = ref<EndpointHealth[]>([])
/** 正在执行「解除熔断」的端点，键为 `channelId:protocol`。 */
const resetting = ref<Set<string>>(new Set())

/** `channelId:protocol` → 健康快照。 */
const healthByKey = computed(() => {
  const map = new Map<string, EndpointHealth>()
  for (const h of healthItems.value) map.set(`${h.channel_id}:${h.protocol}`, h)
  return map
})

const visible = computed(() =>
  showDisabled.value ? store.items : store.items.filter((c) => c.enabled),
)

/** 列表按「路由实际会怎么排」来排：优先级降序，同级按 id 稳定。 */
const sorted = computed(() =>
  [...visible.value].sort((a, b) => b.priority - a.priority || a.id - b.id),
)

onMounted(async () => {
  await Promise.all([store.fetch(), refreshHealth()])
  const loaded = await settle(settings.routingPolicy())
  if (loaded) policy.value = loaded
})

/** 拉取全量端点健康快照。失败静默 —— 健康是辅助信息，不该挡住渠道列表。 */
async function refreshHealth() {
  healthItems.value = (await settle(healthApi.channels())) ?? []
}

/** 健康快照此刻是否处于熔断中（到期时刻在未来才算）。 */
function isSuspended(h: EndpointHealth): boolean {
  if (!h.suspended_until) return false
  const until = new Date(h.suspended_until).getTime()
  return !Number.isNaN(until) && until > Date.now()
}

/** 渠道里正在熔断的端点。 */
function suspendedEndpoints(ch: Channel): EndpointHealth[] {
  return ch.endpoints
    .map((ep) => healthByKey.value.get(`${ch.id}:${ep.protocol}`))
    .filter((h): h is EndpointHealth => h !== undefined && isSuspended(h))
}

/** 距熔断到期还剩多久，口语化。 */
function suspendRemaining(h: EndpointHealth): string {
  if (!h.suspended_until) return ''
  const ms = new Date(h.suspended_until).getTime() - Date.now()
  if (ms <= 0) return '即将恢复'
  const secs = Math.ceil(ms / 1000)
  if (secs < 60) return `${secs} 秒后自动恢复`
  const mins = Math.ceil(secs / 60)
  if (mins < 60) return `${mins} 分钟后自动恢复`
  return `${Math.ceil(mins / 60)} 小时后自动恢复`
}

/** 手动解除熔断，立刻让端点重新参与路由。 */
async function resetBreaker(h: EndpointHealth) {
  const key = `${h.channel_id}:${h.protocol}`
  resetting.value.add(key)
  try {
    await settle(healthApi.reset(h.channel_id, h.protocol))
  } finally {
    await refreshHealth()
    resetting.value.delete(key)
  }
}

async function setNativeFirst(nativeFirst: boolean) {
  if (!policy.value || policySaving.value) return
  const next: RoutingPolicy = { ...policy.value, native_first: nativeFirst }
  policySaving.value = true
  policyError.value = null
  try {
    policy.value = await settings.setRoutingPolicy(next)
  } catch (e) {
    policyError.value = toErrorMessage(e, '保存路由策略失败')
  } finally {
    policySaving.value = false
  }
}

/** 查询上游余额（仅 OpenAI 兼容渠道有意义）。 */
const balanceBusy = ref<Record<number, boolean>>({})

function canProbeBalance(ch: Channel): boolean {
  return ch.endpoints.some(
    (ep) => ep.enabled && (ep.protocol === 'chat' || ep.protocol === 'responses'),
  )
}

async function refreshBalance(ch: Channel) {
  balanceBusy.value[ch.id] = true
  try {
    const result = await channelsApi.balance(ch.id)
    const target = store.items.find((c) => c.id === ch.id)
    if (target) {
      target.balance = result.balance
      target.balance_updated_at = new Date().toISOString()
    }
  } catch (e) {
    testResults.value[ch.id] = {
      success: false,
      message: toErrorMessage(e, '余额查询失败'),
    }
  } finally {
    balanceBusy.value[ch.id] = false
  }
}

async function runTest(ch: Channel) {
  testResults.value[ch.id] = 'pending'
  try {
    testResults.value[ch.id] = await store.test(ch.id)
  } catch (e) {
    testResults.value[ch.id] = {
      success: false,
      message: toErrorMessage(e, '测试失败'),
    }
  }
}

/** 一键全测：并发测所有启用中的渠道，各自的结果落在各自的卡片上。 */
const testingAll = ref(false)

async function runTestAll() {
  const targets = store.items.filter((ch) => ch.enabled)
  if (targets.length === 0 || testingAll.value) return
  testingAll.value = true
  try {
    await Promise.allSettled(targets.map((ch) => runTest(ch)))
  } finally {
    testingAll.value = false
  }
}

const deletingId = ref<number | null>(null)
const copyingId = ref<number | null>(null)

async function confirmDelete(id: number) {
  if (deletingId.value !== null) return
  deletingId.value = id
  try {
    await settle(store.remove(id))
    pendingDelete.value = null
  } finally {
    deletingId.value = null
  }
}

/** 复制渠道。副本禁用创建，直接跳进编辑页改名与调整。 */
async function duplicateChannel(id: number) {
  if (copyingId.value !== null) return
  copyingId.value = id
  try {
    const copy = await settle(store.duplicate(id))
    if (copy) router.push(`/channels/${copy.id}/edit`)
  } finally {
    copyingId.value = null
  }
}

// ---------------------------------------------------------------------------
// 批量选择
// ---------------------------------------------------------------------------

/** 是否处于批量选择模式。 */
const selecting = ref(false)
/** 已选渠道 id。 */
const selected = ref<Set<number>>(new Set())
const bulkBusy = ref(false)
/** 批量删除的二次确认状态。 */
const bulkConfirmDelete = ref(false)

function toggleSelectMode() {
  selecting.value = !selecting.value
  selected.value = new Set()
  bulkConfirmDelete.value = false
}

function toggleSelected(id: number) {
  const next = new Set(selected.value)
  if (next.has(id)) next.delete(id)
  else next.add(id)
  selected.value = next
}

/** 全选当前可见项；已全选时反转为清空。 */
function toggleSelectAll() {
  selected.value =
    selected.value.size === sorted.value.length ? new Set() : new Set(sorted.value.map((c) => c.id))
}

async function runBulk(action: 'enable' | 'disable' | 'delete') {
  if (selected.value.size === 0 || bulkBusy.value) return
  if (action === 'delete' && !bulkConfirmDelete.value) {
    bulkConfirmDelete.value = true
    return
  }
  bulkBusy.value = true
  try {
    const affected = await settle(store.bulk([...selected.value], action))
    if (affected === undefined) return
    selected.value = new Set()
    bulkConfirmDelete.value = false
    if (action === 'delete') selecting.value = false
  } finally {
    bulkBusy.value = false
  }
}

/** 渠道的全部端点协议，用于列表里的徽章组。 */
function protocols(ch: Channel): Protocol[] {
  return ch.endpoints.map((e) => e.protocol)
}

/** 渠道对外暴露的模型总数（去重）。 */
function modelCount(ch: Channel): number {
  const names = new Set<string>()
  for (const ep of ch.endpoints) for (const m of ep.models) names.add(m.name)
  return names.size
}

/** 该渠道是否有任何端点开了协议转换。 */
function hasTranscode(ch: Channel): boolean {
  return ch.endpoints.some((e) => e.transcode.enabled && e.transcode.accepted.length > 0)
}

/** 全局处于熔断中的端点数 —— 头部统计里提醒，卡片里定位。 */
const suspendedCount = computed(() => healthItems.value.filter(isSuspended).length)

function addressLabel(ch: Channel): string {
  const a = ch.address
  if (!a.unofficial) return '官方地址'
  if (a.full_address) return a.base_url || '（未填完整地址）'
  return [a.base_url, a.version_prefix, a.path].filter(Boolean).join('') || '（未填地址）'
}
</script>

<template>
  <div class="mx-auto max-w-6xl">
    <header class="mb-6 flex flex-wrap items-end justify-between gap-4">
      <div>
        <h1 class="text-2xl font-semibold">渠道</h1>
        <p class="mt-1 text-sm text-ink-faint">
          共 {{ store.items.length }} 条 · 启用 {{ store.items.filter((c) => c.enabled).length }} 条
          <span v-if="suspendedCount > 0" class="text-danger">
            · 熔断 {{ suspendedCount }} 个端点
          </span>
        </p>
      </div>

      <div class="flex items-center gap-2">
        <button
          v-if="store.items.length > 0"
          type="button"
          class="glass-button-ghost px-3.5 py-2 text-sm font-medium"
          :disabled="testingAll"
          title="并发测试全部启用中的渠道"
          @click="runTestAll"
        >
          <AppIcon
            :name="testingAll ? 'spinner' : 'bolt'"
            :class="testingAll ? 'animate-spin' : ''"
            :size="15"
          />
          {{ testingAll ? '测试中…' : '全部测试' }}
        </button>
        <button
          v-if="store.items.length > 0"
          type="button"
          class="glass-button-ghost px-3.5 py-2 text-sm font-medium"
          :class="selecting ? '!bg-accent/12 !text-accent' : ''"
          @click="toggleSelectMode"
        >
          <AppIcon name="checklist" :size="15" />
          {{ selecting ? '完成' : '批量管理' }}
        </button>
        <button
          type="button"
          class="glass-button-primary flex items-center gap-1.5 px-4 py-2 text-sm font-medium"
          @click="router.push('/channels/new')"
        >
          <AppIcon name="plus" :size="15" />
          新建渠道
        </button>
      </div>
    </header>

    <!-- 全局开关 + 过滤 -->
    <div class="glass glass-specular mb-5 flex flex-wrap items-center justify-between gap-4 p-4">
      <label v-if="policy" class="flex cursor-pointer items-center gap-3">
        <GlassSwitch
          :model-value="policy.native_first"
          :disabled="policySaving"
          label="原生优先"
          @update:model-value="setNativeFirst"
        />
        <span class="text-sm">
          <span class="font-medium">原生优先</span>
          <span class="ml-2 text-xs text-ink-faint">
            {{
              policy.native_first
                ? '原生协议端点压过高优先级的转换端点'
                : '完全按优先级路由（new-api 语义）'
            }}
          </span>
        </span>
      </label>
      <p v-if="policyError" class="w-full text-xs text-danger">{{ policyError }}</p>

      <label class="flex cursor-pointer items-center gap-2 text-sm text-ink-soft">
        <input v-model="showDisabled" type="checkbox" class="accent-[var(--color-accent)]" />
        显示已禁用
      </label>
    </div>

    <p v-if="store.error" class="glass mb-4 border-danger/30 p-4 text-sm text-danger">
      {{ store.error }}
    </p>

    <!-- 空状态 -->
    <div
      v-if="!store.loading && sorted.length === 0"
      class="glass glass-specular py-10 text-center"
    >
      <div class="shape-app-icon mx-auto grid size-14 place-items-center bg-ink/6 text-ink-faint">
        <AppIcon name="channels" :size="26" />
      </div>
      <h2 class="mt-4 font-medium">还没有渠道</h2>
      <p class="mx-auto mt-2 max-w-sm text-sm text-ink-faint">
        渠道是网关的上游。新建一个渠道，填上 base URL 和 API key，就能开始转发请求。
      </p>
      <button
        type="button"
        class="glass-button-primary mt-5 px-4 py-2 text-sm font-medium"
        @click="router.push('/channels/new')"
      >
        新建第一个渠道
      </button>
    </div>

    <div
      v-else-if="store.loading && sorted.length === 0"
      class="py-16 text-center text-sm text-ink-faint"
    >
      加载中…
    </div>

    <!-- 渠道卡片 -->
    <div v-else class="flex flex-col gap-3" :class="selecting ? 'pb-24' : ''">
      <article
        v-for="ch in sorted"
        :key="ch.id"
        class="glass glass-specular glass-interactive p-4"
        :class="[
          ch.enabled ? '' : 'opacity-55',
          selecting && selected.has(ch.id) ? 'ring-2 ring-accent/45' : '',
          selecting ? 'cursor-pointer select-none' : '',
        ]"
        @click="selecting ? toggleSelected(ch.id) : undefined"
      >
        <div
          class="flex flex-col items-stretch gap-4 sm:flex-row sm:items-start sm:justify-between"
        >
          <!-- 批量选择模式下的复选框 -->
          <div v-if="selecting" class="flex shrink-0 items-start pt-0.5">
            <span
              class="grid size-5 place-items-center rounded-md border transition-colors"
              :class="
                selected.has(ch.id)
                  ? 'border-accent bg-accent text-white'
                  : 'border-ink/25 bg-transparent text-transparent'
              "
              role="checkbox"
              :aria-checked="selected.has(ch.id)"
              :aria-label="`选择 ${ch.name}`"
            >
              <AppIcon name="check" :size="12" />
            </span>
          </div>

          <!-- 左：身份 -->
          <div class="min-w-0 flex-1">
            <div class="flex flex-wrap items-center gap-2">
              <h3 class="whitespace-nowrap text-base font-semibold text-ink">{{ ch.name }}</h3>
              <span
                v-if="ch.kind === 'aggregate'"
                class="proto-badge"
                style="color: var(--color-accent)"
              >
                聚合
              </span>
              <ProtocolBadge
                v-for="p in protocols(ch)"
                :key="p"
                :protocol="p"
                :compact="ch.kind === 'aggregate' && ch.endpoints.length > 2"
              />

              <span
                v-if="hasTranscode(ch)"
                class="proto-badge"
                style="color: var(--color-warning)"
                title="该渠道有端点开启了协议转换"
              >
                转换
              </span>

              <span
                v-if="ch.auto_disabled"
                class="proto-badge"
                style="color: var(--color-danger)"
                title="连续凭据错误被自动停用；按设置的间隔重测，通过后自动恢复。手动启用可立即清除。"
              >
                自动停用
              </span>
            </div>

            <div class="mt-1.5 truncate font-mono text-xs text-ink-faint">
              {{ addressLabel(ch) }}
            </div>

            <div class="mt-2 flex flex-wrap items-center gap-x-4 gap-y-1 text-xs text-ink-faint">
              <span
                >优先级
                <span class="tabular font-medium text-ink-soft">{{ ch.priority }}</span></span
              >
              <span
                >权重 <span class="tabular font-medium text-ink-soft">{{ ch.weight }}</span></span
              >
              <span
                ><span class="tabular font-medium text-ink-soft">{{ modelCount(ch) }}</span>
                个模型</span
              >
              <span v-if="ch.endpoints.length > 1">
                <span class="tabular font-medium text-ink-soft">{{ ch.endpoints.length }}</span>
                个端点
              </span>
              <span v-if="ch.balance != null" :title="`刷新于 ${ch.balance_updated_at ?? ''}`">
                余额
                <span
                  class="tabular font-medium"
                  :class="ch.balance < 1 ? 'text-danger' : 'text-ink-soft'"
                >
                  ${{ ch.balance.toFixed(2) }}
                </span>
              </span>
              <span v-for="t in ch.tags ?? []" :key="t" class="rounded bg-ink/8 px-1.5 py-0.5">{{
                t
              }}</span>
            </div>

            <!-- 测试结果 -->
            <div
              v-if="testResults[ch.id]"
              class="mt-2.5 rounded-lg px-3 py-2 text-xs"
              :class="
                testResults[ch.id] === 'pending'
                  ? 'bg-ink/5 text-ink-faint'
                  : (testResults[ch.id] as ChannelTestResult).success
                    ? 'bg-success/12 text-success'
                    : 'bg-danger/12 text-danger'
              "
            >
              <template v-if="testResults[ch.id] === 'pending'">测试中…</template>
              <template v-else>
                <span class="inline-flex items-center gap-1.5">
                  <AppIcon
                    :name="(testResults[ch.id] as ChannelTestResult).success ? 'check' : 'x'"
                    :size="13"
                  />
                  {{ (testResults[ch.id] as ChannelTestResult).message
                  }}<template v-if="(testResults[ch.id] as ChannelTestResult).latency_ms != null">
                    · {{ (testResults[ch.id] as ChannelTestResult).latency_ms }}ms</template
                  >
                </span>
              </template>
            </div>

            <!-- 熔断警示：被熔断的端点仍会被路由（作为最后手段），
                 但用户有权知道并手动解除。 -->
            <div
              v-for="h in suspendedEndpoints(ch)"
              :key="`susp-${ch.id}-${h.protocol}`"
              class="mt-2.5 rounded-lg bg-danger/12 px-3 py-2 text-xs text-danger"
            >
              <div class="flex flex-wrap items-center gap-2">
                <span class="font-semibold">{{ h.protocol }} 端点熔断中</span>
                <span class="text-danger/80">{{ suspendRemaining(h) }}</span>
                <span>连续失败 {{ h.consecutive_fails }} 次</span>
                <button
                  type="button"
                  class="ml-auto inline-flex items-center gap-1 rounded-md bg-danger/15 px-2 py-1 font-medium hover:bg-danger/25 disabled:opacity-50"
                  :disabled="resetting.has(`${ch.id}:${h.protocol}`)"
                  @click="resetBreaker(h)"
                >
                  <AppIcon
                    v-if="resetting.has(`${ch.id}:${h.protocol}`)"
                    name="spinner"
                    class="animate-spin"
                    :size="11"
                  />
                  {{ resetting.has(`${ch.id}:${h.protocol}`) ? '解除中…' : '解除熔断' }}
                </button>
              </div>
              <p v-if="h.last_error" class="mt-1.5 line-clamp-2 text-danger/80">
                {{ h.last_error }}
              </p>
            </div>
          </div>

          <!-- 右：操作。批量选择模式下隐藏 —— 卡片此时是选择目标，不是操作对象。 -->
          <div
            v-if="!selecting"
            class="flex shrink-0 flex-wrap items-center gap-1.5 sm:justify-end"
          >
            <GlassSwitch
              class="mr-1"
              :model-value="ch.enabled"
              :label="ch.enabled ? `禁用 ${ch.name}` : `启用 ${ch.name}`"
              tone="success"
              @update:model-value="store.toggleEnabled(ch.id, $event)"
            />

            <button
              type="button"
              class="glass-button-ghost px-2.5 py-1.5 text-xs"
              :disabled="testResults[ch.id] === 'pending' || testingAll"
              @click="runTest(ch)"
            >
              <AppIcon
                :name="testResults[ch.id] === 'pending' ? 'spinner' : 'bolt'"
                :class="testResults[ch.id] === 'pending' ? 'animate-spin' : ''"
                :size="13"
              />
              {{ testResults[ch.id] === 'pending' ? '测试中…' : '测试' }}
            </button>

            <button
              v-if="canProbeBalance(ch)"
              type="button"
              class="glass-button-ghost px-2.5 py-1.5 text-xs"
              :disabled="balanceBusy[ch.id]"
              title="查询上游余额（OpenAI 兼容 billing 端点）"
              @click="refreshBalance(ch)"
            >
              <AppIcon
                :name="balanceBusy[ch.id] ? 'spinner' : 'download'"
                :class="balanceBusy[ch.id] ? 'animate-spin' : ''"
                :size="13"
              />
              {{ balanceBusy[ch.id] ? '查询中…' : '余额' }}
            </button>

            <button
              type="button"
              class="glass-button-ghost px-2.5 py-1.5 text-xs"
              :disabled="copyingId === ch.id"
              @click="duplicateChannel(ch.id)"
            >
              <AppIcon
                :name="copyingId === ch.id ? 'spinner' : 'copy'"
                :class="copyingId === ch.id ? 'animate-spin' : ''"
                :size="13"
              />
              {{ copyingId === ch.id ? '复制中…' : '复制' }}
            </button>

            <button
              type="button"
              class="glass-button-ghost px-2.5 py-1.5 text-xs"
              @click="router.push(`/channels/${ch.id}/edit`)"
            >
              <AppIcon name="pencil" :size="13" />
              编辑
            </button>

            <button
              v-if="pendingDelete !== ch.id"
              type="button"
              class="glass-button-ghost glass-button-ghost-danger px-2.5 py-1.5 text-xs !text-ink-faint hover:!text-danger"
              @click="pendingDelete = ch.id"
            >
              <AppIcon name="trash" :size="13" />
              删除
            </button>
            <template v-else>
              <button
                type="button"
                class="inline-flex items-center gap-1 rounded-lg bg-danger px-3 py-1.5 text-xs font-medium text-white hover:brightness-105 disabled:opacity-50"
                :disabled="deletingId === ch.id"
                @click="confirmDelete(ch.id)"
              >
                <AppIcon
                  v-if="deletingId === ch.id"
                  name="spinner"
                  class="animate-spin"
                  :size="12"
                />
                {{ deletingId === ch.id ? '删除中…' : '确认' }}
              </button>
              <button
                type="button"
                class="glass-button-ghost px-2.5 py-1.5 text-xs"
                :disabled="deletingId === ch.id"
                @click="pendingDelete = null"
              >
                取消
              </button>
            </template>
          </div>
        </div>
      </article>
    </div>

    <!-- 批量操作浮动工具条：选择模式专属，固定在视口底部居中。 -->
    <Transition
      enter-active-class="transition-[opacity,transform] duration-200 ease-[--ease-glass]"
      enter-from-class="translate-y-3 opacity-0"
      leave-active-class="transition-[opacity,transform] duration-150 ease-[--ease-glass]"
      leave-to-class="translate-y-3 opacity-0"
    >
      <div
        v-if="selecting"
        class="glass-thick glass-specular fixed bottom-24 left-1/2 z-30 flex -translate-x-1/2 flex-wrap items-center justify-center gap-2 px-4 py-2.5 md:bottom-8"
        role="toolbar"
        aria-label="批量操作"
      >
        <span class="tabular pr-1 text-sm font-medium">已选 {{ selected.size }} 条</span>

        <button
          type="button"
          class="glass-button-ghost px-2.5 py-1.5 text-xs"
          @click="toggleSelectAll"
        >
          {{ selected.size === sorted.length && sorted.length > 0 ? '全不选' : '全选' }}
        </button>

        <span class="h-4 w-px bg-ink/15" aria-hidden="true"></span>

        <button
          type="button"
          class="glass-button-ghost px-2.5 py-1.5 text-xs"
          :disabled="selected.size === 0 || bulkBusy"
          @click="runBulk('enable')"
        >
          <AppIcon v-if="bulkBusy" name="spinner" class="animate-spin" :size="12" />
          启用
        </button>
        <button
          type="button"
          class="glass-button-ghost px-2.5 py-1.5 text-xs"
          :disabled="selected.size === 0 || bulkBusy"
          @click="runBulk('disable')"
        >
          <AppIcon v-if="bulkBusy" name="spinner" class="animate-spin" :size="12" />
          禁用
        </button>
        <button
          v-if="!bulkConfirmDelete"
          type="button"
          class="glass-button-ghost glass-button-ghost-danger px-2.5 py-1.5 text-xs"
          :disabled="selected.size === 0 || bulkBusy"
          @click="runBulk('delete')"
        >
          删除
        </button>
        <button
          v-else
          type="button"
          class="inline-flex items-center gap-1 rounded-lg bg-danger px-3 py-1.5 text-xs font-medium text-white hover:brightness-105 disabled:opacity-50"
          :disabled="bulkBusy"
          @click="runBulk('delete')"
        >
          <AppIcon v-if="bulkBusy" name="spinner" class="animate-spin" :size="12" />
          {{ bulkBusy ? '删除中…' : `确认删除 ${selected.size} 条` }}
        </button>
      </div>
    </Transition>
  </div>
</template>
