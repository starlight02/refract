<script setup lang="ts">
/**
 * 网关设置。
 *
 * 两块内容：
 * 1. 路由策略 —— 是否原生优先、选择模式、最大重试次数。
 *    设计成「先调到满意再保存」，而不是「改一行就存一次」。
 * 2. 日志保留 —— 后台定时清理请求日志的周期。
 * 3. 管理令牌 —— 管理 API 的鉴权。启用后所有 /api 请求都要带令牌，
 *    所以设置成功的同时必须把令牌存进本浏览器，否则下一步请求就 401。
 */
import { computed, onMounted, ref } from 'vue'
import GlassSwitch from '@/components/GlassSwitch.vue'
import AppIcon from '@/components/AppIcon.vue'
import {
  backup,
  getAdminToken,
  setAdminToken as storeLocalToken,
  settings,
  type ImportResult,
} from '@/api/client'
import type { BreakerPolicy, RoutingPolicy, SelectionMode } from '@/api/types'

const loading = ref(true)
const saving = ref(false)
const saved = ref(false)
const loadError = ref<string | null>(null)
const saveError = ref<string | null>(null)

const policy = ref<RoutingPolicy>({
  native_first: true,
  selection: 'weighted_random',
  max_attempts: 3,
  retry_same_channel: true,
})
const retentionDays = ref(30)
const breaker = ref<BreakerPolicy>({
  failure_threshold: 5,
  base_cooldown_secs: 30,
  max_cooldown_secs: 900,
})

/** 上一次从后端拉到的快照，用于判断有没有改过。 */
let policySnapshot = ''
let retentionSnapshot = 30
let breakerSnapshot = ''

const policyDirty = computed(() => policySnapshot !== JSON.stringify(policy.value))
const retentionDirty = computed(() => retentionSnapshot !== retentionDays.value)
const breakerDirty = computed(() => breakerSnapshot !== JSON.stringify(breaker.value))
const isDirty = computed(() => policyDirty.value || retentionDirty.value || breakerDirty.value)
const retentionValid = computed(
  () =>
    Number.isInteger(retentionDays.value) &&
    retentionDays.value >= 1 &&
    retentionDays.value <= 3650,
)
/** 与后端 BreakerPolicy::validate 一致的客户端校验。 */
const breakerValid = computed(() => {
  const b = breaker.value
  return (
    Number.isInteger(b.failure_threshold) &&
    b.failure_threshold >= 0 &&
    b.failure_threshold <= 1000 &&
    Number.isInteger(b.base_cooldown_secs) &&
    b.base_cooldown_secs >= 1 &&
    b.base_cooldown_secs <= 86_400 &&
    Number.isInteger(b.max_cooldown_secs) &&
    b.max_cooldown_secs >= b.base_cooldown_secs &&
    b.max_cooldown_secs <= 86_400
  )
})

onMounted(async () => {
  try {
    const [p, retention, b] = await Promise.all([
      settings.routingPolicy(),
      settings.logRetention(),
      settings.breakerPolicy(),
    ])
    policy.value = p
    policySnapshot = JSON.stringify(p)
    retentionDays.value = retention.days
    retentionSnapshot = retention.days
    breaker.value = b
    breakerSnapshot = JSON.stringify(b)
  } catch (e) {
    loadError.value = e instanceof Error ? e.message : '加载失败'
  } finally {
    loading.value = false
  }
})

async function save() {
  if (!retentionValid.value) {
    saveError.value = '日志保留天数必须是 1–3650 的整数'
    return
  }
  if (!breakerValid.value) {
    saveError.value = '熔断参数不合法：阈值 0–1000，冷却 1–86400 秒且上限不小于起始值'
    return
  }
  saving.value = true
  saveError.value = null
  saved.value = false
  try {
    if (policyDirty.value) {
      const p = await settings.setRoutingPolicy(policy.value)
      policy.value = p
      policySnapshot = JSON.stringify(p)
    }
    if (retentionDirty.value) {
      const retention = await settings.setLogRetention(retentionDays.value)
      retentionDays.value = retention.days
      retentionSnapshot = retention.days
    }
    if (breakerDirty.value) {
      const b = await settings.setBreakerPolicy(breaker.value)
      breaker.value = b
      breakerSnapshot = JSON.stringify(b)
    }
    saved.value = true
  } catch (e) {
    saveError.value = e instanceof Error ? e.message : '保存失败'
  } finally {
    saving.value = false
  }
}

const SELECTION_OPTIONS: { value: SelectionMode; label: string; desc: string }[] = [
  {
    value: 'weighted_random',
    label: '加权随机（推荐）',
    desc: '同优先级内按权重随机选取。适合多渠道流量分配。',
  },
  { value: 'round_robin', label: '轮询', desc: '同优先级内按顺序轮转。适合等量消耗多家余额。' },
  { value: 'first', label: '固定首选', desc: '总是命中同优先级内第一个可用渠道。简单但单点。' },
]

// ── 管理令牌 ──

/** 本浏览器是否保存了令牌 —— 决定提示文案，也方便排查「为什么弹令牌框」。 */
const hasLocalToken = ref(false)
const tokenDraft = ref('')
const showTokenDraft = ref(false)
const tokenBusy = ref(false)
const tokenNotice = ref<{ tone: 'success' | 'danger'; text: string } | null>(null)

onMounted(() => {
  hasLocalToken.value = getAdminToken() !== null
})

/**
 * 启用或更换服务端令牌。
 *
 * 成功后立刻把新令牌写入本地存储：设置令牌的响应本身不需要令牌，但**下一个**
 * 请求就需要了 —— 不存下来的话用户会被自己刚设置的鉴权立刻锁出去。
 */
async function applyToken() {
  const token = tokenDraft.value.trim()
  if (!token || tokenBusy.value) return
  tokenBusy.value = true
  tokenNotice.value = null
  try {
    await settings.setAdminToken(token)
    storeLocalToken(token)
    hasLocalToken.value = true
    tokenDraft.value = ''
    tokenNotice.value = { tone: 'success', text: '令牌已生效，本浏览器已保存。' }
  } catch (e) {
    tokenNotice.value = {
      tone: 'danger',
      text: e instanceof Error ? e.message : '设置失败',
    }
  } finally {
    tokenBusy.value = false
  }
}

/** 关闭管理鉴权：服务端清除令牌哈希，本地也一并清掉。 */
async function clearToken() {
  if (tokenBusy.value) return
  tokenBusy.value = true
  tokenNotice.value = null
  try {
    await settings.setAdminToken(null)
    storeLocalToken(null)
    hasLocalToken.value = false
    tokenNotice.value = { tone: 'success', text: '管理鉴权已关闭。' }
  } catch (e) {
    tokenNotice.value = {
      tone: 'danger',
      text: e instanceof Error ? e.message : '关闭失败',
    }
  } finally {
    tokenBusy.value = false
  }
}

// ── 数据备份 ──

const exportBusy = ref(false)
const importBusy = ref(false)
const importMode = ref<'merge' | 'replace'>('merge')
const backupNotice = ref<{ tone: 'success' | 'danger'; text: string } | null>(null)
const importFileInput = ref<HTMLInputElement | null>(null)
/** 待确认的替换导入：文件已解析但还没提交，等用户二次确认。 */
const pendingReplace = ref<{ name: string; payload: unknown } | null>(null)

/** 导出全量配置并触发浏览器下载。 */
async function exportBackup() {
  if (exportBusy.value) return
  exportBusy.value = true
  backupNotice.value = null
  try {
    const document_ = await backup.export()
    const blob = new Blob([JSON.stringify(document_, null, 2)], { type: 'application/json' })
    const url = URL.createObjectURL(blob)
    const a = document.createElement('a')
    a.href = url
    a.download = `refract-backup-${new Date().toISOString().slice(0, 10)}.json`
    a.click()
    URL.revokeObjectURL(url)
    backupNotice.value = {
      tone: 'success',
      text: '备份已下载。文件包含渠道凭据明文，请像保管密钥一样保管它。',
    }
  } catch (e) {
    backupNotice.value = { tone: 'danger', text: e instanceof Error ? e.message : '导出失败' }
  } finally {
    exportBusy.value = false
  }
}

/**
 * 读取选中的备份文件。合并模式直接导入；替换模式先清空再导入、
 * 不可恢复，所以解析后停下来等一次显式确认。
 */
async function importBackup(event: Event) {
  const input = event.target as HTMLInputElement
  const file = input.files?.[0]
  // 允许连续选择同一个文件重新导入。
  input.value = ''
  if (!file || importBusy.value) return

  backupNotice.value = null
  pendingReplace.value = null
  let parsed: unknown
  try {
    parsed = JSON.parse(await file.text())
  } catch {
    backupNotice.value = { tone: 'danger', text: '文件不是有效的 JSON' }
    return
  }

  if (importMode.value === 'replace') {
    pendingReplace.value = { name: file.name, payload: parsed }
    return
  }
  await runImport(parsed)
}

/** 用户确认后执行替换导入。 */
async function confirmReplaceImport() {
  const pending = pendingReplace.value
  if (!pending) return
  pendingReplace.value = null
  await runImport(pending.payload)
}

/** 跳过名单太长会把提示挤成一堵墙：列前几个，其余折成计数。 */
function skippedDetail(kind: string, names: string[]): string {
  if (names.length === 0) return ''
  const shown = names.slice(0, 5).join('、')
  const rest = names.length > 5 ? ` 等 ${names.length} 个` : ''
  return `跳过的${kind}：${shown}${rest}。`
}

async function runImport(payload: unknown) {
  importBusy.value = true
  backupNotice.value = null
  try {
    const result: ImportResult = await backup.import(payload, importMode.value)
    const detail = [
      skippedDetail('渠道', result.skipped_channels ?? []),
      skippedDetail('密钥', result.skipped_keys ?? []),
    ]
      .filter(Boolean)
      .join(' ')
    backupNotice.value = {
      tone: 'success',
      text:
        `导入完成：渠道 +${result.channels_imported}（跳过 ${result.channels_skipped}），` +
        `密钥 +${result.keys_imported}（跳过 ${result.keys_skipped}）。` +
        (detail ? ` ${detail}` : ''),
    }
  } catch (e) {
    backupNotice.value = {
      tone: 'danger',
      text: e instanceof Error ? e.message : '导入失败',
    }
  } finally {
    importBusy.value = false
  }
}
</script>

<template>
  <div class="mx-auto max-w-2xl pb-16">
    <header class="mb-6">
      <h1 class="text-2xl font-semibold">设置</h1>
      <p class="mt-1 text-sm text-ink-faint">变更需要保存才会生效。</p>
    </header>

    <div v-if="loading" class="py-16 text-center text-sm text-ink-faint">加载中…</div>

    <div v-else-if="loadError" class="glass border-danger/30 p-4 text-sm text-danger">
      {{ loadError }}
    </div>

    <template v-else>
      <!-- 路由策略 -->
      <section class="glass glass-specular flex flex-col gap-5 p-5">
        <h2 class="text-sm font-semibold text-ink-soft uppercase">路由策略</h2>

        <!-- 原生优先（需求 6） -->
        <label class="flex cursor-pointer items-center gap-3">
          <GlassSwitch v-model="policy.native_first" label="原生优先" />
          <div>
            <span class="text-sm font-medium">原生优先</span>
            <p class="mt-0.5 text-xs text-ink-faint">
              关闭时路由逻辑与 new-api 一致。打开时命中同一模型的原生协议端点始终排在转换端点之前。
            </p>
          </div>
        </label>

        <!-- 选择模式 -->
        <div>
          <span class="mb-2 block text-sm font-medium text-ink-soft">选择模式</span>
          <div class="flex flex-col gap-2">
            <label
              v-for="o in SELECTION_OPTIONS"
              :key="o.value"
              class="flex cursor-pointer items-start gap-3 rounded-lg border border-ink/8 px-4 py-3 transition-colors duration-150"
              :class="
                policy.selection === o.value
                  ? 'border-accent/40 bg-accent/8'
                  : 'hover:bg-ink/[0.03]'
              "
            >
              <input
                v-model="policy.selection"
                type="radio"
                :value="o.value"
                name="selection"
                class="mt-0.5 accent-[var(--color-accent)]"
              />
              <div>
                <p class="text-sm font-medium">{{ o.label }}</p>
                <p class="mt-0.5 text-xs text-ink-faint">{{ o.desc }}</p>
              </div>
            </label>
          </div>
        </div>

        <!-- 最大重试 -->
        <label class="flex flex-col gap-1.5">
          <span class="text-sm font-medium text-ink-soft">
            最大重试次数
            <span class="ml-2 font-normal text-ink-faint">
              建议 2–3。过大会拉长超时，1 禁用重试。
            </span>
          </span>
          <input
            v-model.number="policy.max_attempts"
            type="number"
            min="1"
            max="10"
            class="glass-field tabular w-32 px-3 py-2 text-sm outline-none"
          />
        </label>

        <!-- 重试同一渠道 -->
        <label class="flex cursor-pointer items-center gap-3">
          <input
            v-model="policy.retry_same_channel"
            type="checkbox"
            class="accent-[var(--color-accent)]"
          />
          <span class="text-sm">
            重试时允许再次命中同一渠道
            <span class="text-xs text-ink-faint"
              >—— 建议关闭，否则 500 可能只是上游临时故障，原渠道未必恢复</span
            >
          </span>
        </label>
      </section>

      <!-- 熔断 -->
      <section class="glass glass-specular mt-5 flex flex-col gap-4 p-5">
        <div>
          <h2 class="text-sm font-semibold text-ink-soft uppercase">熔断</h2>
          <p class="mt-1 text-xs text-ink-faint">
            端点连续失败达到阈值后暂停参与路由，冷却按指数退避直到上限；期间一次成功即恢复。
            阈值设为 0 关闭熔断。改动立即生效，已在冷却中的端点不受影响。
          </p>
        </div>

        <div class="grid max-w-lg grid-cols-1 gap-4 sm:grid-cols-3">
          <label class="flex flex-col gap-1.5">
            <span class="text-sm font-medium text-ink-soft">失败阈值</span>
            <input
              v-model.number="breaker.failure_threshold"
              type="number"
              min="0"
              max="1000"
              step="1"
              inputmode="numeric"
              aria-label="熔断失败阈值"
              class="glass-field tabular px-3 py-2 text-sm outline-none"
            />
            <span class="text-xs text-ink-faint">连续失败次数，0 关闭</span>
          </label>

          <label class="flex flex-col gap-1.5">
            <span class="text-sm font-medium text-ink-soft">起始冷却（秒）</span>
            <input
              v-model.number="breaker.base_cooldown_secs"
              type="number"
              min="1"
              max="86400"
              step="1"
              inputmode="numeric"
              aria-label="熔断起始冷却秒数"
              class="glass-field tabular px-3 py-2 text-sm outline-none"
            />
            <span class="text-xs text-ink-faint">首次熔断的时长</span>
          </label>

          <label class="flex flex-col gap-1.5">
            <span class="text-sm font-medium text-ink-soft">冷却上限（秒）</span>
            <input
              v-model.number="breaker.max_cooldown_secs"
              type="number"
              min="1"
              max="86400"
              step="1"
              inputmode="numeric"
              aria-label="熔断冷却上限秒数"
              class="glass-field tabular px-3 py-2 text-sm outline-none"
            />
            <span class="text-xs text-ink-faint">退避不超过该值</span>
          </label>
        </div>

        <p v-if="!breakerValid" class="text-xs text-danger" role="alert">
          阈值 0–1000；冷却 1–86400 秒，且上限不能小于起始值。
        </p>
      </section>

      <section class="glass glass-specular mt-5 flex flex-col gap-4 p-5">
        <div>
          <h2 class="text-sm font-semibold text-ink-soft uppercase">日志保留</h2>
          <p class="mt-1 text-xs text-ink-faint">
            服务启动时清理一次，之后每 24 小时按当前设置删除过期请求日志。
          </p>
        </div>

        <label class="flex max-w-sm flex-col gap-1.5">
          <span class="text-sm font-medium text-ink-soft">保留天数</span>
          <div class="flex items-center gap-2">
            <input
              v-model.number="retentionDays"
              type="number"
              min="1"
              max="3650"
              step="1"
              inputmode="numeric"
              class="glass-field tabular w-32 px-3 py-2 text-sm outline-none"
              :aria-invalid="!retentionValid"
            />
            <span class="text-sm text-ink-faint">天</span>
          </div>
          <span v-if="!retentionValid" class="text-xs text-danger" role="alert">
            请输入 1–3650 的整数。
          </span>
        </label>
      </section>

      <!-- 操作栏 -->
      <div class="mt-5 flex items-center gap-3">
        <button
          type="button"
          class="glass-button-primary px-5 py-2.5 text-sm font-medium disabled:opacity-50"
          :disabled="saving || !isDirty || !retentionValid || !breakerValid"
          @click="save"
        >
          {{ saving ? '保存中…' : '保存设置' }}
        </button>

        <Transition
          enter-active-class="transition-opacity duration-300"
          enter-from-class="opacity-0"
          leave-active-class="transition-opacity duration-200"
          leave-to-class="opacity-0"
        >
          <span v-if="saved" class="text-sm text-success">已保存</span>
        </Transition>

        <p v-if="saveError" class="ml-2 text-sm text-danger">{{ saveError }}</p>
      </div>

      <!-- 管理令牌 -->
      <section class="glass glass-specular mt-5 flex flex-col gap-4 p-5">
        <h2 class="text-sm font-semibold text-ink-soft uppercase">管理令牌</h2>
        <p class="text-xs text-ink-faint">
          启用后，管理界面与 /api 的所有请求都需要携带该令牌。服务端只保存哈希， 令牌本身无法读回 ——
          忘记或丢失时只能在此重新设置覆盖。
        </p>

        <p class="text-xs">
          <span v-if="hasLocalToken" class="text-success">本浏览器已保存令牌。</span>
          <span v-else class="text-ink-faint">本浏览器未保存令牌。</span>
        </p>

        <div class="relative">
          <input
            v-model="tokenDraft"
            :type="showTokenDraft ? 'text' : 'password'"
            placeholder="新令牌（启用或更换）"
            autocomplete="new-password"
            aria-label="新管理令牌"
            class="glass-field w-full px-3 py-2 pr-16 font-mono text-sm outline-none"
          />
          <button
            type="button"
            class="absolute top-1/2 right-2 -translate-y-1/2 rounded-md px-2 py-1 text-xs text-ink-faint hover:text-ink"
            :aria-label="showTokenDraft ? '隐藏管理令牌' : '显示管理令牌'"
            :aria-pressed="showTokenDraft"
            @click="showTokenDraft = !showTokenDraft"
          >
            {{ showTokenDraft ? '隐藏' : '显示' }}
          </button>
        </div>

        <div class="flex flex-wrap items-center gap-3">
          <button
            type="button"
            class="glass-button-primary px-4 py-2 text-sm font-medium disabled:opacity-50"
            :disabled="tokenBusy || !tokenDraft.trim()"
            @click="applyToken"
          >
            {{ tokenBusy ? '处理中…' : '启用或更换' }}
          </button>
          <button
            type="button"
            class="glass-button-ghost glass-button-ghost-danger px-4 py-2 text-sm"
            :disabled="tokenBusy"
            @click="clearToken"
          >
            关闭管理鉴权
          </button>
          <p
            v-if="tokenNotice"
            class="text-xs"
            :class="tokenNotice.tone === 'success' ? 'text-success' : 'text-danger'"
          >
            {{ tokenNotice.text }}
          </p>
        </div>
      </section>

      <!-- 数据备份 -->
      <section class="glass glass-specular mt-5 flex flex-col gap-4 p-5">
        <div>
          <h2 class="text-sm font-semibold text-ink-soft uppercase">数据备份</h2>
          <p class="mt-1 text-xs text-ink-faint">
            导出渠道、API 密钥与设置为一个 JSON 文件；可在另一个 Refract 实例导入恢复。
            导出文件含渠道凭据明文；网关密钥只含哈希，恢复后原密钥继续可用。
          </p>
        </div>

        <div class="flex flex-wrap items-center gap-3">
          <button
            type="button"
            class="glass-button-primary flex items-center gap-1.5 px-4 py-2 text-sm font-medium disabled:opacity-50"
            :disabled="exportBusy"
            @click="exportBackup"
          >
            <AppIcon name="download" :size="15" />
            {{ exportBusy ? '导出中…' : '导出备份' }}
          </button>

          <button
            type="button"
            class="glass-button-ghost px-4 py-2 text-sm font-medium"
            :disabled="importBusy"
            @click="importFileInput?.click()"
          >
            <AppIcon name="upload" :size="15" />
            {{ importBusy ? '导入中…' : '导入备份' }}
          </button>
          <input
            ref="importFileInput"
            type="file"
            accept="application/json,.json"
            class="hidden"
            aria-label="选择备份文件"
            @change="importBackup"
          />

          <div class="flex items-center gap-2 text-xs text-ink-soft">
            <label class="flex cursor-pointer items-center gap-1.5">
              <input
                v-model="importMode"
                type="radio"
                value="merge"
                name="import-mode"
                class="accent-[var(--color-accent)]"
              />
              合并（跳过同名）
            </label>
            <label class="flex cursor-pointer items-center gap-1.5">
              <input
                v-model="importMode"
                type="radio"
                value="replace"
                name="import-mode"
                class="accent-[var(--color-accent)]"
              />
              替换（清空后导入）
            </label>
          </div>
        </div>

        <div
          v-if="pendingReplace"
          class="flex flex-wrap items-center gap-3 rounded-lg border border-danger/30 bg-danger/8 px-4 py-3"
          role="alertdialog"
          aria-label="确认替换导入"
        >
          <p class="text-xs text-ink-soft">
            替换导入会<span class="font-semibold text-danger">先清空现有全部渠道与密钥</span
            >，且无法恢复。确定用「{{ pendingReplace.name }}」替换吗？
          </p>
          <div class="flex items-center gap-2">
            <button
              type="button"
              class="rounded-full bg-danger px-3.5 py-1.5 text-xs font-medium text-white hover:brightness-105 disabled:opacity-50"
              :disabled="importBusy"
              @click="confirmReplaceImport"
            >
              确认替换
            </button>
            <button
              type="button"
              class="glass-button-ghost px-3 py-1.5 text-xs"
              @click="pendingReplace = null"
            >
              取消
            </button>
          </div>
        </div>

        <p
          v-if="backupNotice"
          class="text-xs"
          :class="backupNotice.tone === 'success' ? 'text-success' : 'text-danger'"
          role="status"
        >
          {{ backupNotice.text }}
        </p>
      </section>
    </template>
  </div>
</template>
