<script setup lang="ts">
/**
 * 单个协议端点：协议、地址/密钥覆盖、模型芯片、协议转换。
 *
 * 模型芯片原地编辑必须用函数 ref：模板 ref 写在 v-for 里会变成数组，
 * 点开后无法 focus/select，e2e 的 toBeFocused 会挂。
 */
import { nextTick, ref } from 'vue'
import AppIcon from '@/components/AppIcon.vue'
import ProtocolBadge from '@/components/ProtocolBadge.vue'
import * as m from '@/paraglide/messages'
import UpstreamAddressFields from '@/components/channel-editor/UpstreamAddressFields.vue'
import {
  PROTOCOL_LABEL,
  PROTOCOL_ORDER,
  toggleProtocol,
  withoutProtocol,
} from '@/components/protocol'
import {
  emptyAddress,
  hasOwnAddress,
  looksMasked,
  parseAndAddModels,
  previewUrl,
} from '@/utils/channel-form'
import type { ChannelEndpoint, ModelEntry, Protocol, UpstreamAddress } from '@refract/contracts'

defineProps<{
  isAggregate: boolean
  takenProtocols: Protocol[]
  channelAddress: UpstreamAddress
  canRemove: boolean
}>()

const emit = defineEmits<{
  remove: []
  probe: []
}>()

const ep = defineModel<ChannelEndpoint>({ required: true })

const showEndpointCredential = ref(false)
const modelDraft = ref('')
const editingModelKey = ref<string | null>(null)
const editingUpstreamDraft = ref('')
const editMappingInput = ref<HTMLInputElement | null>(null)

function setEditMappingInput(el: unknown) {
  editMappingInput.value = el instanceof HTMLInputElement ? el : null
}

function onEndpointProtocolChange() {
  ep.value.transcode.accepted = withoutProtocol(ep.value.transcode.accepted, ep.value.protocol)
  if (looksMasked(ep.value.credential)) ep.value.credential = null
}

function toggleOwnAddress(on: boolean) {
  ep.value.address = on ? { ...emptyAddress(), unofficial: true } : emptyAddress()
}

function addModel() {
  const raw = modelDraft.value.trim()
  modelDraft.value = ''
  parseAndAddModels(ep.value, raw)
}

function onModelPaste(e: ClipboardEvent) {
  const pasted = e.clipboardData?.getData('text') ?? ''
  if (/[\n,，;； \t]/.test(pasted)) {
    e.preventDefault()
    modelDraft.value = ''
    parseAndAddModels(ep.value, pasted)
  }
}

function removeModel(modelIndex: number) {
  ep.value.models.splice(modelIndex, 1)
}

function modelKey(protocol: Protocol, name: string): string {
  return `${protocol}:${name}`
}

function startEditMapping(m: ModelEntry) {
  editingModelKey.value = modelKey(ep.value.protocol, m.name)
  editingUpstreamDraft.value = m.upstream ?? ''
  void nextTick(() => {
    editMappingInput.value?.focus()
    editMappingInput.value?.select()
  })
}

function cancelEditMapping() {
  editingModelKey.value = null
  editingUpstreamDraft.value = ''
}

function commitEditMapping(m: ModelEntry) {
  if (editingModelKey.value !== modelKey(ep.value.protocol, m.name)) return
  const draft = editingUpstreamDraft.value.trim()
  m.upstream = draft === '' || draft === m.name ? null : draft
  cancelEditMapping()
}

function clearAllModels() {
  ep.value.models = []
}

function toggleAccepted(p: Protocol) {
  if (p === ep.value.protocol) return
  ep.value.transcode.accepted = toggleProtocol(ep.value.transcode.accepted, p)
}

function isAccepted(p: Protocol): boolean {
  return ep.value.transcode.accepted.includes(p)
}
</script>

<template>
  <article class="rounded-xl border border-ink/10 bg-black/5 p-4 dark:bg-white/5">
    <div class="mb-3 flex flex-wrap items-center gap-3">
      <select
        v-model="ep.protocol"
        :disabled="!isAggregate"
        class="glass-field px-3 py-1.5 text-sm outline-none disabled:opacity-60"
        @change="onEndpointProtocolChange"
      >
        <option
          v-for="p in PROTOCOL_ORDER"
          :key="p"
          :value="p"
          :disabled="p !== ep.protocol && takenProtocols.includes(p)"
        >
          {{ p }}
        </option>
      </select>

      <label v-if="isAggregate" class="flex items-center gap-1.5 text-xs text-ink-soft">
        order
        <input
          v-model.number="ep.order"
          type="number"
          min="0"
          class="glass-field tabular w-16 px-2 py-1 text-sm outline-none"
        />
      </label>

      <label class="flex cursor-pointer items-center gap-2 text-xs text-ink-soft">
        <input v-model="ep.enabled" type="checkbox" class="accent-[var(--color-accent)]" />
        {{ m.common_enable() }}
      </label>

      <span class="w-full break-all font-mono text-[0.7rem] text-ink-faint sm:ml-auto sm:w-auto">
        {{ previewUrl(ep, channelAddress) }}
      </span>

      <button
        v-if="canRemove"
        type="button"
        class="rounded px-2 py-1 text-xs text-ink-faint hover:bg-danger/12 hover:text-danger cursor-pointer"
        @click="emit('remove')"
      >
        {{ m.common_remove() }}
      </button>
    </div>

    <!-- 端点地址覆盖 -->
    <div class="mb-3">
      <label class="flex cursor-pointer items-center gap-2 text-xs text-ink-soft">
        <input
          type="checkbox"
          :checked="hasOwnAddress(ep)"
          class="accent-[var(--color-accent)]"
          @change="toggleOwnAddress(($event.target as HTMLInputElement).checked)"
        />
        {{ m.ep_custom_addr() }}<span class="text-ink-faint">{{ m.ep_custom_addr_hint() }}</span>
      </label>

      <div v-if="hasOwnAddress(ep)" class="mt-2 flex flex-col gap-2 pl-5">
        <UpstreamAddressFields v-model="ep.address" variant="endpoint" :protocol="ep.protocol" />
      </div>
    </div>

    <!-- 端点凭据覆盖 -->
    <div class="mb-3">
      <label class="flex cursor-pointer items-center gap-2 text-xs text-ink-soft">
        <input
          type="checkbox"
          :checked="ep.credential !== null"
          class="accent-[var(--color-accent)]"
          @change="ep.credential = ($event.target as HTMLInputElement).checked ? '' : null"
        />
        {{ m.ep_custom_key() }}<span class="text-ink-faint">{{ m.ep_custom_key_hint() }}</span>
      </label>

      <div v-if="ep.credential !== null" class="relative mt-2 pl-5">
        <input
          v-model="ep.credential"
          :type="showEndpointCredential ? 'text' : 'password'"
          placeholder="sk-..."
          autocomplete="new-password"
          :aria-label="m.ep_key_aria({ protocol: ep.protocol })"
          class="glass-field w-full px-3 py-1.5 pr-16 font-mono text-xs outline-none"
        />
        <button
          type="button"
          class="absolute top-1/2 right-2 -translate-y-1/2 rounded px-2 py-0.5 text-[0.7rem] text-ink-faint hover:text-ink"
          :aria-label="
            showEndpointCredential
              ? m.ep_key_hide_aria({ protocol: ep.protocol })
              : m.ep_key_show_aria({ protocol: ep.protocol })
          "
          :aria-pressed="showEndpointCredential === true"
          @click="showEndpointCredential = !showEndpointCredential"
        >
          {{ showEndpointCredential ? m.common_hide() : m.common_show() }}
        </button>
      </div>
    </div>

    <!-- 模型列表与选择 -->
    <div class="mb-4">
      <div class="mb-2 flex flex-wrap items-center justify-between gap-2">
        <div class="flex items-center gap-2">
          <span class="text-xs font-semibold text-ink-soft">{{ m.ep_models_title() }}</span>
          <span class="rounded bg-ink/8 px-1.5 py-0.5 text-[0.7rem] font-medium text-ink-faint">
            {{ m.ep_models_count({ count: ep.models.length }) }}
          </span>
        </div>
        <div class="flex flex-wrap items-center gap-1.5">
          <button
            type="button"
            class="glass-button-ghost flex items-center gap-1 px-2.5 py-1 text-xs font-medium text-accent hover:!bg-accent/15"
            @click="emit('probe')"
          >
            <AppIcon name="globe" :size="13" />
            {{ m.ep_fetch_models() }}
          </button>

          <button
            v-if="ep.models.length > 0"
            type="button"
            class="glass-button-ghost px-2 py-1 text-xs text-ink-faint hover:!text-danger"
            @click="clearAllModels"
          >
            {{ m.ep_clear_models() }}
          </button>
        </div>
      </div>
      <div v-if="ep.models.length > 0" class="mb-2.5 flex flex-wrap gap-1.5">
        <span
          v-for="(modelEntry, mi) in ep.models"
          :key="modelEntry.name"
          class="inline-flex items-center gap-1.5 rounded-full border border-ink/8 bg-ink/6 px-2 py-1 font-mono text-xs shadow-xs"
        >
          <template v-if="editingModelKey === modelKey(ep.protocol, modelEntry.name)">
            <span class="font-medium text-ink">{{ modelEntry.name }}</span>
            <span class="text-[0.7rem] text-ink-faint">→</span>
            <input
              :ref="setEditMappingInput"
              :value="editingUpstreamDraft"
              type="text"
              :aria-label="m.ep_model_upstream_aria({ name: modelEntry.name })"
              :placeholder="modelEntry.name"
              class="glass-field h-auto w-40 px-1.5 py-0.5 font-mono text-[0.7rem] outline-none"
              @input="editingUpstreamDraft = ($event.target as HTMLInputElement).value"
              @keydown.enter.prevent="commitEditMapping(modelEntry)"
              @keydown.esc.prevent="cancelEditMapping"
              @blur="commitEditMapping(modelEntry)"
            />
          </template>
          <button
            v-else
            type="button"
            class="cursor-pointer rounded font-medium text-ink hover:text-accent-deep"
            :title="m.ep_model_edit_title({ name: modelEntry.name })"
            @click="startEditMapping(modelEntry)"
          >
            {{ modelEntry.name
            }}<span v-if="modelEntry.upstream" class="ml-1 text-[0.7rem] text-accent-deep"
              >→{{ modelEntry.upstream }}</span
            >
          </button>
          <button
            type="button"
            class="grid size-3.5 place-items-center rounded-full text-ink-faint hover:bg-danger/20 hover:text-danger"
            :title="m.ep_model_remove_title()"
            @click="removeModel(mi)"
          >
            ×
          </button>
        </span>
      </div>

      <div class="relative">
        <input
          v-model="modelDraft"
          type="text"
          :aria-label="m.ep_model_input_aria({ protocol: PROTOCOL_LABEL[ep.protocol] })"
          :placeholder="m.ep_model_input_placeholder()"
          class="glass-field w-full px-3 py-1.5 pr-16 font-mono text-xs outline-none"
          @keydown.enter.prevent="addModel"
          @paste="onModelPaste"
        />
        <button
          v-if="modelDraft.trim()"
          type="button"
          class="absolute top-1/2 right-1.5 -translate-y-1/2 rounded bg-accent px-2 py-0.5 text-xs text-white"
          @click="addModel"
        >
          {{ m.common_add() }}
        </button>
      </div>
    </div>
    <!-- 协议转换 -->
    <div>
      <label class="flex cursor-pointer items-center gap-2 text-xs text-ink-soft">
        <input
          v-model="ep.transcode.enabled"
          type="checkbox"
          class="accent-[var(--color-accent)]"
        />
        <span class="font-medium">{{ m.ep_transcode_title() }}</span>
        <span class="text-ink-faint">{{ m.ep_transcode_desc() }}</span>
      </label>

      <div v-if="ep.transcode.enabled" class="mt-2 flex flex-wrap gap-2 pl-5">
        <label
          v-for="p in PROTOCOL_ORDER"
          :key="p"
          class="flex items-center gap-1.5 text-xs"
          :class="p === ep.protocol ? 'cursor-not-allowed opacity-40' : 'cursor-pointer'"
        >
          <input
            type="checkbox"
            :checked="isAccepted(p)"
            :disabled="p === ep.protocol"
            class="accent-[var(--color-accent)]"
            @change="toggleAccepted(p)"
          />
          <ProtocolBadge :protocol="p" />
          <span v-if="p === ep.protocol" class="text-ink-faint">{{ m.ep_transcode_native() }}</span>
        </label>
      </div>
      <p v-if="ep.transcode.enabled" class="mt-1.5 pl-5 text-[0.7rem] text-ink-faint">
        {{ m.ep_transcode_hint() }}
      </p>
    </div>
  </article>
</template>
