<script setup lang="ts">
/**
 * 配置 JSON 导入导出。
 */
import { ref } from 'vue'
import AppIcon from '@/components/AppIcon.vue'
import SettingsSectionError from '@/components/settings/SettingsSectionError.vue'
import * as m from '@/paraglide/messages'
import { useAction } from '@/composables/useAction'
import { backup } from '@/api/client'
import { parseJson } from '@/utils/effect'
import type { ImportResult } from '@refract/contracts'

defineProps<{
  loadError?: string | null
}>()

const emit = defineEmits<{
  retry: []
}>()

const exportBackupAction = useAction(m.settings_backup_export_failed(), { toast: true })
const importBackupAction = useAction(m.settings_backup_import_failed(), { toast: true })
const importMode = ref<'merge' | 'replace'>('merge')
const importFileInput = ref<HTMLInputElement | null>(null)
/** 待确认的替换导入：文件已解析但还没提交，等用户二次确认。 */
const pendingReplace = ref<{ name: string; payload: unknown } | null>(null)

/** 导出全量配置并触发浏览器下载。 */
async function exportBackup() {
  if (exportBackupAction.busy) return
  await exportBackupAction.run(
    () => backup.export(),
    (document_) => {
      const blob = new Blob([JSON.stringify(document_, null, 2)], { type: 'application/json' })
      const url = URL.createObjectURL(blob)
      const a = document.createElement('a')
      a.href = url
      a.download = `refract-backup-${new Date().toISOString().slice(0, 10)}.json`
      a.click()
      window.setTimeout(() => URL.revokeObjectURL(url), 10_000)
      return m.settings_backup_exported_msg()
    },
  )
}

/**
 * 读取选中的备份文件。合并模式直接导入；替换模式先清空再导入、
 * 不可恢复，所以解析后停下来等一次显式确认。
 */
async function importBackup(event: Event) {
  const input = event.target as HTMLInputElement
  const file = input.files?.[0]
  input.value = ''
  if (!file || importBackupAction.busy) return

  importBackupAction.clear()
  pendingReplace.value = null
  const parsed = parseJson(await file.text())
  if (parsed === undefined) {
    importBackupAction.notice = { tone: 'danger', text: m.settings_backup_invalid_json() }
    return
  }

  if (importMode.value === 'replace') {
    pendingReplace.value = { name: file.name, payload: parsed }
    return
  }
  await runImport(parsed)
}

/** 用户确认后执行替换导入。 */
async function confirmReplaceImport() {
  const pending = pendingReplace.value
  if (!pending) return
  pendingReplace.value = null
  await runImport(pending.payload)
}

/** 跳过名单太长会把提示挤成一堵墙：列前几个，其余折成计数。 */
function skippedDetail(kind: string, names: string[]): string {
  if (names.length === 0) return ''
  const shown = names.slice(0, 5).join('、')
  const rest = names.length > 5 ? ` 等 ${names.length} 个` : ''
  return `跳过的${kind}：${shown}${rest}。`
}

async function runImport(payload: unknown) {
  await importBackupAction.run(
    () => backup.import(payload, importMode.value),
    (result: ImportResult) => {
      const detail = [
        skippedDetail(m.common_channel(), result.skipped_channels ?? []),
        skippedDetail(m.common_key(), result.skipped_keys ?? []),
      ]
        .filter(Boolean)
        .join(' ')
      return (
        m.settings_backup_imported_msg({
          channels: String(result.channels_imported),
          keys: String(result.keys_imported),
        }) + (detail ? ` ${detail}` : '')
      )
    },
  )
}
</script>

<template>
  <section class="glass glass-specular mt-5 flex flex-col gap-4 p-5">
    <SettingsSectionError :message="loadError" @retry="emit('retry')" />
    <div>
      <h2 class="text-sm font-semibold text-ink-soft uppercase">{{ m.settings_backup_title() }}</h2>
      <p class="mt-1 text-xs text-ink-faint">
        {{ m.settings_backup_desc() }}
      </p>
    </div>

    <div class="flex flex-wrap items-center gap-3">
      <button
        type="button"
        class="glass-button-primary flex items-center gap-1.5 px-4 py-2 text-sm font-medium disabled:opacity-50"
        :disabled="exportBackupAction.busy"
        @click="exportBackup"
      >
        <AppIcon
          :name="exportBackupAction.busy ? 'spinner' : 'download'"
          :class="exportBackupAction.busy ? 'animate-spin' : ''"
          :size="15"
        />
        {{
          exportBackupAction.busy ? m.settings_backup_exporting() : m.settings_backup_export_btn()
        }}
      </button>

      <button
        type="button"
        class="glass-button-ghost px-4 py-2 text-sm font-medium"
        :disabled="importBackupAction.busy"
        @click="importFileInput?.click()"
      >
        <AppIcon
          :name="importBackupAction.busy ? 'spinner' : 'upload'"
          :class="importBackupAction.busy ? 'animate-spin' : ''"
          :size="15"
        />
        {{
          importBackupAction.busy ? m.settings_backup_importing() : m.settings_backup_import_btn()
        }}
      </button>
      <input
        ref="importFileInput"
        type="file"
        accept="application/json,.json"
        class="hidden"
        :aria-label="m.settings_backup_file_aria()"
        @change="importBackup"
      />

      <div class="flex items-center gap-2 text-xs text-ink-soft">
        <label class="flex cursor-pointer items-center gap-1.5">
          <input
            v-model="importMode"
            type="radio"
            value="merge"
            name="import-mode"
            class="accent-[var(--color-accent)]"
          />
          {{ m.settings_backup_mode_merge() }}
        </label>
        <label class="flex cursor-pointer items-center gap-1.5">
          <input
            v-model="importMode"
            type="radio"
            value="replace"
            name="import-mode"
            class="accent-[var(--color-accent)]"
          />
          {{ m.settings_backup_mode_replace() }}
        </label>
      </div>
    </div>

    <div
      v-if="pendingReplace"
      class="flex flex-wrap items-center gap-3 rounded-lg border border-danger/30 bg-danger/8 px-4 py-3"
      role="alertdialog"
      :aria-label="m.settings_backup_replace_confirm_title()"
    >
      <p class="text-xs text-ink-soft">
        {{ m.settings_backup_replace_warn_prefix()
        }}<span class="font-semibold text-danger">{{
          m.settings_backup_replace_warn_highlight()
        }}</span
        >{{ m.settings_backup_replace_warn_suffix({ name: pendingReplace.name }) }}
      </p>
      <div class="flex items-center gap-2">
        <button
          type="button"
          class="inline-flex items-center gap-1 rounded-lg bg-danger px-3.5 py-1.5 text-xs font-medium text-white hover:brightness-105 disabled:opacity-50"
          :disabled="importBackupAction.busy"
          @click="confirmReplaceImport"
        >
          <AppIcon v-if="importBackupAction.busy" name="spinner" class="animate-spin" :size="12" />
          {{
            importBackupAction.busy
              ? m.settings_backup_replacing()
              : m.settings_backup_replace_confirm_btn()
          }}
        </button>
        <button
          type="button"
          class="glass-button-ghost px-3 py-1.5 text-xs cursor-pointer"
          @click="pendingReplace = null"
        >
          {{ m.common_cancel() }}
        </button>
      </div>
    </div>
  </section>
</template>
