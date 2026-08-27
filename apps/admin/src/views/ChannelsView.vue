<script setup lang="ts">
/**
 * 渠道列表。
 *
 * 这一页的核心是让用户**一眼看出路由会怎么走**：优先级、权重、端点协议、
 * 转换开关全部直接可见，而不是藏在编辑页里。new-api 的列表页只显示名字和
 * 状态，结果每次排查「为什么走了这个渠道」都要点进去看。
 */
import { computed, onMounted, ref, toRef } from 'vue'
import { useRouter } from 'vue-router'
import GlassSwitch from '@/components/GlassSwitch.vue'
import ChannelListCard from '@/components/channels/ChannelListCard.vue'
import AppIcon from '@/components/AppIcon.vue'
import { useChannelsStore } from '@/stores/channels'
import { channels as channelsApi, health as healthApi, settings } from '@/api/client'
import { useAction } from '@/composables/useAction'
import { isSuccess, orElse, settled } from '@/utils/effect'
import { toErrorMessage } from '@/utils/error'
import type { Channel, ChannelTestResult, EndpointHealth, RoutingPolicy } from '@refract/contracts'

const router = useRouter()
const store = useChannelsStore()

/** 「原生优先」是全局路由开关（需求 6），放在列表页顶部因为它影响整列的排序语义。 */
const policy = ref<RoutingPolicy | null>(null)
const savePolicy = useAction('保存路由策略失败')
const policySaving = toRef(savePolicy, 'busy')
const policyError = toRef(savePolicy, 'error')

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
  const loaded = await orElse(() => settings.routingPolicy())
  if (loaded) policy.value = loaded
})

/** 拉取全量端点健康快照。失败静默 —— 健康是辅助信息，不该挡住渠道列表。 */
async function refreshHealth() {
  healthItems.value = await orElse(() => healthApi.channels(), [])
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

/** 手动解除熔断，立刻让端点重新参与路由。 */
async function resetBreaker(h: EndpointHealth) {
  const key = `${h.channel_id}:${h.protocol}`
  resetting.value.add(key)
  await orElse(() => healthApi.reset(h.channel_id, h.protocol))
  await refreshHealth()
  resetting.value.delete(key)
}

async function setNativeFirst(nativeFirst: boolean) {
  if (!policy.value || policySaving.value) return
  const next: RoutingPolicy = { ...policy.value, native_first: nativeFirst }
  await savePolicy.run(
    () => settings.setRoutingPolicy(next),
    (saved) => {
      policy.value = saved
    },
  )
}

/** 查询上游余额（仅 OpenAI 兼容渠道有意义）。 */
const balanceBusy = ref<Record<number, boolean>>({})

async function refreshBalance(ch: Channel) {
  balanceBusy.value[ch.id] = true
  const outcome = await settled(() => channelsApi.balance(ch.id))
  balanceBusy.value[ch.id] = false
  if (isSuccess(outcome)) {
    const target = store.items.find((c) => c.id === ch.id)
    if (target) {
      target.balance = outcome.success.balance
      target.balance_updated_at = new Date().toISOString()
    }
    return
  }
  testResults.value[ch.id] = {
    success: false,
    message: toErrorMessage(outcome.failure, '余额查询失败'),
  }
}

async function runTest(ch: Channel) {
  testResults.value[ch.id] = 'pending'
  const outcome = await settled(() => store.test(ch.id))
  testResults.value[ch.id] = isSuccess(outcome)
    ? outcome.success
    : { success: false, message: toErrorMessage(outcome.failure, '测试失败') }
}

/** 一键全测：并发测所有启用中的渠道，各自的结果落在各自的卡片上。 */
const testingAll = ref(false)

async function runTestAll() {
  const targets = store.items.filter((ch) => ch.enabled)
  if (targets.length === 0 || testingAll.value) return
  testingAll.value = true
  await Promise.allSettled(targets.map((ch) => runTest(ch)))
  testingAll.value = false
}

const deletingId = ref<number | null>(null)
const copyingId = ref<number | null>(null)

async function confirmDelete(id: number) {
  if (deletingId.value !== null) return
  deletingId.value = id
  await orElse(() => store.remove(id))
  pendingDelete.value = null
  deletingId.value = null
}

/** 复制渠道。副本禁用创建，直接跳进编辑页改名与调整。 */
async function duplicateChannel(id: number) {
  if (copyingId.value !== null) return
  copyingId.value = id
  const copy = await orElse(() => store.duplicate(id))
  if (copy) router.push(`/channels/${copy.id}/edit`)
  copyingId.value = null
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
  const affected = await orElse(() => store.bulk([...selected.value], action))
  bulkBusy.value = false
  if (affected === undefined) return
  selected.value = new Set()
  bulkConfirmDelete.value = false
  if (action === 'delete') selecting.value = false
}

/** 全局处于熔断中的端点数 —— 头部统计里提醒，卡片里定位。 */
const suspendedCount = computed(() => healthItems.value.filter(isSuspended).length)
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
      <ChannelListCard
        v-for="ch in sorted"
        :key="ch.id"
        :channel="ch"
        :selecting="selecting"
        :selected="selected.has(ch.id)"
        :test-result="testResults[ch.id]"
        :suspended="suspendedEndpoints(ch)"
        :resetting="resetting"
        :testing-all="testingAll"
        :balance-busy="!!balanceBusy[ch.id]"
        :copying="copyingId === ch.id"
        :pending-delete="pendingDelete === ch.id"
        :deleting="deletingId === ch.id"
        @select="toggleSelected(ch.id)"
        @toggle-enabled="store.toggleEnabled(ch.id, $event)"
        @test="runTest(ch)"
        @balance="refreshBalance(ch)"
        @duplicate="duplicateChannel(ch.id)"
        @edit="router.push(`/channels/${ch.id}/edit`)"
        @ask-delete="pendingDelete = ch.id"
        @confirm-delete="confirmDelete(ch.id)"
        @cancel-delete="pendingDelete = null"
        @reset-breaker="resetBreaker"
      />
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
