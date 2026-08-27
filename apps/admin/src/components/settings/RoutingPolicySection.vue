<script setup lang="ts">
/**
 * 路由策略：原生优先、选择模式、重试，以及上游 200 空回复重试。
 */
import GlassSwitch from '@/components/GlassSwitch.vue'
import SettingsSectionError from '@/components/settings/SettingsSectionError.vue'
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

const SELECTION_OPTIONS: { value: SelectionMode; label: string; desc: string }[] = [
  {
    value: 'weighted_random',
    label: '加权随机（推荐）',
    desc: '同优先级内按权重随机选取。适合多渠道流量分配。',
  },
  { value: 'round_robin', label: '轮询', desc: '同优先级内按顺序轮转。适合等量消耗多家余额。' },
  { value: 'first', label: '固定首选', desc: '总是命中同优先级内第一个可用渠道。简单但单点。' },
]
</script>

<template>
  <section class="glass glass-specular flex flex-col gap-5 p-5">
    <SettingsSectionError :message="loadError" @retry="emit('retry')" />
    <h2 class="text-sm font-semibold text-ink-soft uppercase">路由策略</h2>

    <!-- 原生优先（需求 6） -->
    <label class="flex cursor-pointer items-center gap-3">
      <GlassSwitch v-model="policy.native_first" label="原生优先" />
      <div>
        <span class="text-sm font-medium">原生优先</span>
        <p class="mt-0.5 text-xs text-ink-faint">
          关闭时路由逻辑与 new-api 一致。打开时命中同一模型的原生协议端点始终排在转换端点之前。
        </p>
      </div>
    </label>

    <!-- 选择模式 -->
    <div>
      <span class="mb-2 block text-sm font-medium text-ink-soft">选择模式</span>
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
            <p class="text-sm font-medium">{{ o.label }}</p>
            <p class="mt-0.5 text-xs text-ink-faint">{{ o.desc }}</p>
          </div>
        </label>
      </div>
    </div>

    <!-- 最大重试 -->
    <label class="flex flex-col gap-1.5">
      <span class="text-sm font-medium text-ink-soft">
        最大重试次数
        <span class="ml-2 font-normal text-ink-faint"> 0 = 不限。建议 2–3。过大会拉长超时。 </span>
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
        单请求上游调用上限
        <span class="ml-2 font-normal text-ink-faint">
          含重试在内的上游调用总次数，0 = 不限，默认 8。
        </span>
      </span>
      <input
        v-model.number="policy.max_upstream_calls"
        type="number"
        min="0"
        max="255"
        step="1"
        inputmode="numeric"
        aria-label="单请求上游调用次数上限"
        class="glass-field tabular w-32 px-3 py-2 text-sm outline-none"
      />
    </label>
    <p v-if="!valid" class="text-xs text-danger" role="alert">
      最大重试 0–32（0 = 不限）；上游调用上限 0–255。
    </p>

    <!-- 重试同一渠道 -->
    <label class="flex cursor-pointer items-center gap-3">
      <input
        v-model="policy.retry_same_channel"
        type="checkbox"
        class="accent-[var(--color-accent)]"
      />
      <span class="text-sm">
        重试时允许再次命中同一渠道
        <span class="text-xs text-ink-faint"
          >—— 建议关闭，否则 500 可能只是上游临时故障，原渠道未必恢复</span
        >
      </span>
    </label>

    <!-- HTTP 200 空回复重试 -->
    <div class="border-t border-ink/8 pt-4">
      <SettingsSectionError :message="emptyRetryError" @retry="emit('retryEmptyRetry')" />
      <span class="text-sm font-medium text-ink-soft">上游 200 空回复重试</span>
      <p class="mt-1 text-xs text-ink-faint">
        上游返回 HTTP 200 但没有文本、推理、拒答或工具调用，且“完成时刻 −
        首字节时刻”不超过判定窗口时，在同一渠道重试。任一值为 0 即关闭。
      </p>
      <div class="mt-3 grid max-w-md grid-cols-1 gap-4 sm:grid-cols-2">
        <label class="flex flex-col gap-1.5">
          <span class="text-xs font-medium text-ink-soft">判定窗口（秒）</span>
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
          <span class="text-xs font-medium text-ink-soft">最大重试次数</span>
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
        判定窗口需为 0–3600 秒，最大重试需为 0–100 次。
      </p>

      <label class="mt-4 flex cursor-pointer items-center gap-3 border-t border-ink/8 pt-4">
        <GlassSwitch
          v-model="emptyResponseRetry.reject_nonstandard_200"
          label="非标准 200 转为 500"
        />
        <div>
          <span class="text-sm font-medium">非标准 200 转为 500</span>
          <p class="mt-0.5 text-xs text-ink-faint">
            开启后，纯文本、HTML 或无法识别的 JSON/SSE 等不符合渠道协议的 HTTP 200
            响应会转换为不可重试的 500，并返回明确错误提示。
          </p>
        </div>
      </label>
    </div>
  </section>
</template>
