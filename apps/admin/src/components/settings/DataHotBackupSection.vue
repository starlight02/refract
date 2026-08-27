<script setup lang="ts">
/**
 * SQLite 在线热备：库体积 / 日志行数，以及 VACUUM INTO 下载。
 */
import AppIcon from '@/components/AppIcon.vue'
import * as m from '@/paraglide/messages'
import { useAction } from '@/composables/useAction'
import { data as dataApi } from '@/api/client'

defineProps<{
  dbStats: { db_bytes: number; log_rows: number; oldest_log_at: string | null } | null
}>()

const downloadDbBackupAction = useAction(m.settings_db_backup_failed(), { toast: true })

function fmtBytes(bytes: number): string {
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(1)} MB`
  return `${(bytes / 1024 / 1024 / 1024).toFixed(2)} GB`
}

async function downloadDatabaseBackup() {
  if (downloadDbBackupAction.busy) return
  await downloadDbBackupAction.run(
    () => dataApi.backup(),
    () => m.settings_db_backup_downloaded(),
  )
}
</script>

<template>
  <section class="glass glass-specular mt-5 flex flex-col gap-4 p-5">
    <div>
      <h2 class="text-sm font-semibold text-ink-soft uppercase">{{ m.settings_db_title() }}</h2>
      <p class="mt-1 text-xs text-ink-faint">
        {{ m.settings_db_desc() }}
      </p>
    </div>

    <div v-if="dbStats" class="flex flex-wrap gap-x-6 gap-y-1 text-sm text-ink-soft">
      <span>
        {{ m.settings_db_size() }}
        <span class="tabular font-medium">{{ fmtBytes(dbStats.db_bytes) }}</span>
      </span>
      <span>
        {{ m.settings_db_logs() }}
        <span class="tabular font-medium">{{ dbStats.log_rows.toLocaleString() }}</span>
        {{ m.settings_db_rows() }}
      </span>
      <span v-if="dbStats.oldest_log_at">
        {{ m.settings_db_oldest() }}
        <span class="tabular font-medium">{{ dbStats.oldest_log_at }}</span>
      </span>
    </div>

    <div>
      <button
        type="button"
        class="glass-button-ghost inline-flex items-center gap-1.5 px-3 py-2 text-sm"
        :disabled="downloadDbBackupAction.busy"
        @click="downloadDatabaseBackup"
      >
        <AppIcon
          :name="downloadDbBackupAction.busy ? 'spinner' : 'download'"
          :class="downloadDbBackupAction.busy ? 'animate-spin' : ''"
          :size="14"
        />
        {{
          downloadDbBackupAction.busy ? m.settings_db_generating() : m.settings_db_download_btn()
        }}
      </button>
    </div>
  </section>
</template>
