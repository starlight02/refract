<script setup lang="ts">
/**
 * 网关自身的 API 密钥。
 *
 * 关键交互约束：明文只在创建那一刻返回一次。所以创建成功后不是关弹窗，
 * 而是把弹窗切成「请立刻复制」状态，并且这个状态只能由用户显式关闭 ——
 * 自动消失会导致密钥永久丢失。
 */
import { computed, onMounted, ref } from 'vue'
import GlassSwitch from '@/components/GlassSwitch.vue'
import GlassSpinner from '@/components/GlassSpinner.vue'
import AppIcon from '@/components/AppIcon.vue'
import {
  DialogContent,
  DialogDescription,
  DialogOverlay,
  DialogPortal,
  DialogRoot,
  DialogTitle,
} from 'reka-ui'
import { useKeysStore } from '@/stores/keys'
import { logs as logsApi } from '@/api/client'
import { settle } from '@/utils/async'
import { toErrorMessage } from '@/utils/error'
import { numOr } from '@/utils/num'
import type { ApiKey, KeyUsageStat, NewApiKey } from '@refract/contracts'

const store = useKeysStore()

/** 每把密钥最近 24 小时的用量。读不到就不显示 —— 这是辅助信息。 */
const usageByKey = ref<Map<number, KeyUsageStat>>(new Map())

async function refreshUsage() {
  const stats = await settle(logsApi.byKey(24))
  usageByKey.value = new Map((stats ?? []).map((s) => [s.api_key_id, s]))
}

/** 弹窗状态机：关闭 → 填表 → 展示明文。 */
const dialog = ref<'closed' | 'form' | 'created'>('closed')
const creating = ref(false)
const createError = ref<string | null>(null)
const plaintext = ref('')
const copied = ref(false)
const pendingDelete = ref<number | null>(null)
/** 编辑目标；null 表示新建。 */
const editingId = ref<number | null>(null)

/** 表单草稿。逗号分隔文本，提交时切分。 */
const draft = ref({
  name: '',
  models: '',
  tags: '',
  quota: 0,
  budget: 0,
  rpm: 0,
  tpm: 0,
  note: '',
  expiresAt: '',
})

onMounted(() => {
  store.fetch()
  refreshUsage()
})

function openForm() {
  editingId.value = null
  draft.value = {
    name: '',
    models: '',
    tags: '',
    quota: 0,
    budget: 0,
    rpm: 0,
    tpm: 0,
    note: '',
    expiresAt: '',
  }
  createError.value = null
  dialog.value = 'form'
}

/** 编辑已有密钥：预填全部治理属性，提交走 update 而非 create。 */
function openEdit(key: ApiKey) {
  editingId.value = key.id
  draft.value = {
    name: key.name,
    models: key.allowed_models.join(', '),
    tags: key.allowed_tags.join(', '),
    quota: key.quota,
    budget: key.budget,
    rpm: key.rpm_limit,
    tpm: key.tpm_limit,
    note: key.note ?? '',
    expiresAt: key.expires_at ? toLocalInput(key.expires_at) : '',
  }
  createError.value = null
  dialog.value = 'form'
}

/** ISO 时间转 datetime-local 输入格式（本地时区，分钟精度）。 */
function toLocalInput(iso: string): string {
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return ''
  const pad = (n: number) => String(n).padStart(2, '0')
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`
}

function closeDialog() {
  dialog.value = 'closed'
  plaintext.value = ''
  copied.value = false
}

/**
 * 表单弹窗可用 Escape/遮罩关闭；一次性明文状态必须由用户显式确认关闭，
 * 否则一次误按 Escape 就会永久丢失密钥。
 */
function onDialogOpenChange(open: boolean) {
  if (!open && dialog.value === 'form') closeDialog()
}

function guardCreatedDismiss(event: Event) {
  if (dialog.value === 'created') event.preventDefault()
}
function splitList(raw: string): string[] {
  return raw
    .split(',')
    .map((s) => s.trim())
    .filter(Boolean)
}

async function submit() {
  if (!draft.value.name.trim()) return
  creating.value = true
  createError.value = null

  const spec: NewApiKey = {
    name: draft.value.name.trim(),
    allowed_models: splitList(draft.value.models),
    allowed_tags: splitList(draft.value.tags),
    // 数字输入清空归一成 0（= 不限额）；负数钳回 0，保持原 `> 0 ? : 0` 语义。
    quota: Math.max(0, numOr(draft.value.quota, 0)),
    budget: Math.max(0, numOr(draft.value.budget, 0)),
    rpm_limit: Math.max(0, numOr(draft.value.rpm, 0)),
    tpm_limit: Math.max(0, numOr(draft.value.tpm, 0)),
    note: draft.value.note.trim() || null,
    // datetime-local 给的是无时区字符串，补上秒并交给后端按 UTC 解析。
    expires_at: draft.value.expiresAt ? new Date(draft.value.expiresAt).toISOString() : null,
  }

  try {
    if (editingId.value !== null) {
      await store.update(editingId.value, spec)
      closeDialog()
    } else {
      const created = await store.create(spec)
      plaintext.value = created.plaintext
      dialog.value = 'created'
    }
  } catch (e) {
    createError.value = toErrorMessage(e, '保存失败')
  } finally {
    creating.value = false
  }
}

const resettingId = ref<number | null>(null)
const deletingId = ref<number | null>(null)

/** 重置已用配额（周期预算的手动形态）。 */
async function resetUsage(id: number) {
  if (resettingId.value !== null) return
  resettingId.value = id
  await settle(store.resetUsage(id))
  resettingId.value = null
}

async function copyPlaintext() {
  copied.value = (await settle(navigator.clipboard.writeText(plaintext.value))) !== undefined
}

async function destroy(id: number) {
  if (deletingId.value !== null) return
  deletingId.value = id
  try {
    await settle(store.remove(id))
  } finally {
    deletingId.value = null
    pendingDelete.value = null
  }
}

function usagePercent(key: ApiKey): number {
  if (key.quota <= 0) return 0
  return Math.min(100, (key.used_quota / key.quota) * 100)
}

const hasKeys = computed(() => store.items.length > 0)

function fmtTime(iso?: string | null): string {
  if (!iso) return '—'
  const d = new Date(iso)
  return Number.isNaN(d.getTime()) ? iso : d.toLocaleString('zh-CN', { hour12: false })
}

function isExpired(key: ApiKey): boolean {
  if (!key.expires_at) return false
  const d = new Date(key.expires_at)
  return !Number.isNaN(d.getTime()) && d.getTime() < Date.now()
}
</script>

<template>
  <div class="flex flex-col gap-5">
    <header class="flex flex-wrap items-end justify-between gap-4">
      <div>
        <h1 class="text-2xl font-semibold">API 密钥</h1>
        <p class="mt-1 text-sm text-ink-faint">
          客户端用这些密钥访问网关。留空模型/标签白名单表示不限制。
        </p>
      </div>
      <button
        type="button"
        class="glass-button-primary flex items-center gap-1.5 px-4 py-2.5 text-sm font-medium"
        @click="openForm"
      >
        <AppIcon name="plus" :size="15" />
        新建密钥
      </button>
    </header>

    <p v-if="store.error" class="glass border-danger/30 p-4 text-sm text-danger">
      {{ store.error }}
    </p>

    <div v-if="store.loading && !hasKeys" class="py-24 text-center">
      <GlassSpinner size="lg" label="正在读取 API 密钥列表…" />
    </div>

    <section v-else-if="!hasKeys" class="glass glass-specular py-16 text-center">
      <div class="shape-app-icon mx-auto grid size-14 place-items-center bg-ink/6 text-ink-faint">
        <AppIcon name="key" :size="26" />
      </div>
      <p class="mt-4 text-sm text-ink-faint">还没有密钥</p>
      <button
        type="button"
        class="glass-button-primary mt-4 px-4 py-2 text-sm font-medium"
        @click="openForm"
      >
        创建第一个
      </button>
    </section>

    <div v-else class="flex flex-col gap-3">
      <article
        v-for="key in store.items"
        :key="key.id"
        class="glass glass-specular glass-interactive p-4"
        :class="{ 'opacity-60': !key.enabled }"
      >
        <div class="flex flex-wrap items-start gap-4">
          <div class="min-w-0 flex-1">
            <div class="flex flex-wrap items-center gap-2">
              <h2 class="font-medium">{{ key.name }}</h2>
              <code class="rounded bg-ink/8 px-1.5 py-0.5 font-mono text-xs text-ink-soft">
                {{ key.key_prefix }}…
              </code>
              <span
                v-if="isExpired(key)"
                class="rounded-pill bg-danger/12 px-2 py-0.5 text-xs text-danger"
              >
                已过期
              </span>
              <span
                v-else-if="!key.enabled"
                class="rounded-pill bg-ink/10 px-2 py-0.5 text-xs text-ink-faint"
              >
                已停用
              </span>
            </div>

            <div class="mt-2 flex flex-wrap gap-x-5 gap-y-1 text-xs text-ink-faint">
              <span>创建 {{ fmtTime(key.created_at) }}</span>
              <span>最后使用 {{ fmtTime(key.last_used_at) }}</span>
              <span v-if="key.expires_at">过期 {{ fmtTime(key.expires_at) }}</span>
              <span v-if="key.rpm_limit > 0" class="tabular">{{ key.rpm_limit }} RPM</span>
              <span v-if="key.tpm_limit > 0" class="tabular">
                {{ key.tpm_limit.toLocaleString() }} TPM
              </span>
              <span v-if="key.budget > 0" class="tabular">
                预算 ${{ key.used_budget.toFixed(2)
                }}<span class="text-ink-faint">/{{ key.budget.toFixed(2) }}</span>
              </span>
            </div>

            <p v-if="key.note" class="mt-1.5 text-xs text-ink-soft">{{ key.note }}</p>

            <div
              v-if="usageByKey.get(key.id)"
              class="mt-1.5 flex flex-wrap gap-x-4 gap-y-1 text-xs"
            >
              <span class="text-ink-soft">
                24h
                <span class="tabular font-medium">{{
                  usageByKey.get(key.id)!.requests.toLocaleString()
                }}</span>
                次
              </span>
              <span v-if="usageByKey.get(key.id)!.failures > 0" class="tabular text-danger">
                失败 {{ usageByKey.get(key.id)!.failures.toLocaleString() }}
              </span>
              <span class="tabular text-ink-faint">
                ↑{{ usageByKey.get(key.id)!.input_tokens.toLocaleString() }} ↓{{
                  usageByKey.get(key.id)!.output_tokens.toLocaleString()
                }}
                tokens
              </span>
              <span v-if="usageByKey.get(key.id)!.cost > 0" class="tabular text-ink-soft">
                ${{ usageByKey.get(key.id)!.cost.toFixed(4) }}
              </span>
            </div>

            <div
              v-if="key.allowed_models.length > 0 || key.allowed_tags.length > 0"
              class="mt-2 flex flex-wrap gap-1.5"
            >
              <span
                v-for="m in key.allowed_models"
                :key="`m-${m}`"
                class="rounded bg-accent/10 px-1.5 py-0.5 font-mono text-xs text-accent-deep"
              >
                {{ m }}
              </span>
              <span
                v-for="t in key.allowed_tags"
                :key="`t-${t}`"
                class="rounded bg-ink/8 px-1.5 py-0.5 text-xs text-ink-soft"
              >
                #{{ t }}
              </span>
            </div>
          </div>

          <!-- 配额。
               有限配额：标签与数值分列两端，正好对齐下方进度条的起止。
               无限配额：没有进度条撑场，两端分离会让标签和数值看起来
               互不相干 —— 改成紧凑的一组。 -->
          <div v-if="key.quota > 0" class="w-full shrink-0 sm:w-40">
            <div class="flex items-baseline justify-between text-xs">
              <span class="text-ink-faint">配额</span>
              <span class="tabular">
                {{ key.used_quota.toLocaleString() }}
                <span class="text-ink-faint">/ {{ key.quota.toLocaleString() }}</span>
              </span>
            </div>
            <div class="mt-1.5 h-1.5 overflow-hidden rounded-pill bg-ink/10">
              <div
                class="h-full rounded-pill transition-[width] duration-500"
                :class="
                  usagePercent(key) > 90
                    ? 'bg-danger'
                    : usagePercent(key) > 70
                      ? 'bg-warning'
                      : 'bg-accent'
                "
                :style="{ width: `${usagePercent(key)}%` }"
              />
            </div>
          </div>
          <div v-else class="flex shrink-0 items-baseline gap-1.5 text-xs">
            <span class="text-ink-faint">配额</span>
            <span class="tabular">
              {{ key.used_quota.toLocaleString() }}
              <span class="text-ink-faint">/ ∞</span>
            </span>
          </div>

          <div class="flex shrink-0 flex-wrap items-center gap-2">
            <GlassSwitch
              :model-value="key.enabled"
              :label="key.enabled ? `禁用 ${key.name}` : `启用 ${key.name}`"
              tone="success"
              @update:model-value="store.toggleEnabled(key.id, $event)"
            />

            <button
              type="button"
              class="glass-button-ghost px-2.5 py-1.5 text-xs"
              @click="openEdit(key)"
            >
              <AppIcon name="pencil" :size="13" />
              编辑
            </button>

            <button
              v-if="key.used_quota > 0"
              type="button"
              class="glass-button-ghost px-2.5 py-1.5 text-xs"
              :disabled="resettingId === key.id"
              title="已用配额清零"
              @click="resetUsage(key.id)"
            >
              <AppIcon
                :name="resettingId === key.id ? 'spinner' : 'refresh'"
                :class="resettingId === key.id ? 'animate-spin' : ''"
                :size="13"
              />
              {{ resettingId === key.id ? '重置中…' : '重置用量' }}
            </button>

            <template v-if="pendingDelete === key.id">
              <button
                type="button"
                class="inline-flex items-center gap-1 rounded-lg bg-danger px-3 py-1.5 text-xs font-medium text-white hover:brightness-105 disabled:opacity-50"
                :disabled="deletingId === key.id"
                @click="destroy(key.id)"
              >
                <AppIcon
                  v-if="deletingId === key.id"
                  name="spinner"
                  class="animate-spin"
                  :size="12"
                />
                {{ deletingId === key.id ? '删除中…' : '确认删除' }}
              </button>
              <button
                type="button"
                class="glass-button-ghost px-2.5 py-1.5 text-xs"
                :disabled="deletingId === key.id"
                @click="pendingDelete = null"
              >
                取消
              </button>
            </template>
            <button
              v-else
              type="button"
              class="glass-button-ghost glass-button-ghost-danger px-2.5 py-1.5 text-xs !text-ink-faint hover:!text-danger"
              @click="pendingDelete = key.id"
            >
              <AppIcon name="trash" :size="13" />
              删除
            </button>
          </div>
        </div>
      </article>
    </div>

    <!-- 弹窗 -->
    <DialogRoot :open="dialog !== 'closed'" @update:open="onDialogOpenChange">
      <DialogPortal>
        <DialogOverlay
          class="fixed inset-0 z-50 bg-black/40 backdrop-blur-md data-[state=closed]:opacity-0 data-[state=open]:opacity-100"
        />
        <DialogContent
          class="glass-thick glass-specular fixed top-1/2 left-1/2 z-50 w-[calc(100%-2rem)] max-w-md -translate-x-1/2 -translate-y-1/2 !bg-canvas/95 p-6 shadow-2xl outline-none dark:!bg-[#12141c]/95"
          @escape-key-down="guardCreatedDismiss"
          @pointer-down-outside="guardCreatedDismiss"
        >
          <!-- 填表 -->
          <template v-if="dialog === 'form'">
            <DialogTitle class="text-lg font-semibold">
              {{ editingId !== null ? '编辑 API 密钥' : '新建 API 密钥' }}
            </DialogTitle>
            <DialogDescription class="mt-1 text-xs text-ink-faint">
              {{
                editingId !== null
                  ? '只改治理属性，密钥本体不变，客户端无需换钥匙。'
                  : '明文只会出现一次，创建后请立刻保存。'
              }}
            </DialogDescription>

            <form class="mt-5 flex flex-col gap-4" @submit.prevent="submit">
              <label class="flex flex-col gap-1.5">
                <span class="text-xs font-medium text-ink-soft">名称</span>
                <input
                  v-model="draft.name"
                  type="text"
                  placeholder="例如：本地开发"
                  class="glass-field px-3 py-2 text-sm outline-none"
                />
              </label>

              <label class="flex flex-col gap-1.5">
                <span class="text-xs font-medium text-ink-soft">
                  允许的模型<span class="font-normal text-ink-faint">，逗号分隔，留空不限</span>
                </span>
                <input
                  v-model="draft.models"
                  type="text"
                  placeholder="gpt-4o, claude-sonnet-4"
                  class="glass-field px-3 py-2 font-mono text-sm outline-none"
                />
              </label>

              <label class="flex flex-col gap-1.5">
                <span class="text-xs font-medium text-ink-soft">
                  允许的渠道标签<span class="font-normal text-ink-faint">，逗号分隔，留空不限</span>
                </span>
                <input
                  v-model="draft.tags"
                  type="text"
                  placeholder="生产"
                  class="glass-field px-3 py-2 text-sm outline-none"
                />
              </label>

              <div class="grid grid-cols-1 gap-3 sm:grid-cols-2">
                <label class="flex flex-col gap-1.5">
                  <span class="text-xs font-medium text-ink-soft">
                    配额<span class="font-normal text-ink-faint">，0 不限</span>
                  </span>
                  <input
                    v-model.number="draft.quota"
                    type="number"
                    min="0"
                    class="glass-field tabular px-3 py-2 text-sm outline-none"
                  />
                </label>

                <label class="flex flex-col gap-1.5">
                  <span class="text-xs font-medium text-ink-soft">
                    过期<span class="font-normal text-ink-faint">，留空永久</span>
                  </span>
                  <input
                    v-model="draft.expiresAt"
                    type="datetime-local"
                    class="glass-field px-3 py-2 text-sm outline-none"
                  />
                </label>

                <label class="flex flex-col gap-1.5">
                  <span class="text-xs font-medium text-ink-soft">
                    预算 $<span class="font-normal text-ink-faint">，按价表累计，0 不限</span>
                  </span>
                  <input
                    v-model.number="draft.budget"
                    type="number"
                    min="0"
                    step="0.01"
                    class="glass-field tabular px-3 py-2 text-sm outline-none"
                  />
                </label>

                <label class="flex flex-col gap-1.5">
                  <span class="text-xs font-medium text-ink-soft">
                    RPM<span class="font-normal text-ink-faint">，每分钟请求数，0 不限</span>
                  </span>
                  <input
                    v-model.number="draft.rpm"
                    type="number"
                    min="0"
                    class="glass-field tabular px-3 py-2 text-sm outline-none"
                  />
                </label>

                <label class="flex flex-col gap-1.5">
                  <span class="text-xs font-medium text-ink-soft">
                    TPM<span class="font-normal text-ink-faint">，每分钟 token 数，0 不限</span>
                  </span>
                  <input
                    v-model.number="draft.tpm"
                    type="number"
                    min="0"
                    class="glass-field tabular px-3 py-2 text-sm outline-none"
                  />
                </label>
              </div>

              <label class="flex flex-col gap-1.5">
                <span class="text-xs font-medium text-ink-soft">
                  备注<span class="font-normal text-ink-faint">，仅自己可见</span>
                </span>
                <input
                  v-model="draft.note"
                  type="text"
                  placeholder="给 Cursor 用的"
                  class="glass-field px-3 py-2 text-sm outline-none"
                />
              </label>

              <p v-if="createError" class="text-sm text-danger">{{ createError }}</p>

              <div class="mt-1 flex items-center gap-3">
                <button
                  type="submit"
                  class="glass-button-primary px-4 py-2.5 text-sm font-medium disabled:opacity-50"
                  :disabled="creating || !draft.name.trim()"
                >
                  <AppIcon v-if="creating" name="spinner" class="animate-spin mr-1" :size="14" />
                  {{ creating ? '保存中…' : editingId !== null ? '保存' : '创建' }}
                </button>
                <button
                  type="button"
                  class="glass-button-ghost px-3 py-2.5 text-sm"
                  @click="closeDialog"
                >
                  取消
                </button>
              </div>
            </form>
          </template>

          <!-- 展示明文 -->
          <template v-else-if="dialog === 'created'">
            <DialogTitle class="text-lg font-semibold">密钥已创建</DialogTitle>
            <DialogDescription class="mt-1 text-xs text-warning">
              这是唯一一次显示完整密钥。关闭后无法再取回。
            </DialogDescription>

            <div class="mt-4 rounded-lg bg-ink/8 p-3">
              <code class="block font-mono text-sm break-all select-all">{{ plaintext }}</code>
            </div>

            <div class="mt-4 flex items-center gap-3">
              <button
                type="button"
                class="glass-button-primary px-4 py-2.5 text-sm font-medium"
                @click="copyPlaintext"
              >
                {{ copied ? '已复制' : '复制' }}
              </button>
              <button
                type="button"
                class="glass-button-ghost px-3 py-2.5 text-sm"
                @click="closeDialog"
              >
                我已保存，关闭
              </button>
            </div>
          </template>
        </DialogContent>
      </DialogPortal>
    </DialogRoot>
  </div>
</template>
