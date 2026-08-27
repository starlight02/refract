<script setup lang="ts">
/**
 * 模型价表：按百万 token 计价的模式匹配行。
 */
import AppIcon from '@/components/AppIcon.vue'
import SettingsSectionError from '@/components/settings/SettingsSectionError.vue'
import * as m from '@/paraglide/messages'
import type { ModelPrice } from '@refract/contracts'

const pricing = defineModel<ModelPrice[]>({ required: true })

defineProps<{
  loadError?: string | null
  valid: boolean
}>()

const emit = defineEmits<{
  retry: []
}>()

function addPriceRow() {
  pricing.value.push({
    pattern: '',
    input_per_m: 0,
    output_per_m: 0,
    cached_input_per_m: null,
    cache_write_per_m: null,
  })
}

/** number input 清空时 v-model.number 给空串；归一成 null（后端语义：回落输入价）。 */
function normalizePrice(row: ModelPrice, key: 'cached_input_per_m' | 'cache_write_per_m') {
  const value = row[key]
  if (value === undefined || (typeof value === 'string' && value === '') || Number.isNaN(value)) {
    row[key] = null
  }
}

function removePriceRow(index: number) {
  pricing.value.splice(index, 1)
}
</script>

<template>
  <section class="glass glass-specular mt-5 flex flex-col gap-4 p-5">
    <SettingsSectionError :message="loadError" @retry="emit('retry')" />
    <div>
      <h2 class="text-sm font-semibold text-ink-soft uppercase">
        {{ m.settings_pricing_title() }}
      </h2>
      <p class="mt-1 text-xs text-ink-faint">
        {{ m.settings_pricing_desc() }}
      </p>
    </div>

    <div v-if="pricing.length > 0" class="flex flex-col gap-2">
      <div
        class="grid grid-cols-[1fr_6rem_6rem_6rem_6rem_2.5rem] items-center gap-2 text-xs text-ink-faint"
      >
        <span>{{ m.settings_pricing_col_pattern() }}</span>
        <span class="text-right">{{ m.settings_pricing_col_input() }}</span>
        <span class="text-right">{{ m.settings_pricing_col_output() }}</span>
        <span class="text-right">{{ m.settings_pricing_col_cache_read() }}</span>
        <span class="text-right">{{ m.settings_pricing_col_cache_write() }}</span>
        <span></span>
      </div>
      <div
        v-for="(row, i) in pricing"
        :key="i"
        class="grid grid-cols-[1fr_6rem_6rem_6rem_6rem_2.5rem] items-center gap-2"
      >
        <input
          v-model="row.pattern"
          type="text"
          :placeholder="m.settings_pricing_pattern_placeholder()"
          :aria-label="m.settings_pricing_pattern_aria({ index: i + 1 })"
          class="glass-field px-3 py-2 font-mono text-xs outline-none"
        />
        <input
          v-model.number="row.input_per_m"
          type="number"
          min="0"
          step="0.01"
          :aria-label="m.settings_pricing_input_aria({ index: i + 1 })"
          class="glass-field tabular px-3 py-2 text-right text-xs outline-none"
        />
        <input
          v-model.number="row.output_per_m"
          type="number"
          min="0"
          step="0.01"
          :aria-label="m.settings_pricing_output_aria({ index: i + 1 })"
          class="glass-field tabular px-3 py-2 text-right text-xs outline-none"
        />
        <input
          v-model.number="row.cached_input_per_m"
          type="number"
          min="0"
          step="0.01"
          :placeholder="m.settings_pricing_fallback_input()"
          :aria-label="m.settings_pricing_cache_read_aria({ index: i + 1 })"
          class="glass-field tabular px-3 py-2 text-right text-xs outline-none"
          @change="normalizePrice(row, 'cached_input_per_m')"
        />
        <input
          v-model.number="row.cache_write_per_m"
          type="number"
          min="0"
          step="0.01"
          :placeholder="m.settings_pricing_fallback_input()"
          :aria-label="m.settings_pricing_cache_write_aria({ index: i + 1 })"
          class="glass-field tabular px-3 py-2 text-right text-xs outline-none"
          @change="normalizePrice(row, 'cache_write_per_m')"
        />
        <button
          type="button"
          class="glass-button-ghost glass-button-ghost-danger justify-center px-2 py-2"
          :aria-label="m.settings_pricing_del_aria({ index: i + 1 })"
          @click="removePriceRow(i)"
        >
          <AppIcon name="x" :size="13" />
        </button>
      </div>
    </div>

    <div>
      <button type="button" class="glass-button-ghost px-3 py-2 text-sm" @click="addPriceRow">
        <AppIcon name="plus" :size="14" />
        {{ m.settings_pricing_add_btn() }}
      </button>
    </div>

    <p v-if="!valid" class="text-xs text-danger" role="alert">{{ m.settings_pricing_val_err() }}</p>
  </section>
</template>
