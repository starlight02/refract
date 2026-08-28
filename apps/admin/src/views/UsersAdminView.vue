<script setup lang="ts">
import { computed, onMounted, reactive, ref } from 'vue'
import AppIcon from '@/components/AppIcon.vue'
import GlassSpinner from '@/components/GlassSpinner.vue'
import { useAction } from '@/composables/useAction'
import * as m from '@/paraglide/messages'
import { getLocale } from '@/paraglide/runtime'
import { users } from '@/api/client'
import { orElse } from '@/utils/effect'
import type { LedgerEntry, UserListItem, UserRole, UserStatus } from '@refract/contracts'
import {
  DialogContent,
  DialogDescription,
  DialogOverlay,
  DialogPortal,
  DialogRoot,
  DialogTitle,
} from 'reka-ui'

const items = ref<UserListItem[]>([])
const loading = ref(false)
const load = useAction(m.users_load_failed())
const save = reactive(useAction(m.users_save_failed(), { toast: true }))

const dialog = ref<'closed' | 'create' | 'wallet' | 'ledger'>('closed')
const walletMode = ref<'topup' | 'adjust' | 'refund'>('topup')
const target = ref<UserListItem | null>(null)
const ledger = ref<LedgerEntry[]>([])

const draft = ref({
  email: '',
  password: '',
  display_name: '',
  role: 'user' as UserRole,
  initial_balance: 0,
  amount: 0,
  note: '',
})

onMounted(() => {
  void refresh()
})

async function refresh() {
  loading.value = true
  const rows = await orElse(() => users.list({ limit: 100 }))
  if (rows) items.value = rows
  loading.value = false
}

function openCreate() {
  draft.value = {
    email: '',
    password: '',
    display_name: '',
    role: 'user',
    initial_balance: 0,
    amount: 0,
    note: '',
  }
  save.clear()
  dialog.value = 'create'
}

function openWallet(user: UserListItem, mode: 'topup' | 'adjust' | 'refund') {
  target.value = user
  walletMode.value = mode
  draft.value.amount = 0
  draft.value.note = ''
  save.clear()
  dialog.value = 'wallet'
}

async function openLedger(user: UserListItem) {
  target.value = user
  ledger.value = (await orElse(() => users.ledger(user.id, { limit: 50 }))) ?? []
  dialog.value = 'ledger'
}

async function createUser() {
  await save.run(
    async () => {
      const created = await users.create({
        email: draft.value.email.trim(),
        password: draft.value.password,
        display_name: draft.value.display_name.trim() || undefined,
        role: draft.value.role,
        initial_balance: draft.value.initial_balance || undefined,
      })
      dialog.value = 'closed'
      await refresh()
      return created
    },
    () => m.common_saved(),
  )
}

async function toggleStatus(user: UserListItem) {
  await save.run(async () => {
    const result = await (user.status === 'disabled'
      ? users.enable(user.id)
      : users.disable(user.id))
    await refresh()
    return result
  })
}

async function setRole(user: UserListItem, role: UserRole) {
  await save.run(async () => {
    const result = await users.update(user.id, { role })
    await refresh()
    return result
  })
}

async function applyWallet() {
  const user = target.value
  if (!user) return
  const amount = draft.value.amount
  const note = draft.value.note.trim()
  await save.run(
    async () => {
      const result =
        walletMode.value === 'topup'
          ? await users.topup(user.id, amount, note)
          : walletMode.value === 'refund'
            ? await users.refund(user.id, amount, note)
            : await users.adjust(user.id, amount, note)
      dialog.value = 'closed'
      await refresh()
      return result
    },
    () => m.common_saved(),
  )
}

function statusLabel(status: UserStatus): string {
  if (status === 'active') return m.users_status_active()
  if (status === 'disabled') return m.users_status_disabled()
  return m.users_status_pending_verification()
}

function onRoleChange(user: UserListItem, event: Event) {
  const el = event.target
  if (el instanceof HTMLSelectElement) void setRole(user, el.value as UserRole)
}

function fmtTime(iso?: string): string {
  if (!iso) return '—'
  const d = new Date(iso)
  return Number.isNaN(d.getTime())
    ? iso
    : d.toLocaleString(getLocale() === 'zh-Hans' ? 'zh-CN' : 'en-US', { hour12: false })
}

const walletTitle = computed(() => {
  if (walletMode.value === 'topup') return m.users_topup()
  if (walletMode.value === 'refund') return m.users_refund()
  return m.users_adjust()
})
</script>

<template>
  <div class="mx-auto max-w-6xl">
    <header class="mb-6 flex flex-wrap items-end justify-between gap-4">
      <div>
        <h1 class="text-2xl font-semibold">{{ m.users_title() }}</h1>
        <p class="mt-1 text-sm text-ink-faint">{{ m.users_subtitle() }}</p>
      </div>
      <button
        type="button"
        class="glass-button-primary flex items-center gap-1.5 px-4 py-2.5 text-sm font-medium"
        @click="openCreate"
      >
        <AppIcon name="plus" :size="15" />
        {{ m.users_new() }}
      </button>
    </header>

    <p v-if="load.error || save.error" class="glass mb-4 border-danger/30 p-4 text-sm text-danger">
      {{ load.error || save.error }}
    </p>

    <div v-if="loading && items.length === 0" class="py-24 text-center">
      <GlassSpinner size="lg" :label="m.common_loading()" />
    </div>

    <section v-else-if="items.length === 0" class="glass glass-specular py-16 text-center">
      <p class="text-sm text-ink-faint">{{ m.users_empty() }}</p>
    </section>

    <section v-else class="glass glass-specular overflow-x-auto">
      <table class="min-w-[720px] w-full text-sm">
        <thead>
          <tr class="border-b border-ink/10 text-left text-xs text-ink-faint">
            <th class="px-4 py-3 font-medium">{{ m.users_email() }}</th>
            <th class="px-4 py-3 font-medium">{{ m.users_role() }}</th>
            <th class="px-4 py-3 font-medium">{{ m.users_status() }}</th>
            <th class="px-4 py-3 text-right font-medium">{{ m.users_balance() }}</th>
            <th class="px-4 py-3 font-medium">{{ m.users_created() }}</th>
            <th class="px-4 py-3 font-medium">{{ m.common_actions() }}</th>
          </tr>
        </thead>
        <tbody>
          <tr v-for="user in items" :key="user.id" class="border-b border-ink/5 last:border-0">
            <td class="px-4 py-3">
              <div class="font-medium">{{ user.display_name || user.email }}</div>
              <div class="text-xs text-ink-faint">{{ user.email }}</div>
            </td>
            <td class="px-4 py-3">
              <select
                class="glass-field px-2 py-1 text-xs outline-none"
                :value="user.role"
                @change="onRoleChange(user, $event)"
              >
                <option value="user">{{ m.users_role_user() }}</option>
                <option value="admin">{{ m.users_role_admin() }}</option>
              </select>
            </td>
            <td class="px-4 py-3 text-xs">{{ statusLabel(user.status) }}</td>
            <td class="tabular px-4 py-3 text-right">${{ user.balance.toFixed(4) }}</td>
            <td class="px-4 py-3 text-xs text-ink-faint">{{ fmtTime(user.created_at) }}</td>
            <td class="px-4 py-3">
              <div class="flex flex-wrap gap-1.5">
                <button
                  type="button"
                  class="glass-button-ghost px-2 py-1 text-xs"
                  @click="openWallet(user, 'topup')"
                >
                  {{ m.users_topup() }}
                </button>
                <button
                  type="button"
                  class="glass-button-ghost px-2 py-1 text-xs"
                  @click="openWallet(user, 'adjust')"
                >
                  {{ m.users_adjust() }}
                </button>
                <button
                  type="button"
                  class="glass-button-ghost px-2 py-1 text-xs"
                  @click="openLedger(user)"
                >
                  {{ m.users_ledger() }}
                </button>
                <button
                  type="button"
                  class="glass-button-ghost px-2 py-1 text-xs"
                  @click="toggleStatus(user)"
                >
                  {{ user.status === 'disabled' ? m.users_enable() : m.users_disable() }}
                </button>
              </div>
            </td>
          </tr>
        </tbody>
      </table>
    </section>

    <DialogRoot :open="dialog !== 'closed'" @update:open="(open) => !open && (dialog = 'closed')">
      <DialogPortal>
        <DialogOverlay class="fixed inset-0 z-50 bg-ink/25 backdrop-blur-sm" />
        <DialogContent
          class="glass-thick glass-specular fixed top-1/2 left-1/2 z-50 w-[calc(100%-2rem)] max-w-md -translate-x-1/2 -translate-y-1/2 p-6 outline-none"
        >
          <template v-if="dialog === 'create'">
            <DialogTitle class="text-lg font-semibold">{{ m.users_new() }}</DialogTitle>
            <DialogDescription class="sr-only">{{ m.users_subtitle() }}</DialogDescription>
            <form class="mt-4 flex flex-col gap-3" @submit.prevent="createUser">
              <input
                v-model="draft.email"
                type="email"
                required
                :placeholder="m.users_email()"
                class="glass-field px-3 py-2 text-sm outline-none"
              />
              <input
                v-model="draft.display_name"
                type="text"
                :placeholder="m.users_email()"
                class="glass-field px-3 py-2 text-sm outline-none"
              />
              <input
                v-model="draft.password"
                type="password"
                required
                minlength="10"
                :placeholder="m.users_password()"
                class="glass-field px-3 py-2 text-sm outline-none"
              />
              <select v-model="draft.role" class="glass-field px-3 py-2 text-sm outline-none">
                <option value="user">{{ m.users_role_user() }}</option>
                <option value="admin">{{ m.users_role_admin() }}</option>
              </select>
              <input
                v-model.number="draft.initial_balance"
                type="number"
                min="0"
                step="0.01"
                :placeholder="m.users_initial_balance()"
                class="glass-field tabular px-3 py-2 text-sm outline-none"
              />
              <button
                type="submit"
                class="glass-button-primary px-4 py-2 text-sm"
                :disabled="save.busy"
              >
                {{ m.users_create() }}
              </button>
            </form>
          </template>
          <template v-else-if="dialog === 'wallet'">
            <DialogTitle class="text-lg font-semibold">{{ walletTitle }}</DialogTitle>
            <DialogDescription class="mt-1 text-xs text-ink-faint">{{
              target?.email
            }}</DialogDescription>
            <form class="mt-4 flex flex-col gap-3" @submit.prevent="applyWallet">
              <input
                v-model.number="draft.amount"
                type="number"
                step="0.0001"
                required
                :placeholder="walletMode === 'adjust' ? m.users_delta() : m.users_amount()"
                class="glass-field tabular px-3 py-2 text-sm outline-none"
              />
              <input
                v-model="draft.note"
                type="text"
                :placeholder="m.users_note()"
                class="glass-field px-3 py-2 text-sm outline-none"
              />
              <button
                type="submit"
                class="glass-button-primary px-4 py-2 text-sm"
                :disabled="save.busy"
              >
                {{ m.common_confirm() }}
              </button>
            </form>
          </template>
          <template v-else>
            <DialogTitle class="text-lg font-semibold">{{ m.users_ledger() }}</DialogTitle>
            <DialogDescription class="mt-1 text-xs text-ink-faint">{{
              target?.email
            }}</DialogDescription>
            <div class="mt-4 max-h-80 overflow-y-auto text-sm">
              <p v-if="ledger.length === 0" class="text-ink-faint">{{ m.wallet_empty() }}</p>
              <div
                v-for="row in ledger"
                :key="row.id"
                class="flex justify-between border-b border-ink/5 py-2"
              >
                <span>{{ row.kind }} · {{ row.note }}</span>
                <span class="tabular" :class="row.delta < 0 ? 'text-danger' : 'text-success'">
                  {{ row.delta.toFixed(4) }}
                </span>
              </div>
            </div>
          </template>
        </DialogContent>
      </DialogPortal>
    </DialogRoot>
  </div>
</template>
