<script setup lang="ts">
/**
 * 网关级 RPM / TPM / 并发，以及单 IP RPM。
 */
import SettingsSectionError from '@/components/settings/SettingsSectionError.vue'
import * as m from '@/paraglide/messages'
import type { GlobalLimits, IpLimits } from '@refract/contracts'

const limits = defineModel<GlobalLimits>({ required: true })
const ipLimits = defineModel<IpLimits>('ipLimits', { required: true })

defineProps<{
  loadError?: string | null
  valid: boolean
  ipValid: boolean
}>()

const emit = defineEmits<{
  retry: []
}>()
</script>

<template>
  <section class="glass glass-specular mt-5 flex flex-col gap-4 p-5">
    <SettingsSectionError :message="loadError" @retry="emit('retry')" />
    <div>
      <h2 class="text-sm font-semibold text-ink-soft uppercase">{{ m.settings_limits_title() }}</h2>
      <p class="mt-1 text-xs text-ink-faint">
        {{ m.settings_limits_desc() }}
      </p>
    </div>

    <div class="grid max-w-xl grid-cols-1 gap-4 sm:grid-cols-2">
      <label class="flex flex-col gap-1.5">
        <span class="text-sm font-medium text-ink-soft">{{ m.settings_limits_rpm() }}</span>
        <input
          v-model.number="limits.rpm"
          type="number"
          min="0"
          max="1000000"
          step="1"
          inputmode="numeric"
          :aria-label="m.settings_limits_rpm_aria()"
          class="glass-field tabular px-3 py-2 text-sm outline-none"
        />
        <span class="text-xs text-ink-faint">{{ m.settings_limits_rpm_hint() }}</span>
      </label>

      <label class="flex flex-col gap-1.5">
        <span class="text-sm font-medium text-ink-soft">{{ m.settings_limits_tpm() }}</span>
        <input
          v-model.number="limits.tpm"
          type="number"
          min="0"
          max="1000000000"
          step="1000"
          inputmode="numeric"
          :aria-label="m.settings_limits_tpm_aria()"
          class="glass-field tabular px-3 py-2 text-sm outline-none"
        />
        <span class="text-xs text-ink-faint">
          {{ m.settings_limits_tpm_hint() }}
        </span>
      </label>

      <label class="flex flex-col gap-1.5">
        <span class="text-sm font-medium text-ink-soft">{{ m.settings_limits_concurrency() }}</span>
        <input
          v-model.number="limits.max_concurrency"
          type="number"
          min="0"
          max="100000"
          step="1"
          inputmode="numeric"
          :aria-label="m.settings_limits_concurrency_aria()"
          class="glass-field tabular px-3 py-2 text-sm outline-none"
        />
        <span class="text-xs text-ink-faint">{{ m.settings_limits_concurrency_hint() }}</span>
      </label>

      <label class="flex flex-col gap-1.5">
        <span class="text-sm font-medium text-ink-soft">{{ m.settings_limits_ip_rpm() }}</span>
        <input
          v-model.number="ipLimits.rpm"
          type="number"
          min="0"
          max="1000000"
          step="1"
          inputmode="numeric"
          :aria-label="m.settings_limits_ip_rpm_aria()"
          class="glass-field tabular px-3 py-2 text-sm outline-none"
        />
        <span class="text-xs text-ink-faint">{{ m.settings_limits_ip_rpm_hint() }}</span>
      </label>
    </div>

    <p v-if="!valid" class="text-xs text-danger" role="alert">
      {{ m.settings_limits_val_err() }}
    </p>
    <p v-if="!ipValid" class="text-xs text-danger" role="alert">
      {{ m.settings_limits_ip_val_err() }}
    </p>
  </section>
</template>
