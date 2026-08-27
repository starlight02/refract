<script setup lang="ts">
/**
 * 熔断策略：失败阈值与冷却退避。
 */
import SettingsSectionError from '@/components/settings/SettingsSectionError.vue'
import type { BreakerPolicy } from '@refract/contracts'

const breaker = defineModel<BreakerPolicy>({ required: true })

defineProps<{
  loadError?: string | null
  valid: boolean
}>()

const emit = defineEmits<{
  retry: []
}>()
</script>

<template>
  <section class="glass glass-specular mt-5 flex flex-col gap-4 p-5">
    <SettingsSectionError :message="loadError" @retry="emit('retry')" />
    <div>
      <h2 class="text-sm font-semibold text-ink-soft uppercase">熔断</h2>
      <p class="mt-1 text-xs text-ink-faint">
        端点连续失败达到阈值后暂停参与路由，冷却按指数退避直到上限；期间一次成功即恢复。 阈值设为 0
        关闭熔断。改动立即生效，已在冷却中的端点不受影响。
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

    <p v-if="!valid" class="text-xs text-danger" role="alert">
      阈值 0–1000；冷却 1–86400 秒，且上限不能小于起始值。
    </p>
  </section>
</template>
