<script setup lang="ts">
/**
 * 渠道参数覆盖的结构化编辑器。
 *
 * 通用字段对所有协议生效；协议分组用分段按钮切换，不再靠键名猜作用域。
 */
import { computed, ref } from 'vue'
import AppIcon from '@/components/AppIcon.vue'
import * as m from '@/paraglide/messages'
import { PROTOCOL_LABEL, PROTOCOL_ORDER } from '@/components/protocol'
import {
  emptyOverrideRow,
  overrideDraftError,
  parseOverrideValue,
  type OverrideDraft,
  type OverrideRow,
} from '@/utils/param-override'
import type { Protocol } from '@refract/contracts'

const draft = defineModel<OverrideDraft>({ required: true })
const activeProtocol = ref<Protocol>('chat')

const draftError = computed(() => overrideDraftError(draft.value))

function protocolCount(protocol: Protocol): number {
  return draft.value.protocols[protocol].filter((row) => row.key.trim() || row.valueText.trim())
    .length
}

function addRow(rows: OverrideRow[]) {
  rows.push(emptyOverrideRow())
}

function removeRow(rows: OverrideRow[], index: number) {
  rows.splice(index, 1)
  if (rows.length === 0) rows.push(emptyOverrideRow())
}

function valueHint(row: OverrideRow): string | null {
  if (!row.valueText.trim()) return null
  const parsed = parseOverrideValue(row.valueText)
  if (!parsed.ok) return parsed.error
  if (parsed.value === null) return m.override_delete_value_hint()
  return typeof parsed.value
}
</script>

<template>
  <div class="flex flex-col gap-3">
    <div>
      <span class="text-xs font-medium text-ink-soft">{{ m.override_title() }}</span>
      <p class="mt-1 text-[0.7rem] text-ink-faint">
        {{ m.override_desc_p1() }}<code class="font-mono">{ "temperature": 0.7 }</code
        >{{ m.override_desc_p2() }}
      </p>
    </div>

    <section class="flex flex-col gap-2">
      <span class="text-[0.7rem] font-medium text-ink-soft">{{ m.override_common_section() }}</span>
      <div
        v-for="(row, index) in draft.common"
        :key="`common-${index}`"
        class="flex items-start gap-2"
      >
        <input
          v-model="row.key"
          type="text"
          :placeholder="m.override_key_placeholder()"
          spellcheck="false"
          class="glass-field w-36 shrink-0 px-2 py-1.5 font-mono text-xs outline-none"
        />
        <div class="min-w-0 flex-1">
          <input
            v-model="row.valueText"
            type="text"
            placeholder='0.7 / true / "text" / null / {"topK":40}'
            spellcheck="false"
            class="glass-field w-full px-2 py-1.5 font-mono text-xs outline-none"
          />
          <p v-if="valueHint(row)" class="mt-0.5 text-[0.65rem] text-ink-faint">
            {{ valueHint(row) }}
          </p>
        </div>
        <button
          type="button"
          class="rounded px-2 py-1.5 text-xs text-ink-faint hover:bg-danger/12 hover:text-danger cursor-pointer"
          :aria-label="m.override_delete_field_aria()"
          @click="removeRow(draft.common, index)"
        >
          <AppIcon name="trash" :size="13" />
        </button>
      </div>
      <button
        type="button"
        class="glass-button-ghost self-start px-2 py-1 text-xs"
        @click="addRow(draft.common)"
      >
        <AppIcon name="plus" :size="12" />
        {{ m.override_add_field() }}
      </button>
    </section>
    <section class="flex flex-col gap-2">
      <span class="text-[0.7rem] font-medium text-ink-soft">{{
        m.override_protocol_section()
      }}</span>
      <div class="flex flex-wrap gap-1">
        <button
          v-for="protocol in PROTOCOL_ORDER"
          :key="protocol"
          type="button"
          class="rounded-lg px-2 py-1 text-xs transition-colors"
          :class="
            activeProtocol === protocol
              ? 'bg-accent/15 text-accent'
              : 'text-ink-soft hover:bg-ink/5'
          "
          @click="activeProtocol = protocol"
        >
          {{ PROTOCOL_LABEL[protocol] }}
          <span v-if="protocolCount(protocol) > 0" class="ml-1 tabular text-ink-faint">
            {{ protocolCount(protocol) }}
          </span>
        </button>
      </div>
      <div
        v-for="(row, index) in draft.protocols[activeProtocol]"
        :key="`${activeProtocol}-${index}`"
        class="flex items-start gap-2"
      >
        <input
          v-model="row.key"
          type="text"
          :placeholder="m.override_key_placeholder()"
          spellcheck="false"
          class="glass-field w-36 shrink-0 px-2 py-1.5 font-mono text-xs outline-none"
        />
        <div class="min-w-0 flex-1">
          <input
            v-model="row.valueText"
            type="text"
            placeholder='0.7 / true / "text" / null / {"topK":40}'
            spellcheck="false"
            class="glass-field w-full px-2 py-1.5 font-mono text-xs outline-none"
          />
          <p v-if="valueHint(row)" class="mt-0.5 text-[0.65rem] text-ink-faint">
            {{ valueHint(row) }}
          </p>
        </div>
        <button
          type="button"
          class="rounded px-2 py-1.5 text-xs text-ink-faint hover:bg-danger/12 hover:text-danger cursor-pointer"
          :aria-label="m.override_delete_field_aria()"
          @click="removeRow(draft.protocols[activeProtocol], index)"
        >
          <AppIcon name="trash" :size="13" />
        </button>
      </div>
      <button
        type="button"
        class="glass-button-ghost self-start px-2 py-1 text-xs"
        @click="addRow(draft.protocols[activeProtocol])"
      >
        <AppIcon name="plus" :size="12" />
        {{ m.override_add_proto_field({ proto: PROTOCOL_LABEL[activeProtocol] }) }}
      </button>
    </section>
    <p v-if="draftError" class="text-xs text-danger" role="alert">{{ draftError }}</p>
  </div>
</template>
