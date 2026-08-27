<script setup lang="ts">
/**
 * 路由策略：原生优先、选择模式、重试，以及上游 200 空回复重试。
 */
import GlassSwitch from '@/components/GlassSwitch.vue'
import SettingsSectionError from '@/components/settings/SettingsSectionError.vue'
import * as m from '@/paraglide/messages'
import type { EmptyResponseRetryPolicy, RoutingPolicy, SelectionMode } from '@refract/contracts'

const policy = defineModel<RoutingPolicy>({ required: true })
const emptyResponseRetry = defineModel<EmptyResponseRetryPolicy>('emptyRetry', { required: true })

defineProps<{
  loadError?: string | null
  emptyRetryError?: string | null
  valid: boolean
  emptyRetryValid: boolean
}>()

const emit = defineEmits<{
  retry: []
  retryEmptyRetry: []
}>()

const SELECTION_OPTIONS: { value: SelectionMode; label: () => string; desc: () => string }[] = [
  {
    value: 'weighted_random',
    label: m.settings_sel_weighted,
    desc: m.settings_sel_weighted_desc,
  },
  { value: 'round_robin', label: m.settings_sel_rr, desc: m.settings_sel_rr_desc },
  { value: 'first', label: m.settings_sel_first, desc: m.settings_sel_first_desc },
]
</script>

<template>
  <section class="glass glass-specular flex flex-col gap-5 p-5">
    <SettingsSectionError :message="loadError" @retry="emit('retry')" />
    <h2 class="text-sm font-semibold text-ink-soft uppercase">{{ m.settings_routing_title() }}</h2>

    <!-- 原生优先（需求 6） -->
    <label class="flex cursor-pointer items-center gap-3">
      <GlassSwitch v-model="policy.native_first" :label="m.settings_native_first()" />
      <div>
        <span class="text-sm font-medium">{{ m.settings_native_first() }}</span>
        <p class="mt-0.5 text-xs text-ink-faint">
          {{ m.settings_native_first_desc() }}
        </p>
      </div>
    </label>

    <!-- 选择模式 -->
    <div>
      <span class="mb-2 block text-sm font-medium text-ink-soft">{{
        m.settings_selection_mode()
      }}</span>
      <div class="flex flex-col gap-2">
        <label
          v-for="o in SELECTION_OPTIONS"
          :key="o.value"
          class="flex cursor-pointer items-start gap-3 rounded-lg border border-ink/8 px-4 py-3 transition-colors duration-150"
          :class="
            policy.selection === o.value ? 'border-accent/40 bg-accent/8' : 'hover:bg-ink/[0.03]'
          "
        >
          <input
            v-model="policy.selection"
            type="radio"
            :value="o.value"
            name="selection"
            class="mt-0.5 accent-[var(--color-accent)]"
          />
          <div>
            <p class="text-sm font-medium">{{ o.label() }}</p>
            <p class="mt-0.5 text-xs text-ink-faint">{{ o.desc() }}</p>
          </div>
        </label>
      </div>
    </div>

    <!-- 最大重试 -->
    <label class="flex flex-col gap-1.5">
      <span class="text-sm font-medium text-ink-soft">
        {{ m.settings_max_attempts() }}
        <span class="ml-2 font-normal text-ink-faint">{{ m.settings_max_attempts_hint() }}</span>
      </span>
      <input
        v-model.number="policy.max_attempts"
        type="number"
        min="0"
        max="32"
        class="glass-field tabular w-32 px-3 py-2 text-sm outline-none"
      />
    </label>

    <!-- 单请求上游调用上限 -->
    <label class="flex flex-col gap-1.5">
      <span class="text-sm font-medium text-ink-soft">
        {{ m.settings_max_upstream_calls() }}
        <span class="ml-2 font-normal text-ink-faint">
          {{ m.settings_max_upstream_calls_hint() }}
        </span>
      </span>
      <input
        v-model.number="policy.max_upstream_calls"
        type="number"
        min="0"
        max="255"
        step="1"
        inputmode="numeric"
        :aria-label="m.settings_max_upstream_calls_aria()"
        class="glass-field tabular w-32 px-3 py-2 text-sm outline-none"
      />
    </label>
    <p v-if="!valid" class="text-xs text-danger" role="alert">
      {{ m.settings_routing_val_err() }}
    </p>

    <!-- 重试同一渠道 -->
    <label class="flex cursor-pointer items-center gap-3">
      <input
        v-model="policy.retry_same_channel"
        type="checkbox"
        class="accent-[var(--color-accent)]"
      />
      <span class="text-sm">
        {{ m.settings_retry_same_channel() }}
        <span class="text-xs text-ink-faint">{{ m.settings_retry_same_channel_hint() }}</span>
      </span>
    </label>

    <!-- HTTP 200 空回复重试 -->
    <div class="border-t border-ink/8 pt-4">
      <SettingsSectionError :message="emptyRetryError" @retry="emit('retryEmptyRetry')" />
      <span class="text-sm font-medium text-ink-soft">{{ m.settings_empty_retry_title() }}</span>
      <p class="mt-1 text-xs text-ink-faint">
        {{ m.settings_empty_retry_desc() }}
      </p>
      <div class="mt-3 grid max-w-md grid-cols-1 gap-4 sm:grid-cols-2">
        <label class="flex flex-col gap-1.5">
          <span class="text-xs font-medium text-ink-soft">{{
            m.settings_empty_retry_window()
          }}</span>
          <input
            v-model.number="emptyResponseRetry.window_secs"
            type="number"
            min="0"
            max="3600"
            step="1"
            inputmode="numeric"
            class="glass-field tabular px-3 py-2 text-sm outline-none"
          />
        </label>
        <label class="flex flex-col gap-1.5">
          <span class="text-xs font-medium text-ink-soft">{{ m.settings_empty_retry_max() }}</span>
          <input
            v-model.number="emptyResponseRetry.max_retries"
            type="number"
            min="0"
            max="100"
            step="1"
            inputmode="numeric"
            class="glass-field tabular px-3 py-2 text-sm outline-none"
          />
        </label>
      </div>
      <p v-if="!emptyRetryValid" class="mt-2 text-xs text-danger" role="alert">
        {{ m.settings_empty_retry_val_err() }}
      </p>

      <label class="mt-4 flex cursor-pointer items-center gap-3 border-t border-ink/8 pt-4">
        <GlassSwitch
          v-model="emptyResponseRetry.reject_nonstandard_200"
          :label="m.settings_reject_nonstandard_200()"
        />
        <div>
          <span class="text-sm font-medium">{{ m.settings_reject_nonstandard_200() }}</span>
          <p class="mt-0.5 text-xs text-ink-faint">
            {{ m.settings_reject_nonstandard_200_desc() }}
          </p>
        </div>
      </label>
    </div>
  </section>
</template>
