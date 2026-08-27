<script setup lang="ts">
import * as m from '@/paraglide/messages'
withDefaults(
  defineProps<{
    size?: 'sm' | 'md' | 'lg' | 'xl'
    label?: string
    tone?: 'accent' | 'faint' | 'white'
  }>(),
  {
    size: 'md',
    label: undefined,
    tone: 'accent',
  },
)

const SIZE_MAP = {
  sm: 'size-4 border-[2px]',
  md: 'size-6 border-[2.5px]',
  lg: 'size-8 border-[3px]',
  xl: 'size-10 border-[3.5px]',
}

const TONE_MAP = {
  accent: 'border-accent/20 border-t-accent text-accent',
  faint: 'border-ink/15 border-t-ink-soft text-ink-soft',
  white: 'border-white/25 border-t-white text-white',
}
</script>

<template>
  <div class="inline-flex flex-col items-center justify-center gap-3">
    <div
      class="animate-spin rounded-full transition-transform duration-300 ease-out"
      :class="[SIZE_MAP[size], TONE_MAP[tone]]"
      role="status"
      :aria-label="m.common_loading()"
    />
    <span v-if="label" class="text-xs font-medium tracking-wide text-ink-faint select-none">
      {{ label }}
    </span>
  </div>
</template>
