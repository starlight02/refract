<script setup lang="ts">
/**
 * 全局玻璃开关。
 *
 * 交互语义交给 reka-ui：键盘切换、焦点、禁用状态、ARIA 与隐藏表单输入
 * 都由 SwitchRoot 处理；这里仅定义 Refract 的 macOS 玻璃外观。
 */
import { SwitchRoot, SwitchThumb } from 'reka-ui'

withDefaults(
  defineProps<{
    label: string
    disabled?: boolean
    tone?: 'accent' | 'success'
  }>(),
  { disabled: false, tone: 'accent' },
)

const model = defineModel<boolean>({ required: true })
</script>

<template>
  <SwitchRoot
    v-model="model"
    :disabled="disabled"
    :aria-label="label"
    class="glass-switch relative h-6 w-11 shrink-0 cursor-pointer border-0 transition-transform duration-150 ease-[--ease-glass] active:scale-[0.97] disabled:cursor-not-allowed disabled:opacity-45"
    :class="
      model ? (tone === 'success' ? 'glass-switch-on-success' : 'glass-switch-on-accent') : ''
    "
  >
    <SwitchThumb
      class="glass-switch-thumb block size-5 translate-x-0.5 rounded-full transition-transform duration-200 ease-[--ease-glass] will-change-transform data-[state=checked]:translate-x-[1.375rem]"
    />
  </SwitchRoot>
</template>
