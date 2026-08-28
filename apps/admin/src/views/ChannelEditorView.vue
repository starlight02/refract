<script setup lang="ts">
/**
 * 渠道编辑器编排：路由、加载/保存/删除、脏检查、类型同步、探测开窗。
 *
 * 表单结构跟着领域模型走。单协议与聚合都是「渠道 + 端点数组」，
 * 单协议时锁死数量为 1 并同步协议。
 */
import { computed, onBeforeUnmount, onMounted, ref, toRef, watch } from 'vue'
import { onBeforeRouteLeave, useRoute, useRouter } from 'vue-router'
import AppIcon from '@/components/AppIcon.vue'
import GlassSpinner from '@/components/GlassSpinner.vue'
import ChannelAdvancedSection from '@/components/channel-editor/ChannelAdvancedSection.vue'
import ChannelBasicsSection from '@/components/channel-editor/ChannelBasicsSection.vue'
import EndpointCard from '@/components/channel-editor/EndpointCard.vue'
import KeyPoolSection from '@/components/channel-editor/KeyPoolSection.vue'
import ProbeModelsDialog, {
  type ProbeDialogState,
} from '@/components/channel-editor/ProbeModelsDialog.vue'
import UpstreamAddressFields from '@/components/channel-editor/UpstreamAddressFields.vue'
import { useAction } from '@/composables/useAction'
import * as m from '@/paraglide/messages'
import { type ApiScope, scopedChannels } from '@/api/client'
import { useChannelsStore } from '@/stores/channels'
import { numOr, numOrNull } from '@/utils/num'
import { withoutProtocol, PROTOCOL_ORDER } from '@/components/protocol'
import {
  emptyHeaderRow,
  headerRowsError,
  headersFromRows,
  rowsFromHeaders,
} from '@/utils/extra-headers'
import {
  draftFromOverride,
  emptyOverrideDraft,
  overrideDraftError,
  overrideFromDraft,
} from '@/utils/param-override'
import { blankChannel, looksMasked, newEndpoint, poolCredentials } from '@/utils/channel-form'
import { validateChannel } from '@/utils/channel-validation'
import type { Channel, ChannelEndpoint, ModelEntry } from '@refract/contracts'

const props = withDefaults(defineProps<{ scope?: ApiScope }>(), { scope: 'admin' })
const route = useRoute()
const router = useRouter()
const store = useChannelsStore(props.scope)
const channelsApi = scopedChannels(props.scope)

function listPath(): string {
  return props.scope === 'admin' ? '/admin/channels' : '/channels'
}
const pristineSnapshot = ref('')
const isSubmitting = ref(false)

function getSnapshot(): string {
  return JSON.stringify({
    form: form.value,
    tagsText: tagsText.value,
    credentialsText: credentialsText.value,
    overrideDraft: overrideDraft.value,
    headerRows: headerRows.value,
  })
}

const isDirty = computed(
  () =>
    !isSubmitting.value &&
    pristineSnapshot.value !== '' &&
    getSnapshot() !== pristineSnapshot.value,
)

function onBeforeUnload(e: BeforeUnloadEvent) {
  if (isDirty.value && !isSubmitting.value) {
    e.preventDefault()
    e.returnValue = ''
  }
}

onBeforeRouteLeave(() => {
  if (isDirty.value && !isSubmitting.value) {
    const confirm = window.confirm(m.ch_edit_leave_confirm())
    if (!confirm) return false
  }
})

const editingId = computed(() => {
  const raw = route.params.id
  return typeof raw === 'string' ? Number(raw) : null
})
const isEdit = computed(() => editingId.value !== null && !Number.isNaN(editingId.value))

const form = ref<Channel>(blankChannel())
const loadChannel = useAction(m.ch_edit_load_failed())
const saveChannel = useAction(m.ch_edit_save_failed(), { toast: true })
const destroyChannel = useAction(m.ch_edit_delete_failed(), { toast: true })
const probeModels = useAction(m.ch_edit_probe_failed_generic())
const loading = toRef(loadChannel, 'busy')
const saving = toRef(saveChannel, 'busy')
const destroying = toRef(destroyChannel, 'busy')
const saveError = computed(() => loadChannel.error ?? saveChannel.error ?? destroyChannel.error)
const tagsText = ref('')
const credentialsText = ref('')
const pendingDelete = ref(false)

const showAdvanced = ref(false)
const overrideDraft = ref(emptyOverrideDraft())
const headerRows = ref([emptyHeaderRow()])

const paramOverrideError = computed(() => overrideDraftError(overrideDraft.value))
const headersError = computed(() => headerRowsError(headerRows.value))

onMounted(async () => {
  window.addEventListener('beforeunload', onBeforeUnload)
  if (!isEdit.value) {
    if (props.scope === 'me') form.value.visibility = 'private'
    pristineSnapshot.value = getSnapshot()
    return
  }
  await loadChannel.run(async () => {
    const ch = await channelsApi.get(editingId.value as number)
    ch.empty_response_retry ??= { window_secs: null, max_retries: null }
    ch.visibility ??= 'shared'
    form.value = ch
    tagsText.value = (ch.tags ?? []).join(', ')
    overrideDraft.value = draftFromOverride(ch.param_override)
    headerRows.value = rowsFromHeaders(ch.extra_headers)
    const allCreds = [ch.credential, ...(ch.credentials ?? [])].filter((c) => c && c.trim() !== '')
    credentialsText.value = allCreds.join('\n')
    form.value.credential = ''
    if (
      ch.param_override ||
      (ch.extra_headers ?? []).length > 0 ||
      ch.proxy ||
      ch.test_model ||
      ch.empty_response_retry.window_secs !== null ||
      ch.empty_response_retry.max_retries !== null
    ) {
      showAdvanced.value = true
    }
  })
})

onBeforeUnmount(() => {
  window.removeEventListener('beforeunload', onBeforeUnload)
})

/**
 * 渠道类型变化时同步端点结构。
 *
 * 单协议渠道必须恰好一个端点且协议匹配（后端硬约束）。切到聚合时保留
 * 已有端点作为第一个，避免用户填了一半的配置被清空。
 */
watch(
  () => form.value.kind,
  (kind, previous) => {
    if (kind === previous) return
    if (kind === 'aggregate') return
    const first = form.value.endpoints[0] ?? newEndpoint(kind)
    const protocolChanged = first.protocol !== kind
    first.protocol = kind
    first.transcode.accepted = withoutProtocol(first.transcode.accepted, kind)
    if (protocolChanged && looksMasked(first.credential)) first.credential = null
    form.value.endpoints = [first]
  },
)

const isAggregate = computed(() => form.value.kind === 'aggregate')

const availableProtocols = computed(() =>
  PROTOCOL_ORDER.filter((p) => !form.value.endpoints.some((e) => e.protocol === p)),
)

function addEndpoint() {
  const next = availableProtocols.value[0]
  if (!next) return
  const order = form.value.endpoints.reduce((max, e) => Math.max(max, e.order), -1) + 1
  form.value.endpoints.push(newEndpoint(next, order))
}

function removeEndpoint(index: number) {
  form.value.endpoints.splice(index, 1)
}

const probeDialog = ref<ProbeDialogState>({
  open: false,
  protocol: null,
  targetEndpoint: null,
  loading: false,
  error: null,
  models: [],
  selected: new Set(),
  filterQuery: '',
})

async function openProbeDialog(ep: ChannelEndpoint) {
  probeDialog.value = {
    open: true,
    protocol: ep.protocol,
    targetEndpoint: ep,
    loading: true,
    error: null,
    models: [],
    selected: new Set(ep.models.map((m) => m.name)),
    filterQuery: '',
  }

  const result = await probeModels.run(async () => {
    const effectiveAddress =
      ep.address.unofficial || ep.address.full_address || !!ep.address.base_url
        ? ep.address
        : form.value.address
    const effectiveCredential =
      ep.credential ?? (credentialsText.value.split('\n')[0]?.trim() || '')
    return store.probeDirect({
      protocol: ep.protocol,
      address: effectiveAddress,
      credential: effectiveCredential,
      proxy: form.value.proxy || null,
    })
  })
  probeDialog.value.loading = false
  if (result === undefined) {
    probeDialog.value.error = probeModels.error
    return
  }
  probeDialog.value.models = result.models
  const all = new Set(ep.models.map((m) => m.name))
  for (const m of result.models) all.add(m.id)
  probeDialog.value.selected = all
}

function retryProbe() {
  const ep = probeDialog.value.targetEndpoint
  if (ep) void openProbeDialog(ep)
}

function applyProbeModels() {
  const ep = probeDialog.value.targetEndpoint
  if (!ep) return
  const currentMap = new Map(ep.models.map((m) => [m.name, m]))
  const newModels: ModelEntry[] = []

  for (const id of probeDialog.value.selected) {
    if (currentMap.has(id)) {
      newModels.push(currentMap.get(id)!)
    } else {
      newModels.push({ name: id, upstream: null })
    }
  }

  ep.models = newModels
  probeDialog.value.open = false
}

const validation = computed(() => validateChannel(form.value, credentialsText.value))

const canSave = computed(
  () =>
    validation.value.length === 0 &&
    !saving.value &&
    paramOverrideError.value === null &&
    headersError.value === null,
)

async function save() {
  if (!canSave.value) return
  isSubmitting.value = true
  loadChannel.clear()
  destroyChannel.clear()

  const saved = await saveChannel.run(
    async () => {
      const builtOverride = overrideFromDraft(overrideDraft.value)
      const builtHeaders = headersFromRows(headerRows.value)
      const payload: Channel = {
        ...form.value,
        priority: numOr(form.value.priority, 0),
        weight: numOr(form.value.weight, 1),
        timeout_secs: numOr(form.value.timeout_secs, 0),
        endpoints: form.value.endpoints.map((ep) => ({ ...ep, order: numOr(ep.order, 0) })),
        empty_response_retry: {
          window_secs: numOrNull(form.value.empty_response_retry.window_secs),
          max_retries: numOrNull(form.value.empty_response_retry.max_retries),
        },
        tags: tagsText.value
          .split(',')
          .map((t) => t.trim())
          .filter(Boolean),
        credential: '',
        credentials: poolCredentials(credentialsText.value),
        param_override: builtOverride.value,
        extra_headers: builtHeaders.headers,
        proxy: form.value.proxy?.trim() || null,
        note: form.value.note?.trim() || null,
        test_model: form.value.test_model?.trim() || null,
        visibility: props.scope === 'me' ? 'private' : (form.value.visibility ?? 'shared'),
      }

      if (isEdit.value) await store.update(payload)
      else await store.create(payload)
    },
    () => {
      router.push(listPath())
      return m.ch_edit_saved_msg()
    },
  )
  if (saved === undefined) isSubmitting.value = false
}

async function destroy() {
  if (!isEdit.value || destroying.value) return
  isSubmitting.value = true
  loadChannel.clear()
  saveChannel.clear()
  const removed = await destroyChannel.run(
    () => store.remove(editingId.value as number),
    () => {
      router.push(listPath())
      return m.ch_edit_deleted_msg()
    },
  )
  if (removed === undefined) isSubmitting.value = false
}
</script>

<template>
  <div class="mx-auto max-w-4xl pb-16">
    <header class="mb-6 flex items-end justify-between gap-4">
      <div>
        <h1 class="text-2xl font-semibold">
          {{ isEdit ? m.ch_edit_title_edit() : m.ch_edit_title_new() }}
        </h1>
        <p class="mt-1 text-sm text-ink-faint">
          {{ m.ch_edit_subtitle() }}
        </p>
      </div>
      <button
        type="button"
        class="glass-button-ghost px-3.5 py-2 text-sm"
        @click="router.push(listPath())"
      >
        {{ m.common_back() }}
      </button>
    </header>

    <div v-if="loading" class="py-24 text-center">
      <GlassSpinner size="lg" :label="m.ch_edit_loading()" />
    </div>

    <form v-else class="flex flex-col gap-4" @submit.prevent="save">
      <ChannelBasicsSection
        v-model="form"
        v-model:tags-text="tagsText"
        :hide-visibility="props.scope === 'me'"
      />

      <section class="glass glass-specular p-5">
        <h2 class="mb-1 text-sm font-semibold text-ink-soft uppercase">
          {{ m.ch_edit_default_addr_title() }}
        </h2>
        <p class="mb-4 text-xs text-ink-faint">
          {{ m.ch_edit_default_addr_desc() }}
        </p>
        <UpstreamAddressFields v-model="form.address" variant="channel" />
      </section>

      <KeyPoolSection v-model="form" v-model:credentials-text="credentialsText" />

      <section class="glass glass-specular p-5">
        <div class="mb-1 flex items-center justify-between">
          <h2 class="text-sm font-semibold text-ink-soft uppercase">
            {{ m.ch_edit_endpoints_title() }}
          </h2>
          <button
            v-if="isAggregate && availableProtocols.length > 0"
            type="button"
            class="glass-button-ghost px-3 py-1.5 text-xs font-medium"
            @click="addEndpoint"
          >
            <AppIcon name="plus" :size="14" />
            {{ m.ch_edit_add_endpoint() }}
          </button>
        </div>
        <p class="mb-4 text-xs text-ink-faint">
          {{ isAggregate ? m.ch_edit_endpoints_hint_agg() : m.ch_edit_endpoints_hint_single() }}
        </p>

        <div class="flex flex-col gap-4">
          <EndpointCard
            v-for="(ep, i) in form.endpoints"
            :key="ep.protocol"
            v-model="form.endpoints[i]!"
            :is-aggregate="isAggregate"
            :taken-protocols="form.endpoints.map((e) => e.protocol)"
            :channel-address="form.address"
            :can-remove="isAggregate && form.endpoints.length > 1"
            @remove="removeEndpoint(i)"
            @probe="openProbeDialog(ep)"
          />
        </div>
      </section>

      <ChannelAdvancedSection
        v-model="form"
        v-model:show-advanced="showAdvanced"
        v-model:override-draft="overrideDraft"
        v-model:header-rows="headerRows"
      />

      <div v-if="validation.length > 0" class="glass border-warning/30 p-4">
        <p class="mb-2 text-xs font-medium text-warning">{{ m.ch_edit_validation_errors() }}</p>
        <ul class="list-inside list-disc text-xs text-ink-soft">
          <li v-for="e in validation" :key="e">{{ e }}</li>
        </ul>
      </div>

      <p v-if="saveError" class="glass border-danger/30 p-4 text-sm text-danger">{{ saveError }}</p>

      <div class="flex items-center gap-3">
        <button
          type="submit"
          class="glass-button-primary px-5 py-2.5 text-sm font-medium disabled:opacity-50"
          :disabled="!canSave"
        >
          <AppIcon v-if="saving" name="spinner" class="animate-spin mr-1" :size="15" />
          {{
            saving
              ? m.common_saving()
              : isEdit
                ? m.ch_edit_save_changes()
                : m.ch_edit_create_channel()
          }}
        </button>

        <button
          type="button"
          class="glass-button-ghost px-4 py-2.5 text-sm"
          :disabled="saving || destroying"
          @click="router.push(listPath())"
        >
          {{ m.common_cancel() }}
        </button>

        <template v-if="isEdit">
          <button
            v-if="!pendingDelete"
            type="button"
            class="glass-button-ghost glass-button-ghost-danger ml-auto px-4 py-2.5 text-sm !text-ink-faint hover:!text-danger"
            :disabled="saving || destroying"
            @click="pendingDelete = true"
          >
            <AppIcon name="trash" :size="14" />
            {{ m.ch_edit_delete_channel() }}
          </button>
          <div v-else class="ml-auto flex items-center gap-2">
            <button
              type="button"
              class="inline-flex items-center gap-1 rounded-lg bg-danger px-3.5 py-2 text-sm font-medium text-white hover:brightness-105 disabled:opacity-50"
              :disabled="destroying"
              @click="destroy"
            >
              <AppIcon v-if="destroying" name="spinner" class="animate-spin" :size="13" />
              {{ destroying ? m.common_deleting() : m.ch_edit_confirm_delete_btn() }}
            </button>
            <button
              type="button"
              class="glass-button-ghost px-3 py-2 text-sm"
              :disabled="destroying"
              @click="pendingDelete = false"
            >
              {{ m.common_cancel() }}
            </button>
          </div>
        </template>
      </div>
    </form>

    <ProbeModelsDialog v-model="probeDialog" @apply="applyProbeModels" @retry="retryProbe" />
  </div>
</template>
