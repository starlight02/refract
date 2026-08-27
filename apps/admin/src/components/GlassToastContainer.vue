<script setup lang="ts">
import { useToastStore, type ToastTone } from '@/stores/toast'
import * as m from '@/paraglide/messages'
import AppIcon from '@/components/AppIcon.vue'

const toastStore = useToastStore()
function toneIcon(tone: ToastTone): 'check' | 'danger' | 'warning' | 'info' {
  switch (tone) {
    case 'success':
      return 'check'
    case 'danger':
      return 'danger'
    case 'warning':
      return 'warning'
    case 'info':
    default:
      return 'info'
  }
}

function toneColorClass(tone: ToastTone): string {
  switch (tone) {
    case 'success':
      return 'text-success'
    case 'danger':
      return 'text-danger'
    case 'warning':
      return 'text-warning'
    case 'info':
    default:
      return 'text-accent'
  }
}
</script>

<template>
  <div
    class="fixed top-5 right-5 z-50 pointer-events-none flex flex-col gap-2.5 max-w-sm w-[calc(100vw-2.5rem)] sm:w-88"
    aria-live="polite"
    aria-atomic="true"
  >
    <TransitionGroup
      enter-active-class="transition duration-200 ease-out"
      enter-from-class="opacity-0 translate-y-2 scale-95"
      enter-to-class="opacity-100 translate-y-0 scale-100"
      leave-active-class="transition duration-150 ease-in"
      leave-from-class="opacity-100 translate-y-0 scale-100"
      leave-to-class="opacity-0 -translate-y-2 scale-95"
    >
      <div
        v-for="item in toastStore.items"
        :key="item.id"
        class="glass glass-specular pointer-events-auto flex items-start gap-3 border-ink/10 p-3.5 shadow-lg"
        :role="item.tone === 'danger' ? 'alert' : 'status'"
      >
        <div class="mt-0.5 shrink-0" :class="toneColorClass(item.tone)">
          <AppIcon :name="toneIcon(item.tone)" :size="16" />
        </div>

        <div class="min-w-0 flex-1">
          <p class="text-sm font-medium text-ink leading-snug break-words">
            {{ item.text }}
          </p>
          <p v-if="item.description" class="mt-1 text-xs text-ink-soft leading-relaxed break-words">
            {{ item.description }}
          </p>
        </div>

        <button
          type="button"
          class="-mr-1 -mt-1 shrink-0 rounded-lg p-1 text-ink-faint hover:bg-ink/5 hover:text-ink transition-colors cursor-pointer"
          :aria-label="m.common_close()"
          @click="toastStore.dismiss(item.id)"
        >
          <AppIcon name="x" :size="14" />
        </button>
      </div>
    </TransitionGroup>
  </div>
</template>
