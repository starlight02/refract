<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import AppIcon from '@/components/AppIcon.vue'
import GlassSpinner from '@/components/GlassSpinner.vue'
import { useAction } from '@/composables/useAction'
import * as m from '@/paraglide/messages'
import { getLocale } from '@/paraglide/runtime'
import { me, scopedLogs } from '@/api/client'
import { orElse } from '@/utils/effect'
import type { LedgerEntry, LedgerKind, TimeBucket, Wallet } from '@refract/contracts'

const load = useAction(m.wallet_load_failed())
const wallet = ref<Wallet | null>(null)
const entries = ref<LedgerEntry[]>([])
const series = ref<TimeBucket[]>([])

onMounted(async () => {
  await load.run(async () => {
    const [w, rows, buckets] = await Promise.all([
      me.wallet(),
      me.ledger({ limit: 100 }),
      scopedLogs('me').timeseries(720, 'day'),
    ])
    wallet.value = w
    entries.value = rows
    series.value = buckets
  })
})

const spend30 = computed(() => series.value.reduce((sum, b) => sum + b.cost, 0))

const CHART_W = 640
const CHART_H = 120
const CHART_PAD = 8
const spark = computed(() => {
  const buckets = series.value
  if (buckets.length < 2) return null
  const max = Math.max(...buckets.map((b) => b.cost), 0.0001)
  const step = (CHART_W - CHART_PAD * 2) / (buckets.length - 1)
  const y = (value: number) => CHART_H - CHART_PAD - (value / max) * (CHART_H - CHART_PAD * 2)
  return buckets
    .map((b, i) => `${(CHART_PAD + i * step).toFixed(1)},${y(b.cost).toFixed(1)}`)
    .join(' ')
})

function kindLabel(kind: LedgerKind): string {
  if (kind === 'topup') return m.wallet_kind_topup()
  if (kind === 'charge') return m.wallet_kind_charge()
  if (kind === 'refund') return m.wallet_kind_refund()
  return m.wallet_kind_adjust()
}

function fmtTime(iso: string): string {
  const d = new Date(iso)
  return Number.isNaN(d.getTime())
    ? iso
    : d.toLocaleString(getLocale() === 'zh-Hans' ? 'zh-CN' : 'en-US', { hour12: false })
}

async function exportLedger(format: 'csv' | 'ndjson') {
  await me.exportLedger(format)
}
</script>

<template>
  <div class="mx-auto max-w-6xl">
    <header class="mb-6">
      <h1 class="text-2xl font-semibold">{{ m.wallet_title() }}</h1>
      <p class="mt-1 text-sm text-ink-faint">{{ m.wallet_subtitle() }}</p>
    </header>

    <p v-if="load.error" class="glass mb-4 border-danger/30 p-4 text-sm text-danger">
      {{ load.error }}
    </p>

    <div v-if="load.busy && !wallet" class="py-24 text-center">
      <GlassSpinner size="lg" :label="m.common_loading()" />
    </div>

    <template v-else>
      <div class="mb-6 grid grid-cols-1 gap-4 sm:grid-cols-2">
        <div class="glass glass-specular p-5">
          <p class="text-xs font-medium uppercase text-ink-faint">{{ m.wallet_balance() }}</p>
          <p class="tabular mt-2 text-3xl font-semibold">
            ${{ (wallet?.balance ?? 0).toFixed(4) }}
            <span class="text-sm font-normal text-ink-faint">{{ wallet?.currency }}</span>
          </p>
        </div>
        <div class="glass glass-specular p-5">
          <p class="text-xs font-medium uppercase text-ink-faint">{{ m.wallet_spend_30d() }}</p>
          <p class="tabular mt-2 text-3xl font-semibold">${{ spend30.toFixed(4) }}</p>
        </div>
      </div>

      <section v-if="spark" class="glass glass-specular mb-6 p-5">
        <h2 class="mb-3 text-sm font-semibold text-ink-soft uppercase">
          {{ m.wallet_spend_30d() }}
        </h2>
        <svg :viewBox="`0 0 ${CHART_W} ${CHART_H}`" class="h-28 w-full">
          <polyline fill="none" stroke="var(--color-accent)" stroke-width="2" :points="spark" />
        </svg>
      </section>

      <section class="glass glass-specular p-5">
        <div class="mb-4 flex flex-wrap items-center justify-between gap-2">
          <h2 class="text-sm font-semibold text-ink-soft uppercase">{{ m.wallet_ledger() }}</h2>
          <div class="flex gap-2">
            <button
              type="button"
              class="glass-button-ghost px-3 py-1.5 text-xs"
              @click="exportLedger('csv')"
            >
              <AppIcon name="download" :size="13" />
              {{ m.wallet_export_csv() }}
            </button>
            <button
              type="button"
              class="glass-button-ghost px-3 py-1.5 text-xs"
              @click="exportLedger('ndjson')"
            >
              <AppIcon name="download" :size="13" />
              {{ m.wallet_export_ndjson() }}
            </button>
          </div>
        </div>
        <p v-if="entries.length === 0" class="py-8 text-center text-sm text-ink-faint">
          {{ m.wallet_empty() }}
        </p>
        <table v-else class="w-full text-sm">
          <tbody>
            <tr v-for="row in entries" :key="row.id" class="border-b border-ink/5 last:border-0">
              <td class="py-2.5 text-ink-faint">{{ fmtTime(row.created_at) }}</td>
              <td class="py-2.5">{{ kindLabel(row.kind) }}</td>
              <td class="py-2.5 text-ink-soft">{{ row.note }}</td>
              <td
                class="tabular py-2.5 text-right font-medium"
                :class="row.delta < 0 ? 'text-danger' : 'text-success'"
              >
                {{ row.delta > 0 ? '+' : '' }}{{ row.delta.toFixed(4) }}
              </td>
              <td class="tabular py-2.5 text-right text-ink-faint">
                ${{ row.balance_after.toFixed(4) }}
              </td>
            </tr>
          </tbody>
        </table>
      </section>
    </template>
  </div>
</template>
