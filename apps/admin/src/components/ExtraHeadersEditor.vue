<script setup lang="ts">
/**
 * 渠道自定义请求头的行编辑器。
 *
 * 一行一对 Name / Value，校验规则与后端保存时一致。
 */
import AppIcon from '@/components/AppIcon.vue'
import { emptyHeaderRow, headerRowError, type HeaderRow } from '@/utils/extra-headers'

const rows = defineModel<HeaderRow[]>({ required: true })

function addRow() {
  rows.value.push(emptyHeaderRow())
}

function removeRow(index: number) {
  rows.value.splice(index, 1)
  if (rows.value.length === 0) rows.value.push(emptyHeaderRow())
}
</script>

<template>
  <div class="flex flex-col gap-2">
    <div>
      <span class="text-xs font-medium text-ink-soft">自定义请求头</span>
      <p class="mt-1 text-[0.7rem] text-ink-faint">
        随所有上游调用发送。鉴权头（Authorization / x-api-key）由网关掌管，不能覆盖。
      </p>
    </div>
    <div v-for="(row, index) in rows" :key="index" class="flex flex-col gap-1">
      <div class="flex items-start gap-2">
        <input
          v-model="row.name"
          type="text"
          placeholder="X-Site-Token"
          spellcheck="false"
          class="glass-field w-40 shrink-0 px-2 py-1.5 font-mono text-xs outline-none"
        />
        <input
          v-model="row.value"
          type="text"
          placeholder="值"
          spellcheck="false"
          class="glass-field min-w-0 flex-1 px-2 py-1.5 font-mono text-xs outline-none"
        />
        <button
          type="button"
          class="rounded px-2 py-1.5 text-xs text-ink-faint hover:bg-danger/12 hover:text-danger"
          aria-label="删除请求头"
          @click="removeRow(index)"
        >
          <AppIcon name="trash" :size="13" />
        </button>
      </div>
      <p v-if="headerRowError(row)" class="text-xs text-danger" role="alert">
        {{ headerRowError(row) }}
      </p>
    </div>
    <button type="button" class="glass-button-ghost self-start px-2 py-1 text-xs" @click="addRow">
      <AppIcon name="plus" :size="12" />
      添加请求头
    </button>
  </div>
</template>
