<script setup lang="ts">
/**
 * 请求日志保留天数，以及是否记录请求/响应正文。
 */
import GlassSwitch from '@/components/GlassSwitch.vue'
import SettingsSectionError from '@/components/settings/SettingsSectionError.vue'
import * as m from '@/paraglide/messages'
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
      <h2 class="text-sm font-semibold text-ink-soft uppercase">
        {{ m.settings_retention_title() }}
      </h2>
      <p class="mt-1 text-xs text-ink-faint">
        {{ m.settings_retention_desc() }}
      </p>
    </div>

    <label class="flex cursor-pointer items-center gap-3">
      <GlassSwitch v-model="logBodies" :label="m.settings_retention_bodies()" />
      <span>
        <span class="text-sm font-medium">{{ m.settings_retention_bodies() }}</span>
        <span class="ml-2 text-xs text-ink-faint">
          {{ m.settings_retention_bodies_desc() }}
        </span>
      </span>
    </label>

    <label class="flex max-w-sm flex-col gap-1.5">
      <span class="text-sm font-medium text-ink-soft">{{ m.settings_retention_days() }}</span>
      <div class="flex items-center gap-2">
        <input
          v-model.number="retentionDays"
          type="number"
          min="1"
          max="3650"
          step="1"
          inputmode="numeric"
          :aria-label="m.settings_retention_days()"
          :aria-invalid="!valid"
        />
        <span class="text-sm text-ink-faint">{{ m.settings_retention_unit() }}</span>
      </div>
      <span v-if="!valid" class="text-xs text-danger" role="alert">
        {{ m.settings_retention_val_err() }}
      </span>
    </label>
  </section>
</template>
