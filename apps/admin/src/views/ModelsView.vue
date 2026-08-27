<script setup lang="ts">
/**
 * 模型总览：网关当前对外提供哪些模型、各来自哪些渠道、按什么价格计费。
 *
 * 数据从渠道配置派生而不是单独维护 —— 模型清单的唯一真相是渠道配置，
 * 再存一份就会漂移。
 */
import { computed, onMounted, reactive, ref } from 'vue'
import { settings as settingsApi } from '@/api/client'
import { useChannelsStore } from '@/stores/channels'
import { useAction } from '@/composables/useAction'
import * as m from '@/paraglide/messages'
import type { ModelPrice, Protocol } from '@refract/contracts'
import ProtocolBadge from '@/components/ProtocolBadge.vue'
import GlassSpinner from '@/components/GlassSpinner.vue'
const channelsStore = useChannelsStore()
const pricing = ref<ModelPrice[]>([])
const loadModels = reactive(useAction(m.models_load_failed()))
loadModels.busy = true
const filter = ref('')

onMounted(async () => {
  await loadModels.run(async () => {
    const [, prices] = await Promise.all([channelsStore.fetch(), settingsApi.pricing()])
    pricing.value = prices
    if (channelsStore.error) loadModels.fail(channelsStore.error)
  })
})

interface ModelRow {
  name: string
  channels: string[]
  protocols: Protocol[]
  price?: ModelPrice
}

/** 与后端 `price_for` 同语义：精确名优先，其后最长前缀通配。 */
function priceFor(model: string): ModelPrice | undefined {
  let best: ModelPrice | undefined
  for (const price of pricing.value) {
    const prefix = price.pattern.endsWith('*') ? price.pattern.slice(0, -1) : null
    if (prefix === null) {
      if (price.pattern === model) return price
    } else if (
      model.startsWith(prefix) &&
      (best === undefined || price.pattern.length > best.pattern.length)
    ) {
      best = price
    }
  }
  return best
}

const rows = computed<ModelRow[]>(() => {
  const byName = new Map<string, { channels: Set<string>; protocols: Set<Protocol> }>()
  for (const channel of channelsStore.items) {
    if (!channel.enabled) continue
    for (const endpoint of channel.endpoints) {
      if (!endpoint.enabled) continue
      for (const entry of endpoint.models) {
        let slot = byName.get(entry.name)
        if (!slot) {
          slot = { channels: new Set(), protocols: new Set() }
          byName.set(entry.name, slot)
        }
        slot.channels.add(channel.name)
        slot.protocols.add(endpoint.protocol)
      }
    }
  }
  const needle = filter.value.trim().toLowerCase()
  return [...byName.entries()]
    .filter(([name]) => needle === '' || name.toLowerCase().includes(needle))
    .sort(([a], [b]) => a.localeCompare(b))
    .map(([name, slot]) => ({
      name,
      channels: [...slot.channels].sort(),
      protocols: [...slot.protocols].sort() as Protocol[],
      price: priceFor(name),
    }))
})

function fmtPrice(value: number): string {
  return value.toLocaleString(undefined, { maximumFractionDigits: 4 })
}
</script>

<template>
  <div class="flex flex-col gap-5">
    <header class="flex flex-wrap items-end justify-between gap-4">
      <div>
        <h1 class="text-2xl font-semibold">{{ m.models_title() }}</h1>
        <p class="mt-1 text-sm text-ink-faint">
          {{ m.models_subtitle() }}
        </p>
      </div>
      <input
        v-model="filter"
        type="search"
        :placeholder="m.models_filter_placeholder()"
        :aria-label="m.models_filter_aria()"
        class="glass-field w-56 outline-none"
      />
    </header>

    <p v-if="loadModels.error" class="glass border-danger/30 p-4 text-sm text-danger">
      {{ loadModels.error }}
    </p>
    <div v-else-if="loadModels.busy" class="py-24 text-center">
      <GlassSpinner size="lg" :label="m.models_loading()" />
    </div>
    <section v-else-if="rows.length === 0" class="glass glass-specular py-16 text-center">
      <p class="text-sm text-ink-faint">
        {{ filter ? m.models_no_match() : m.models_no_channels() }}
      </p>
    </section>

    <section v-else class="glass glass-specular overflow-x-auto">
      <table class="w-full min-w-[40rem] border-collapse text-sm">
        <thead>
          <tr class="text-left text-xs text-ink-faint">
            <th class="px-4 py-3 font-medium">{{ m.models_col_model() }}</th>
            <th class="px-4 py-3 font-medium">{{ m.models_col_protocol() }}</th>
            <th class="px-4 py-3 font-medium">{{ m.models_col_channel() }}</th>
            <th class="px-4 py-3 text-right font-medium">{{ m.models_col_input_price() }}</th>
            <th class="px-4 py-3 text-right font-medium">{{ m.models_col_output_price() }}</th>
          </tr>
        </thead>
        <tbody>
          <tr
            v-for="row in rows"
            :key="row.name"
            class="border-t border-ink/6 transition-colors hover:bg-ink/3"
          >
            <td class="px-4 py-2.5 font-mono text-xs">{{ row.name }}</td>
            <td class="px-4 py-2.5">
              <span class="inline-flex flex-wrap gap-1">
                <ProtocolBadge v-for="p in row.protocols" :key="p" :protocol="p" />
              </span>
            </td>
            <td class="px-4 py-2.5 text-xs text-ink-soft">
              {{ row.channels.join('、') }}
            </td>
            <td class="tabular px-4 py-2.5 text-right text-xs">
              {{ row.price ? fmtPrice(row.price.input_per_m) : '—' }}
            </td>
            <td class="tabular px-4 py-2.5 text-right text-xs">
              {{ row.price ? fmtPrice(row.price.output_per_m) : '—' }}
            </td>
          </tr>
        </tbody>
      </table>
    </section>
  </div>
</template>
