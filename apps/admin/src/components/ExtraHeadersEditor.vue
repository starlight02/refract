<script setup lang="ts">
/**
 * 渠道自定义请求头的行编辑器。
 *
 * 一行一对 Name / Value，校验规则与后端保存时一致。
 */
import AppIcon from '@/components/AppIcon.vue'
import * as m from '@/paraglide/messages'
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
      <span class="text-xs font-medium text-ink-soft">{{ m.headers_title() }}</span>
      <p class="mt-1 text-[0.7rem] text-ink-faint">
        {{ m.headers_desc() }}
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
          :placeholder="m.headers_value_placeholder()"
          spellcheck="false"
          class="glass-field min-w-0 flex-1 px-2 py-1.5 font-mono text-xs outline-none"
        />
        <button
          type="button"
          class="rounded px-2 py-1.5 text-xs text-ink-faint hover:bg-danger/12 hover:text-danger cursor-pointer"
          :aria-label="m.headers_delete_aria()"
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
      {{ m.headers_add() }}
    </button>
  </div>
</template>
