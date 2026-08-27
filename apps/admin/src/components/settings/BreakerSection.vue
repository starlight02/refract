<script setup lang="ts">
/**
 * 熔断策略：失败阈值与冷却退避。
 */
import SettingsSectionError from '@/components/settings/SettingsSectionError.vue'
import * as m from '@/paraglide/messages'
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
      <h2 class="text-sm font-semibold text-ink-soft uppercase">
        {{ m.settings_breaker_title() }}
      </h2>
      <p class="mt-1 text-xs text-ink-faint">
        {{ m.settings_breaker_desc() }}
      </p>
    </div>

    <div class="grid max-w-lg grid-cols-1 gap-4 sm:grid-cols-3">
      <label class="flex flex-col gap-1.5">
        <span class="text-sm font-medium text-ink-soft">{{ m.settings_breaker_threshold() }}</span>
        <input
          v-model.number="breaker.failure_threshold"
          type="number"
          min="0"
          max="1000"
          step="1"
          inputmode="numeric"
          :aria-label="m.settings_breaker_threshold_aria()"
        />
        <span class="text-xs text-ink-faint">{{ m.settings_breaker_threshold_hint() }}</span>
      </label>

      <label class="flex flex-col gap-1.5">
        <span class="text-sm font-medium text-ink-soft">{{
          m.settings_breaker_base_cooldown()
        }}</span>
        <input
          v-model.number="breaker.base_cooldown_secs"
          type="number"
          min="1"
          max="86400"
          step="1"
          inputmode="numeric"
          :aria-label="m.settings_breaker_base_cooldown_aria()"
        />
        <span class="text-xs text-ink-faint">{{ m.settings_breaker_base_cooldown_hint() }}</span>
      </label>

      <label class="flex flex-col gap-1.5">
        <span class="text-sm font-medium text-ink-soft">{{
          m.settings_breaker_max_cooldown()
        }}</span>
        <input
          v-model.number="breaker.max_cooldown_secs"
          type="number"
          min="1"
          max="86400"
          step="1"
          inputmode="numeric"
          :aria-label="m.settings_breaker_max_cooldown_aria()"
        />
        <span class="text-xs text-ink-faint">{{ m.settings_breaker_max_cooldown_hint() }}</span>
      </label>
    </div>

    <p v-if="!valid" class="text-xs text-danger" role="alert">
      {{ m.settings_breaker_val_err() }}
    </p>
  </section>
</template>
