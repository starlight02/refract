<script setup lang="ts">
/**
 * 配置 JSON 导入导出。
 */
import { ref } from 'vue'
import AppIcon from '@/components/AppIcon.vue'
import SettingsSectionError from '@/components/settings/SettingsSectionError.vue'
import { useAction } from '@/composables/useAction'
import { backup } from '@/api/client'
import { parseJson } from '@/utils/effect'

defineProps<{
  loadError?: string | null
}>()

const emit = defineEmits<{
  retry: []
}>()

const exportBackupAction = useAction('导出失败', { toast: true })
const importBackupAction = useAction('导入失败', { toast: true })
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
      return '备份已下载'
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
    importBackupAction.notice = { tone: 'danger', text: '文件不是有效的 JSON' }
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
    (result) => {
      const detail = [
        skippedDetail('渠道', result.skipped_channels ?? []),
        skippedDetail('密钥', result.skipped_keys ?? []),
      ]
        .filter(Boolean)
        .join(' ')
      return (
        `导入完成：渠道 +${result.channels_imported}（跳过 ${result.channels_skipped}），` +
        `密钥 +${result.keys_imported}（跳过 ${result.keys_skipped}）。` +
        (detail ? ` ${detail}` : '')
      )
    },
  )
}
</script>

<template>
  <section class="glass glass-specular mt-5 flex flex-col gap-4 p-5">
    <SettingsSectionError :message="loadError" @retry="emit('retry')" />
    <div>
      <h2 class="text-sm font-semibold text-ink-soft uppercase">数据备份</h2>
      <p class="mt-1 text-xs text-ink-faint">
        导出渠道、API 密钥与设置为一个 JSON 文件；可在另一个 Refract 实例导入恢复。
        导出文件含渠道凭据明文；网关密钥只含哈希，恢复后原密钥继续可用。
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
        {{ exportBackupAction.busy ? '导出中…' : '导出备份' }}
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
        {{ importBackupAction.busy ? '导入中…' : '导入备份' }}
      </button>
      <input
        ref="importFileInput"
        type="file"
        accept="application/json,.json"
        class="hidden"
        aria-label="选择备份文件"
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
          合并（跳过同名）
        </label>
        <label class="flex cursor-pointer items-center gap-1.5">
          <input
            v-model="importMode"
            type="radio"
            value="replace"
            name="import-mode"
            class="accent-[var(--color-accent)]"
          />
          替换（清空后导入）
        </label>
      </div>
    </div>

    <div
      v-if="pendingReplace"
      class="flex flex-wrap items-center gap-3 rounded-lg border border-danger/30 bg-danger/8 px-4 py-3"
      role="alertdialog"
      aria-label="确认替换导入"
    >
      <p class="text-xs text-ink-soft">
        替换导入会<span class="font-semibold text-danger">先清空现有全部渠道与密钥</span
        >，且无法恢复。确定用「{{ pendingReplace.name }}」替换吗？
      </p>
      <div class="flex items-center gap-2">
        <button
          type="button"
          class="inline-flex items-center gap-1 rounded-lg bg-danger px-3.5 py-1.5 text-xs font-medium text-white hover:brightness-105 disabled:opacity-50"
          :disabled="importBackupAction.busy"
          @click="confirmReplaceImport"
        >
          <AppIcon v-if="importBackupAction.busy" name="spinner" class="animate-spin" :size="12" />
          {{ importBackupAction.busy ? '替换中…' : '确认替换' }}
        </button>
        <button type="button" @click="pendingReplace = null">取消</button>
      </div>
    </div>
  </section>
</template>
