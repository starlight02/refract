<script setup lang="ts">
/**
 * 协议徽章。四种协议各有固定色，让用户在列表里靠颜色扫读。
 */
import { computed } from 'vue'
import type { Protocol } from '@refract/contracts'

const props = defineProps<{
  protocol: Protocol
  /** 紧凑模式：只显示缩写，用于聚合渠道的多徽章并排。 */
  compact?: boolean
}>()

const LABELS: Record<Protocol, { full: string; short: string; color: string }> = {
  chat: { full: 'Chat', short: 'C', color: 'var(--color-proto-chat)' },
  responses: { full: 'Responses', short: 'R', color: 'var(--color-proto-responses)' },
  messages: { full: 'Messages', short: 'M', color: 'var(--color-proto-messages)' },
  gemini: { full: 'Gemini', short: 'G', color: 'var(--color-proto-gemini)' },
}

const meta = computed(() => LABELS[props.protocol])
</script>

<template>
  <span class="proto-badge" :style="{ color: meta.color }">
    {{ compact ? meta.short : meta.full }}
  </span>
</template>
