<script setup lang="ts">
/**
 * 模型总览：网关当前对外提供哪些模型、各来自哪些渠道、按什么价格计费。
 *
 * 数据从渠道配置派生而不是单独维护 —— 模型清单的唯一真相是渠道配置，
 * 再存一份就会漂移。
 */
import { computed, onMounted, ref } from 'vue'
import { channels as channelsApi, settings as settingsApi } from '@/api/client'
import type { Channel, ModelPrice, Protocol } from '@/api/types'
import ProtocolBadge from '@/components/ProtocolBadge.vue'

const channels = ref<Channel[]>([])
const pricing = ref<ModelPrice[]>([])
const loading = ref(true)
const error = ref<string | null>(null)
const filter = ref('')

onMounted(async () => {
  try {
    const [chs, prices] = await Promise.all([channelsApi.list(), settingsApi.pricing()])
    channels.value = chs
    pricing.value = prices
  } catch (e) {
    error.value = e instanceof Error ? e.message : '加载失败'
  } finally {
    loading.value = false
  }
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
  for (const channel of channels.value) {
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
        <h1 class="text-2xl font-semibold">模型</h1>
        <p class="mt-1 text-sm text-ink-faint">
          由启用中的渠道派生。价格在设置页维护，单位为每百万 token。
        </p>
      </div>
      <input
        v-model="filter"
        type="search"
        placeholder="筛选模型名"
        aria-label="筛选模型名"
        class="glass-field w-56 px-3 py-2 text-sm outline-none"
      />
    </header>

    <p v-if="error" class="glass border-danger/30 p-4 text-sm text-danger">{{ error }}</p>
    <div v-else-if="loading" class="py-16 text-center text-sm text-ink-faint">加载中…</div>

    <section v-else-if="rows.length === 0" class="glass glass-specular py-16 text-center">
      <p class="text-sm text-ink-faint">
        {{ filter ? '没有匹配的模型' : '还没有渠道提供模型 —— 先去渠道页配置' }}
      </p>
    </section>

    <section v-else class="glass glass-specular overflow-x-auto">
      <table class="w-full min-w-[40rem] border-collapse text-sm">
        <thead>
          <tr class="text-left text-xs text-ink-faint">
            <th class="px-4 py-3 font-medium">模型</th>
            <th class="px-4 py-3 font-medium">协议</th>
            <th class="px-4 py-3 font-medium">渠道</th>
            <th class="px-4 py-3 text-right font-medium">输入 / M</th>
            <th class="px-4 py-3 text-right font-medium">输出 / M</th>
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
