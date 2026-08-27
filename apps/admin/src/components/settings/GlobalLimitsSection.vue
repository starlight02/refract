<script setup lang="ts">
/**
 * 网关级 RPM / TPM / 并发，以及单 IP RPM。
 */
import SettingsSectionError from '@/components/settings/SettingsSectionError.vue'
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
      <h2 class="text-sm font-semibold text-ink-soft uppercase">全局限制</h2>
      <p class="mt-1 text-xs text-ink-faint">
        网关级保险丝，对所有请求生效（包括免鉴权模式）。跑飞的本地 agent 循环不该原样打穿上游账单。0
        表示不限。
      </p>
    </div>

    <div class="grid max-w-xl grid-cols-1 gap-4 sm:grid-cols-2">
      <label class="flex flex-col gap-1.5">
        <span class="text-sm font-medium text-ink-soft">全局 RPM</span>
        <input
          v-model.number="limits.rpm"
          type="number"
          min="0"
          max="1000000"
          step="1"
          inputmode="numeric"
          aria-label="全局每分钟请求数上限"
          class="glass-field tabular px-3 py-2 text-sm outline-none"
        />
        <span class="text-xs text-ink-faint">每分钟请求数上限</span>
      </label>

      <label class="flex flex-col gap-1.5">
        <span class="text-sm font-medium text-ink-soft">全局 TPM</span>
        <input
          v-model.number="limits.tpm"
          type="number"
          min="0"
          max="1000000000"
          step="1000"
          inputmode="numeric"
          aria-label="全局每分钟 token 数上限"
          class="glass-field tabular px-3 py-2 text-sm outline-none"
        />
        <span class="text-xs text-ink-faint">
          每分钟 token 数上限。RPM 挡不住「少量请求 × 巨大上下文」
        </span>
      </label>

      <label class="flex flex-col gap-1.5">
        <span class="text-sm font-medium text-ink-soft">并发上限</span>
        <input
          v-model.number="limits.max_concurrency"
          type="number"
          min="0"
          max="100000"
          step="1"
          inputmode="numeric"
          aria-label="全局并发上限"
          class="glass-field tabular px-3 py-2 text-sm outline-none"
        />
        <span class="text-xs text-ink-faint">同时在途请求数（流式占用直到结束）</span>
      </label>

      <label class="flex flex-col gap-1.5">
        <span class="text-sm font-medium text-ink-soft">单 IP RPM</span>
        <input
          v-model.number="ipLimits.rpm"
          type="number"
          min="0"
          max="1000000"
          step="1"
          inputmode="numeric"
          aria-label="单 IP 每分钟请求数上限"
          class="glass-field tabular px-3 py-2 text-sm outline-none"
        />
        <span class="text-xs text-ink-faint">单 IP 每分钟请求上限，0 = 不限</span>
      </label>
    </div>

    <p v-if="!valid" class="text-xs text-danger" role="alert">
      RPM ≤ 1,000,000；TPM ≤ 1,000,000,000；并发 ≤ 100,000。
    </p>
    <p v-if="!ipValid" class="text-xs text-danger" role="alert">单 IP RPM ≤ 1,000,000。</p>
  </section>
</template>
