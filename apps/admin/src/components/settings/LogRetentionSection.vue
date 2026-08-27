<script setup lang="ts">
/**
 * 请求日志保留天数，以及是否记录请求/响应正文。
 */
import GlassSwitch from '@/components/GlassSwitch.vue'
import SettingsSectionError from '@/components/settings/SettingsSectionError.vue'

const retentionDays = defineModel<number>({ required: true })
const logBodies = defineModel<boolean>('logBodies', { required: true })

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
    <div>
      <SettingsSectionError :message="loadError" @retry="emit('retry')" />
      <h2 class="text-sm font-semibold text-ink-soft uppercase">日志保留</h2>
      <p class="mt-1 text-xs text-ink-faint">
        服务启动时清理一次，之后每 24 小时按当前设置删除过期请求日志。
      </p>
    </div>

    <label class="flex cursor-pointer items-center gap-3">
      <GlassSwitch v-model="logBodies" label="记录请求与响应正文" />
      <span>
        <span class="text-sm font-medium">记录请求与响应正文</span>
        <span class="ml-2 text-xs text-ink-faint">
          排障时可在日志里查看完整请求；正文超过 64KB 截断，流式存聚合文本。
        </span>
      </span>
    </label>

    <label class="flex max-w-sm flex-col gap-1.5">
      <span class="text-sm font-medium text-ink-soft">保留天数</span>
      <div class="flex items-center gap-2">
        <input
          v-model.number="retentionDays"
          type="number"
          min="1"
          max="3650"
          step="1"
          inputmode="numeric"
          aria-label="保留天数"
          :aria-invalid="!valid"
        />
        <span class="text-sm text-ink-faint">天</span>
      </div>
      <span v-if="!valid" class="text-xs text-danger" role="alert"> 请输入 1–3650 的整数。 </span>
    </label>
  </section>
</template>
