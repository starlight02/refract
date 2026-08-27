<script setup lang="ts">
/**
 * 模型价表：按百万 token 计价的模式匹配行。
 */
import AppIcon from '@/components/AppIcon.vue'
import SettingsSectionError from '@/components/settings/SettingsSectionError.vue'
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
      <h2 class="text-sm font-semibold text-ink-soft uppercase">模型价表</h2>
      <p class="mt-1 text-xs text-ink-faint">
        按「每百万 token」计价，币种自定。模式支持精确模型名或以 * 结尾的前缀
        （精确名优先，其后取最长前缀）。缓存读/写价留空按输入价计（不打折）。
        请求日志按落库当时的价表固化成本。
      </p>
    </div>

    <div v-if="pricing.length > 0" class="flex flex-col gap-2">
      <div
        class="grid grid-cols-[1fr_6rem_6rem_6rem_6rem_2.5rem] items-center gap-2 text-xs text-ink-faint"
      >
        <span>模式</span>
        <span class="text-right">输入 / M</span>
        <span class="text-right">输出 / M</span>
        <span class="text-right">缓存读 / M</span>
        <span class="text-right">缓存写 / M</span>
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
          placeholder="gpt-4o 或 gpt-4o*"
          :aria-label="`价表第 ${i + 1} 行模式`"
          class="glass-field px-3 py-2 font-mono text-xs outline-none"
        />
        <input
          v-model.number="row.input_per_m"
          type="number"
          min="0"
          step="0.01"
          :aria-label="`价表第 ${i + 1} 行输入单价`"
          class="glass-field tabular px-3 py-2 text-right text-xs outline-none"
        />
        <input
          v-model.number="row.output_per_m"
          type="number"
          min="0"
          step="0.01"
          :aria-label="`价表第 ${i + 1} 行输出单价`"
          class="glass-field tabular px-3 py-2 text-right text-xs outline-none"
        />
        <input
          v-model.number="row.cached_input_per_m"
          type="number"
          min="0"
          step="0.01"
          placeholder="=输入"
          :aria-label="`价表第 ${i + 1} 行缓存读单价`"
          class="glass-field tabular px-3 py-2 text-right text-xs outline-none"
          @change="normalizePrice(row, 'cached_input_per_m')"
        />
        <input
          v-model.number="row.cache_write_per_m"
          type="number"
          min="0"
          step="0.01"
          placeholder="=输入"
          :aria-label="`价表第 ${i + 1} 行缓存写单价`"
          class="glass-field tabular px-3 py-2 text-right text-xs outline-none"
          @change="normalizePrice(row, 'cache_write_per_m')"
        />
        <button
          type="button"
          class="glass-button-ghost glass-button-ghost-danger justify-center px-2 py-2"
          :aria-label="`删除价表第 ${i + 1} 行`"
          @click="removePriceRow(i)"
        >
          <AppIcon name="x" :size="13" />
        </button>
      </div>
    </div>

    <div>
      <button type="button" class="glass-button-ghost px-3 py-2 text-sm" @click="addPriceRow">
        <AppIcon name="plus" :size="14" />
        添加规则
      </button>
    </div>

    <p v-if="!valid" class="text-xs text-danger" role="alert">模式不能为空，价格必须是非负数字。</p>
  </section>
</template>
