<script setup lang="ts">
/**
 * 渠道编辑器 —— 需求 2/3/4/5 全部落在这一页。
 *
 * 设计要点：**表单结构跟着领域模型走，而不是跟着后端表结构走**。
 * 单协议渠道和聚合渠道在数据上都是「渠道 + 端点数组」，所以这里也用
 * 同一套端点编辑器，只是单协议时锁死数量为 1 并同步协议 —— 用户不会
 * 感到自己在填两种不同的表单。
 */
import { computed, nextTick, onBeforeUnmount, onMounted, ref, watch } from 'vue'
import { onBeforeRouteLeave, useRoute, useRouter } from 'vue-router'
import { useToastStore } from '@/stores/toast'
import {
  DialogClose,
  DialogContent,
  DialogDescription,
  DialogOverlay,
  DialogPortal,
  DialogRoot,
  DialogTitle,
} from 'reka-ui'
import ProtocolBadge from '@/components/ProtocolBadge.vue'
import GlassSwitch from '@/components/GlassSwitch.vue'
import GlassSpinner from '@/components/GlassSpinner.vue'
import AppIcon from '@/components/AppIcon.vue'
import {
  PROTOCOL_LABEL,
  PROTOCOL_ORDER,
  toggleProtocol,
  withoutProtocol,
} from '@/components/protocol'
import { useChannelsStore } from '@/stores/channels'
import { channels as channelsApi } from '@/api/client'
import { numOrNull, numOr } from '@/utils/num'
import type {
  Channel,
  ChannelEndpoint,
  ChannelKind,
  KeyStrategy,
  ModelEntry,
  ModelProbe,
  Protocol,
  UpstreamAddress,
} from '@refract/contracts'

const route = useRoute()
const router = useRouter()
const store = useChannelsStore()
const toastStore = useToastStore()
const pristineSnapshot = ref('')
const isSubmitting = ref(false)

function getSnapshot(): string {
  return JSON.stringify({
    form: form.value,
    tagsText: tagsText.value,
    credentialsText: credentialsText.value,
    paramOverrideText: paramOverrideText.value,
    headersText: headersText.value,
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
    const confirm = window.confirm('有未保存的渠道配置更改，确定要离开吗？')
    if (!confirm) return false
  }
})

const editingId = computed(() => {
  const raw = route.params.id
  return typeof raw === 'string' ? Number(raw) : null
})
const isEdit = computed(() => editingId.value !== null && !Number.isNaN(editingId.value))

/** 展示顺序即后端 `Protocol::ALL` 的顺序。 */
const ALL_PROTOCOLS = PROTOCOL_ORDER

const KIND_OPTIONS: { value: ChannelKind; label: string; hint: string }[] = [
  { value: 'chat', label: 'Chat', hint: 'OpenAI Chat Completions' },
  { value: 'responses', label: 'Responses', hint: 'OpenAI Responses API' },
  { value: 'messages', label: 'Messages', hint: 'Anthropic Messages' },
  { value: 'gemini', label: 'Gemini', hint: 'Google Gemini' },
  { value: 'aggregate', label: '聚合', hint: '一个渠道挂多个协议端点' },
]

/** 协议默认地址段，用作输入框 placeholder —— 让用户知道留空会得到什么。 */
const PROTO_DEFAULTS: Record<Protocol, { base: string; prefix: string; path: string }> = {
  chat: { base: 'https://api.openai.com', prefix: '/v1', path: '/chat/completions' },
  responses: { base: 'https://api.openai.com', prefix: '/v1', path: '/responses' },
  messages: { base: 'https://api.anthropic.com', prefix: '/v1', path: '/messages' },
  gemini: {
    base: 'https://generativelanguage.googleapis.com',
    prefix: '/v1beta',
    path: '/models/{model}:{action}',
  },
}

function emptyAddress(): UpstreamAddress {
  return {
    unofficial: false,
    full_address: false,
    base_url: null,
    version_prefix: null,
    path: null,
  }
}

function newEndpoint(protocol: Protocol, order = 0): ChannelEndpoint {
  return {
    protocol,
    order,
    enabled: true,
    address: emptyAddress(),
    credential: null,
    models: [],
    transcode: { enabled: false, accepted: [] },
  }
}

function blankChannel(): Channel {
  return {
    id: 0,
    owner_id: 1,
    name: '',
    kind: 'chat',
    enabled: true,
    priority: 0,
    weight: 1,
    credential: '',
    credentials: [],
    key_strategy: 'round_robin',
    endpoints: [newEndpoint('chat')],
    address: emptyAddress(),
    tags: [],
    timeout_secs: 0,
    proxy: null,
    param_override: null,
    note: null,
    empty_response_retry: { window_secs: null, max_retries: null },
  }
}

const form = ref<Channel>(blankChannel())
const loading = ref(false)
const saving = ref(false)
const destroying = ref(false)
const saveError = ref<string | null>(null)
const showCredential = ref(false)
/**
 * 端点级 UI 状态一律按协议名索引，而不是数组下标 ——
 * 聚合渠道里协议唯一，且移除端点（splice）不会让状态错位到别的端点上。
 */
const showEndpointCredential = ref<Record<string, boolean>>({})
/** 标签用逗号分隔的文本编辑，提交时再切分 —— 逐字符维护数组会打断输入。 */
const tagsText = ref('')
/** 每个端点的「新增模型」输入框。 */
const modelDraft = ref<Record<string, string>>({})
/** 多密钥池：一行一把钥匙的文本编辑（含掩码行），提交时再切分。 */
const credentialsText = ref('')
/** 密钥池使用策略选项，与后端 KeyStrategy 的 serde 值一致。 */
const KEY_STRATEGY_OPTIONS: { value: KeyStrategy; label: string; hint: string }[] = [
  { value: 'round_robin', label: '轮询', hint: '每次请求依次换用池中的钥匙' },
  { value: 'sticky', label: '黏性', hint: '同一调用方固定用一把钥匙，出错才换' },
  { value: 'random', label: '随机', hint: '每次请求随机选一把钥匙' },
]

/** 合并后的钥匙池（忽略空行），供探测快照与保存载荷复用。 */
const poolCredentials = () =>
  credentialsText.value
    .split('\n')
    .map((l) => l.trim())
    .filter(Boolean)

/** 钥匙池行数（忽略空行） */
const poolLineCount = computed(() => poolCredentials().length)

/** 探测结果，供一键同步。 */
const probing = ref<Record<string, boolean>>({})
const probeError = ref<Record<string, string>>({})
const pendingDelete = ref(false)
/** 已保存的探测相关配置快照 —— 地址或凭据改了没保存时，探测走的还是旧配置。 */
const savedProbeConfig = ref('')

// ── 高级配置 ──
// param_override 与 extra_headers 都用文本编辑，提交时解析 ——
// 逐键维护结构化编辑器的复杂度对个位数条目不值得。
const showAdvanced = ref(false)
const paramOverrideText = ref('')
const headersText = ref('')

const paramOverrideError = computed(() => {
  const text = paramOverrideText.value.trim()
  if (!text) return null
  try {
    const parsed: unknown = JSON.parse(text)
    if (parsed === null || typeof parsed !== 'object' || Array.isArray(parsed)) {
      return '必须是 JSON 对象'
    }
    return null
  } catch (e) {
    return e instanceof Error ? `JSON 解析失败：${e.message}` : 'JSON 解析失败'
  }
})

/** 每行 `Name: Value`；空行忽略。 */
function parseHeaders(text: string): [string, string][] | string {
  const out: [string, string][] = []
  for (const raw of text.split('\n')) {
    const line = raw.trim()
    if (!line) continue
    const colon = line.indexOf(':')
    if (colon <= 0) return `无法解析：「${line}」（应为 Name: Value）`
    out.push([line.slice(0, colon).trim(), line.slice(colon + 1).trim()])
  }
  return out
}

const headersError = computed(() => {
  const parsed = parseHeaders(headersText.value)
  return typeof parsed === 'string' ? parsed : null
})

function normalizeEmptyRetryOverride(key: 'window_secs' | 'max_retries') {
  const value = form.value.empty_response_retry[key]
  if (value === null || (value as unknown) === '' || Number.isNaN(value)) {
    form.value.empty_response_retry[key] = null
  }
}

/** 只序列化影响探测的字段：改模型列表这类字段不应该把探测按钮锁掉。 */
function probeConfigOf(ch: Channel, credentials: string[]): string {
  return JSON.stringify({
    kind: ch.kind,
    address: ch.address,
    credentials,
    proxy: ch.proxy,
    endpoints: ch.endpoints.map((e) => ({
      protocol: e.protocol,
      address: e.address,
      credential: e.credential,
    })),
  })
}

/** 表单里影响探测的配置与已保存版本不一致 —— 此时探测结果会误导用户。 */
const probeStale = computed(
  () => isEdit.value && savedProbeConfig.value !== probeConfigOf(form.value, poolCredentials()),
)

onMounted(async () => {
  window.addEventListener('beforeunload', onBeforeUnload)
  if (!isEdit.value) {
    pristineSnapshot.value = getSnapshot()
    return
  }
  loading.value = true
  try {
    const ch = await channelsApi.get(editingId.value as number)
    ch.empty_response_retry ??= { window_secs: null, max_retries: null }
    form.value = ch
    tagsText.value = (ch.tags ?? []).join(', ')
    paramOverrideText.value = ch.param_override ? JSON.stringify(ch.param_override, null, 2) : ''
    headersText.value = (ch.extra_headers ?? []).map(([k, v]) => `${k}: ${v}`).join('\n')
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
    savedProbeConfig.value = probeConfigOf(ch, allCreds)
  } catch (e) {
    saveError.value = e instanceof Error ? e.message : '加载渠道失败'
  } finally {
    loading.value = false
  }
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
    // 切到单协议：只留一个端点，协议强制对齐。
    const first = form.value.endpoints[0] ?? newEndpoint(kind)
    const protocolChanged = first.protocol !== kind
    first.protocol = kind
    // 原生协议不能出现在自己的转换列表里。
    first.transcode.accepted = withoutProtocol(first.transcode.accepted, kind)
    // 协议变了，端点级掩码凭据就还原不了了（后端按协议匹配）—— 清成继承。
    if (protocolChanged && looksMasked(first.credential)) first.credential = null
    form.value.endpoints = [first]
  },
)

const isAggregate = computed(() => form.value.kind === 'aggregate')

/** 聚合渠道里还没被占用的协议，用于「添加端点」。 */
const availableProtocols = computed(() =>
  ALL_PROTOCOLS.filter((p) => !form.value.endpoints.some((e) => e.protocol === p)),
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

/**
 * 值是否是后端返回的脱敏占位符（`sk-a…9f2c` / `••••`）。
 * 真实密钥是 ASCII，这两个字符只可能来自掩码。
 */
function looksMasked(value: string | null | undefined): boolean {
  return !!value && (value.includes('…') || value.includes('•'))
}

/**
 * 端点协议变更：把自己从转换列表里剔掉，否则后端会拒绝保存。
 * 掩码凭据一并清空 —— 后端按协议匹配还原掩码，协议一变就还原不了，
 * 提交会被拒；清成继承渠道默认，让用户重填。
 */
function onEndpointProtocolChange(ep: ChannelEndpoint) {
  ep.transcode.accepted = withoutProtocol(ep.transcode.accepted, ep.protocol)
  if (looksMasked(ep.credential)) ep.credential = null
}

function toggleAccepted(ep: ChannelEndpoint, p: Protocol) {
  if (p === ep.protocol) return
  ep.transcode.accepted = toggleProtocol(ep.transcode.accepted, p)
}

function isAccepted(ep: ChannelEndpoint, p: Protocol): boolean {
  return ep.transcode.accepted.includes(p)
}

/** 端点是否自定义了地址 —— 全空即继承渠道默认。 */
function hasOwnAddress(ep: ChannelEndpoint): boolean {
  const a = ep.address
  return a.unofficial || a.full_address || !!a.base_url || !!a.version_prefix || !!a.path
}

function toggleOwnAddress(ep: ChannelEndpoint, on: boolean) {
  ep.address = on ? { ...emptyAddress(), unofficial: true } : emptyAddress()
}

function parseAndAddModels(ep: ChannelEndpoint, rawText: string): void {
  const trimmed = rawText.trim()
  if (!trimmed) return

  // 支持逗号（中英文）、分号（中英文）、换行、空格、Tab 分隔的批量输入，也支持 `别名=上游名` 映射
  const items = trimmed.split(/[\n,，;； \t]+/).filter(Boolean)
  const existing = new Set(ep.models.map((m) => m.name))

  for (const item of items) {
    const [name, upstream] = item.includes('=') ? item.split('=', 2) : [item, undefined]
    const trimmedName = name!.trim()
    if (!trimmedName || existing.has(trimmedName)) continue
    ep.models.push({ name: trimmedName, upstream: upstream?.trim() || null })
    existing.add(trimmedName)
  }
}

function addModel(ep: ChannelEndpoint) {
  const raw = (modelDraft.value[ep.protocol] ?? '').trim()
  modelDraft.value[ep.protocol] = ''
  parseAndAddModels(ep, raw)
}

function onModelPaste(e: ClipboardEvent, ep: ChannelEndpoint) {
  const pasted = e.clipboardData?.getData('text') ?? ''
  if (/[\n,，;； \t]/.test(pasted)) {
    e.preventDefault()
    modelDraft.value[ep.protocol] = ''
    parseAndAddModels(ep, pasted)
  }
}

function removeModel(ep: ChannelEndpoint, modelIndex: number) {
  ep.models.splice(modelIndex, 1)
}

/** 正在原地编辑上游名的模型，key 为 `${协议}:${对外名}`。 */
const editingModelKey = ref<string | null>(null)
/** 编辑中的上游名草稿。 */
const editingUpstreamDraft = ref('')
/** 编辑输入框本体，用于进入编辑态后立刻聚焦并全选。 */
const editMappingInput = ref<HTMLInputElement | null>(null)

function modelKey(protocol: Protocol, name: string): string {
  return `${protocol}:${name}`
}

function startEditMapping(ep: ChannelEndpoint, m: ModelEntry) {
  editingModelKey.value = modelKey(ep.protocol, m.name)
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

/** 提交编辑：留空清除映射，回到与对外名相同。 */
function commitEditMapping(ep: ChannelEndpoint, m: ModelEntry) {
  if (editingModelKey.value !== modelKey(ep.protocol, m.name)) return
  const draft = editingUpstreamDraft.value.trim()
  m.upstream = draft === '' || draft === m.name ? null : draft
  cancelEditMapping()
}

function clearAllModels(ep: ChannelEndpoint) {
  ep.models = []
}

interface ProbeDialogState {
  open: boolean
  protocol: Protocol | null
  targetEndpoint: ChannelEndpoint | null
  loading: boolean
  error: string | null
  models: ModelProbe[]
  selected: Set<string>
  filterQuery: string
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

const filteredProbeModels = computed(() => {
  const q = probeDialog.value.filterQuery.trim().toLowerCase()
  if (!q) return probeDialog.value.models
  return probeDialog.value.models.filter(
    (m) =>
      m.id.toLowerCase().includes(q) ||
      (m.display_name && m.display_name.toLowerCase().includes(q)),
  )
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

  try {
    const effectiveAddress =
      ep.address.unofficial || ep.address.full_address || !!ep.address.base_url
        ? ep.address
        : form.value.address
    const effectiveCredential =
      ep.credential ?? (credentialsText.value.split('\n')[0]?.trim() || '')

    const res = await store.probeDirect({
      protocol: ep.protocol,
      address: effectiveAddress,
      credential: effectiveCredential,
      proxy: form.value.proxy || null,
    })

    probeDialog.value.models = res.models
    probeDialog.value.loading = false
    const all = new Set(ep.models.map((m) => m.name))
    for (const m of res.models) all.add(m.id)
    probeDialog.value.selected = all
  } catch (e) {
    probeDialog.value.loading = false
    probeDialog.value.error =
      e instanceof Error ? e.message : '探测上游模型列表失败，请检查 Base URL 和密钥是否正确'
  }
}

function toggleProbeSelected(id: string) {
  const s = new Set(probeDialog.value.selected)
  if (s.has(id)) s.delete(id)
  else s.add(id)
  probeDialog.value.selected = s
}

function selectAllFiltered() {
  const s = new Set(probeDialog.value.selected)
  for (const m of filteredProbeModels.value) s.add(m.id)
  probeDialog.value.selected = s
}

function deselectAllFiltered() {
  const s = new Set(probeDialog.value.selected)
  for (const m of filteredProbeModels.value) s.delete(m.id)
  probeDialog.value.selected = s
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

/** 客户端校验，与后端 `Channel::validate` 的规则一致。 */
const validation = computed<string[]>(() => {
  const errors: string[] = []
  const f = form.value

  if (!f.name.trim()) errors.push('渠道名不能为空')
  if (f.endpoints.length === 0) errors.push('至少需要一个协议端点')

  const emptyWindow = f.empty_response_retry.window_secs
  if (
    emptyWindow !== null &&
    (!Number.isInteger(emptyWindow) || emptyWindow < 0 || emptyWindow > 3600)
  ) {
    errors.push('空回复判定窗口必须留空，或填写 0–3600 的整数')
  }
  const emptyRetries = f.empty_response_retry.max_retries
  if (
    emptyRetries !== null &&
    (!Number.isInteger(emptyRetries) || emptyRetries < 0 || emptyRetries > 100)
  ) {
    errors.push('空回复最大重试必须留空，或填写 0–100 的整数')
  }

  if (f.kind !== 'aggregate') {
    if (f.endpoints.length !== 1) errors.push('单协议渠道必须恰好一个端点')
    else if (f.endpoints[0]!.protocol !== f.kind)
      errors.push('单协议渠道的端点协议必须与渠道类型一致')
  }

  const seen = new Set<Protocol>()
  for (const ep of f.endpoints) {
    if (seen.has(ep.protocol)) errors.push(`协议 ${ep.protocol} 出现了多个端点`)
    seen.add(ep.protocol)

    if (ep.transcode.accepted.includes(ep.protocol))
      errors.push(`端点 ${ep.protocol} 不能把自己的原生协议列为转换目标`)

    const hasOwn = !!ep.credential && ep.credential.trim() !== ''
    // 主密钥与钥匙池任一非空即可兜底。
    const hasDefault = credentialsText.value.split('\n').some((line) => line.trim() !== '')
    if (!hasOwn && !hasDefault) errors.push(`端点 ${ep.protocol} 没有密钥，且渠道默认密钥为空`)

    // 地址校验只在非官方且非完整地址时有意义。
    const addr = hasOwnAddress(ep) ? ep.address : f.address
    if (addr.unofficial && !addr.base_url?.trim())
      errors.push(`端点 ${ep.protocol} 开启了非官方地址但没填 base URL`)
  }

  return errors
})

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
  saving.value = true
  saveError.value = null

  const parsedHeaders = parseHeaders(headersText.value)
  const payload: Channel = {
    ...form.value,
    // 数字输入清空时 v-model.number 留下空串，serde 拒收；归一回默认值。
    priority: numOr(form.value.priority, 0),
    weight: numOr(form.value.weight, 1),
    timeout_secs: numOr(form.value.timeout_secs, 0),
    endpoints: form.value.endpoints.map((ep) => ({ ...ep, order: numOr(ep.order, 0) })),
    // 可空数字清空即「继承全局」，与价表 cached_input 的空语义一致。
    empty_response_retry: {
      window_secs: numOrNull(form.value.empty_response_retry.window_secs),
      max_retries: numOrNull(form.value.empty_response_retry.max_retries),
    },
    tags: tagsText.value
      .split(',')
      .map((t) => t.trim())
      .filter(Boolean),
    // 密钥池：一行一把，空行忽略；掩码行由后端按值还原成真实密钥。
    credential: '',
    credentials: poolCredentials(),
    param_override: paramOverrideText.value.trim()
      ? (JSON.parse(paramOverrideText.value) as Record<string, unknown>)
      : null,
    extra_headers: typeof parsedHeaders === 'string' ? [] : parsedHeaders,
    proxy: form.value.proxy?.trim() || null,
    note: form.value.note?.trim() || null,
    test_model: form.value.test_model?.trim() || null,
  }

  try {
    if (isEdit.value) await store.update(payload)
    else await store.create(payload)
    toastStore.success('渠道已保存')
    router.push('/channels')
  } catch (e) {
    saveError.value = e instanceof Error ? e.message : '保存失败'
    toastStore.danger(saveError.value)
    isSubmitting.value = false
  } finally {
    saving.value = false
  }
}

async function destroy() {
  if (!isEdit.value || destroying.value) return
  isSubmitting.value = true
  destroying.value = true
  try {
    await store.remove(editingId.value as number)
    toastStore.success('渠道已删除')
    router.push('/channels')
  } catch (e) {
    saveError.value = e instanceof Error ? e.message : '删除失败'
    toastStore.danger(saveError.value)
    isSubmitting.value = false
  } finally {
    destroying.value = false
  }
}

/**
 * 与后端 `join_segments` 相同的拼接语义：base 去尾斜杠，
 * 每段 trim、空段跳过、缺前导斜杠则补、尾斜杠去掉。
 * 预览必须和实际请求一字不差，否则就是在误导。
 */
function joinSegments(base: string, segments: (string | null | undefined)[]): string {
  let out = base.replace(/\/+$/, '')
  for (const raw of segments) {
    const seg = raw?.trim()
    if (!seg) continue
    if (!seg.startsWith('/')) out += '/'
    out += seg.replace(/\/+$/, '')
  }
  return out
}

/** 拼出的最终地址预览，让用户在保存前就看到会打到哪。 */
function previewUrl(ep: ChannelEndpoint): string {
  const a = hasOwnAddress(ep) ? ep.address : form.value.address
  const d = PROTO_DEFAULTS[ep.protocol]
  if (!a.unofficial) return joinSegments(d.base, [d.prefix, d.path])
  if (a.full_address) return a.base_url?.trim() || '（未填完整地址）'
  const base = a.base_url?.trim()
  if (!base) return '（未填 base URL）'
  return joinSegments(base, [a.version_prefix?.trim() || d.prefix, a.path?.trim() || d.path])
}
</script>

<template>
  <div class="mx-auto max-w-4xl pb-16">
    <header class="mb-6 flex items-end justify-between gap-4">
      <div>
        <h1 class="text-2xl font-semibold">
          {{ isEdit ? '编辑渠道' : '新建渠道' }}
        </h1>
        <p class="mt-1 text-sm text-ink-faint">
          渠道类型即协议 —— 与厂商无关，同一个中转站可以用聚合渠道表达多种协议。
        </p>
      </div>
      <button
        type="button"
        class="glass-button-ghost px-3.5 py-2 text-sm"
        @click="router.push('/channels')"
      >
        返回
      </button>
    </header>

    <div v-if="loading" class="py-24 text-center">
      <GlassSpinner size="lg" label="正在读取渠道配置…" />
    </div>

    <form v-else class="flex flex-col gap-4" @submit.prevent="save">
      <!-- 基础信息 -->
      <section class="glass glass-specular p-5">
        <h2 class="mb-4 text-sm font-semibold text-ink-soft uppercase">基础</h2>

        <div class="grid grid-cols-1 gap-4 sm:grid-cols-2">
          <label class="flex flex-col gap-1.5">
            <input
              v-model="form.name"
              type="text"
              placeholder="例如：中转站-主力"
              class="glass-field px-3 py-2 text-sm outline-none"
            />
          </label>

          <label class="flex flex-col gap-1.5">
            <span class="text-xs font-medium text-ink-soft">类型</span>
            <select v-model="form.kind" class="glass-field px-3 py-2 text-sm outline-none">
              <option v-for="k in KIND_OPTIONS" :key="k.value" :value="k.value">
                {{ k.label }} — {{ k.hint }}
              </option>
            </select>
          </label>

          <label class="flex flex-col gap-1.5">
            <span class="text-xs font-medium text-ink-soft">
              优先级
              <span class="font-normal text-ink-faint">越大越优先</span>
            </span>
            <input
              v-model.number="form.priority"
              type="number"
              class="glass-field tabular px-3 py-2 text-sm outline-none"
            />
          </label>

          <label class="flex flex-col gap-1.5">
            <span class="text-xs font-medium text-ink-soft">
              权重
              <span class="font-normal text-ink-faint">同优先级内的加权随机</span>
            </span>
            <input
              v-model.number="form.weight"
              type="number"
              min="0"
              class="glass-field tabular px-3 py-2 text-sm outline-none"
            />
          </label>

          <label class="flex flex-col gap-1.5">
            <span class="text-xs font-medium text-ink-soft">
              超时（秒）
              <span class="font-normal text-ink-faint">0 用全局默认</span>
            </span>
            <input
              v-model.number="form.timeout_secs"
              type="number"
              min="0"
              class="glass-field tabular px-3 py-2 text-sm outline-none"
            />
          </label>

          <label class="flex flex-col gap-1.5">
            <span class="text-xs font-medium text-ink-soft"
              >标签<span class="font-normal text-ink-faint">，逗号分隔</span></span
            >
            <input
              v-model="tagsText"
              type="text"
              placeholder="生产, 便宜"
              class="glass-field px-3 py-2 text-sm outline-none"
            />
          </label>
        </div>

        <label class="mt-4 flex cursor-pointer items-center gap-3">
          <GlassSwitch v-model="form.enabled" label="启用渠道" tone="success" />
          <span class="text-sm">启用</span>
        </label>
      </section>

      <!-- 渠道默认地址 -->
      <section class="glass glass-specular p-5">
        <h2 class="mb-1 text-sm font-semibold text-ink-soft uppercase">默认地址</h2>
        <p class="mb-4 text-xs text-ink-faint">
          端点未单独配置地址时继承这里。三段拼接：base URL + 版本前缀 + 路径。
        </p>

        <div class="flex flex-col gap-3">
          <label class="flex cursor-pointer items-center gap-3">
            <GlassSwitch v-model="form.address.unofficial" label="非官方地址" />
            <span class="text-sm">
              <span class="font-medium">非官方地址</span>
              <span class="ml-2 text-xs text-ink-faint"> 关闭时一律使用协议官方地址 </span>
            </span>
          </label>

          <label v-if="form.address.unofficial" class="flex cursor-pointer items-center gap-3">
            <GlassSwitch v-model="form.address.full_address" label="完整地址" />
            <span class="text-sm">
              <span class="font-medium">完整地址</span>
              <span class="ml-2 text-xs text-ink-faint">直接指定最终 URL，不拼接不校验</span>
            </span>
          </label>

          <template v-if="form.address.unofficial">
            <input
              v-if="form.address.full_address"
              v-model="form.address.base_url"
              type="text"
              placeholder="https://proxy.example.com/openai/v1/chat/completions"
              class="glass-field px-3 py-2 font-mono text-sm outline-none"
            />
            <div v-else class="grid grid-cols-1 gap-3 sm:grid-cols-3">
              <input
                v-model="form.address.base_url"
                type="text"
                placeholder="https://api.example.com"
                class="glass-field px-3 py-2 font-mono text-sm outline-none sm:col-span-1"
              />
              <input
                v-model="form.address.version_prefix"
                type="text"
                placeholder="/v1（留空用协议默认）"
                class="glass-field px-3 py-2 font-mono text-sm outline-none"
              />
              <input
                v-model="form.address.path"
                type="text"
                placeholder="/chat/completions（留空用协议默认）"
                class="glass-field px-3 py-2 font-mono text-sm outline-none"
              />
            </div>
          </template>

          <p v-else class="rounded-lg bg-ink/5 px-3 py-2 text-xs text-ink-faint">
            使用各协议的官方地址。要接中转站请打开「非官方地址」。
          </p>
        </div>
      </section>

      <!-- 默认凭据 -->
      <section class="glass glass-specular p-5">
        <div class="mb-1 flex items-center justify-between">
          <h2 class="text-sm font-semibold text-ink-soft uppercase">默认密钥</h2>
          <button
            type="button"
            class="rounded px-2 py-1 text-xs text-ink-faint hover:text-ink"
            :aria-pressed="showCredential"
            @click="showCredential = !showCredential"
          >
            {{ showCredential ? '隐藏' : '显示' }}
          </button>
        </div>
        <p class="mb-4 text-xs text-ink-faint">
          端点未单独配置密钥时继承这里。每行一把，支持多把钥匙轮换。
        </p>

        <!-- 密钥池：一行一把，保存时原样回传掩码行由后端还原 -->
        <div class="mt-4">
          <textarea
            id="credentials-pool"
            v-model="credentialsText"
            rows="4"
            spellcheck="false"
            autocomplete="new-password"
            placeholder="sk-...&#10;sk-...&#10;sk-..."
            aria-label="上游钥匙池，每行一把"
            class="glass-field w-full resize-y px-3 py-2 font-mono text-sm outline-none"
            :class="showCredential ? undefined : '[webkit-text-security:disc]'"
          ></textarea>
          <p class="mt-1 text-xs text-ink-faint">
            {{ poolLineCount }} 把钥匙参与轮换；留空则不可用（端点也未配置时）。
          </p>
        </div>

        <!-- 钥匙池策略 -->
        <div class="mt-4 flex flex-col gap-1.5">
          <span class="text-xs font-medium text-ink-soft">钥匙池策略</span>
          <div class="flex flex-wrap gap-2">
            <label
              v-for="option in KEY_STRATEGY_OPTIONS"
              :key="option.value"
              class="glass-field flex cursor-pointer items-center gap-2 px-3 py-2 text-sm"
            >
              <input
                v-model="form.key_strategy"
                type="radio"
                name="key-strategy"
                :value="option.value"
                class="accent-accent"
              />
              <span>{{ option.label }}</span>
              <span class="text-xs text-ink-faint">{{ option.hint }}</span>
            </label>
          </div>
        </div>
      </section>

      <!-- 端点 -->
      <section class="glass glass-specular p-5">
        <div class="mb-1 flex items-center justify-between">
          <h2 class="text-sm font-semibold text-ink-soft uppercase">协议端点</h2>
          <button
            v-if="isAggregate && availableProtocols.length > 0"
            type="button"
            class="glass-button-ghost px-3 py-1.5 text-xs font-medium"
            @click="addEndpoint"
          >
            <AppIcon name="plus" :size="14" />
            添加端点
          </button>
        </div>
        <p class="mb-4 text-xs text-ink-faint">
          {{
            isAggregate
              ? '每个端点可独立配置地址、密钥、模型与协议转换。order 越小越优先命中。'
              : '单协议渠道恰好一个端点，协议与渠道类型一致。'
          }}
        </p>

        <div class="flex flex-col gap-4">
          <article
            v-for="(ep, i) in form.endpoints"
            :key="ep.protocol"
            class="rounded-xl border border-ink/10 bg-black/5 p-4 dark:bg-white/5"
          >
            <div class="mb-3 flex flex-wrap items-center gap-3">
              <select
                v-model="ep.protocol"
                :disabled="!isAggregate"
                class="glass-field px-3 py-1.5 text-sm outline-none disabled:opacity-60"
                @change="onEndpointProtocolChange(ep)"
              >
                <option
                  v-for="p in ALL_PROTOCOLS"
                  :key="p"
                  :value="p"
                  :disabled="p !== ep.protocol && form.endpoints.some((e) => e.protocol === p)"
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
                启用
              </label>

              <span
                class="w-full break-all font-mono text-[0.7rem] text-ink-faint sm:ml-auto sm:w-auto"
              >
                {{ previewUrl(ep) }}
              </span>

              <button
                v-if="isAggregate && form.endpoints.length > 1"
                type="button"
                class="rounded px-2 py-1 text-xs text-ink-faint hover:bg-danger/12 hover:text-danger"
                @click="removeEndpoint(i)"
              >
                移除
              </button>
            </div>

            <!-- 端点地址覆盖 -->
            <div class="mb-3">
              <label class="flex cursor-pointer items-center gap-2 text-xs text-ink-soft">
                <input
                  type="checkbox"
                  :checked="hasOwnAddress(ep)"
                  class="accent-[var(--color-accent)]"
                  @change="toggleOwnAddress(ep, ($event.target as HTMLInputElement).checked)"
                />
                自定义地址<span class="text-ink-faint">（不勾选则继承渠道默认）</span>
              </label>

              <div v-if="hasOwnAddress(ep)" class="mt-2 flex flex-col gap-2 pl-5">
                <label class="flex cursor-pointer items-center gap-2 text-xs text-ink-soft">
                  <input
                    v-model="ep.address.full_address"
                    type="checkbox"
                    class="accent-[var(--color-accent)]"
                  />
                  完整地址
                </label>

                <input
                  v-if="ep.address.full_address"
                  v-model="ep.address.base_url"
                  type="text"
                  placeholder="https://proxy.example.com/full/path"
                  class="glass-field px-3 py-1.5 font-mono text-xs outline-none"
                />
                <div v-else class="grid grid-cols-1 gap-2 sm:grid-cols-3">
                  <input
                    v-model="ep.address.base_url"
                    type="text"
                    :placeholder="PROTO_DEFAULTS[ep.protocol].base"
                    class="glass-field px-3 py-1.5 font-mono text-xs outline-none"
                  />
                  <input
                    v-model="ep.address.version_prefix"
                    type="text"
                    :placeholder="PROTO_DEFAULTS[ep.protocol].prefix"
                    class="glass-field px-3 py-1.5 font-mono text-xs outline-none"
                  />
                  <input
                    v-model="ep.address.path"
                    type="text"
                    :placeholder="PROTO_DEFAULTS[ep.protocol].path"
                    class="glass-field px-3 py-1.5 font-mono text-xs outline-none"
                  />
                </div>
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
                自定义密钥<span class="text-ink-faint">（不勾选则继承渠道默认）</span>
              </label>

              <div v-if="ep.credential !== null" class="relative mt-2 pl-5">
                <input
                  v-model="ep.credential"
                  :type="showEndpointCredential[ep.protocol] ? 'text' : 'password'"
                  placeholder="sk-..."
                  autocomplete="new-password"
                  :aria-label="`${ep.protocol} 端点 API 密钥`"
                  class="glass-field w-full px-3 py-1.5 pr-16 font-mono text-xs outline-none"
                />
                <button
                  type="button"
                  class="absolute top-1/2 right-2 -translate-y-1/2 rounded px-2 py-0.5 text-[0.7rem] text-ink-faint hover:text-ink"
                  :aria-label="
                    showEndpointCredential[ep.protocol]
                      ? `隐藏 ${ep.protocol} 端点 API 密钥`
                      : `显示 ${ep.protocol} 端点 API 密钥`
                  "
                  :aria-pressed="showEndpointCredential[ep.protocol] === true"
                  @click="
                    showEndpointCredential[ep.protocol] = !showEndpointCredential[ep.protocol]
                  "
                >
                  {{ showEndpointCredential[ep.protocol] ? '隐藏' : '显示' }}
                </button>
              </div>
            </div>

            <!-- 模型列表与选择 -->
            <div class="mb-4">
              <div class="mb-2 flex flex-wrap items-center justify-between gap-2">
                <div class="flex items-center gap-2">
                  <span class="text-xs font-semibold text-ink-soft">模型列表</span>
                  <span
                    class="rounded bg-ink/8 px-1.5 py-0.5 text-[0.7rem] font-medium text-ink-faint"
                  >
                    已选 {{ ep.models.length }} 个
                  </span>
                </div>
                <div class="flex flex-wrap items-center gap-1.5">
                  <!-- 在线拉取模型 (新建或编辑均可用) -->
                  <button
                    type="button"
                    class="glass-button-ghost flex items-center gap-1 px-2.5 py-1 text-xs font-medium text-accent hover:!bg-accent/15"
                    @click="openProbeDialog(ep)"
                  >
                    <AppIcon name="globe" :size="13" />
                    从上游获取模型
                  </button>

                  <!-- 清空 -->
                  <button
                    v-if="ep.models.length > 0"
                    type="button"
                    class="glass-button-ghost px-2 py-1 text-xs text-ink-faint hover:!text-danger"
                    @click="clearAllModels(ep)"
                  >
                    清空
                  </button>
                </div>
              </div>
              <!-- 已选模型标签展示区 -->
              <div v-if="ep.models.length > 0" class="mb-2.5 flex flex-wrap gap-1.5">
                <span
                  v-for="(m, mi) in ep.models"
                  :key="m.name"
                  class="inline-flex items-center gap-1.5 rounded-full border border-ink/8 bg-ink/6 px-2 py-1 font-mono text-xs shadow-xs"
                >
                  <template v-if="editingModelKey === modelKey(ep.protocol, m.name)">
                    <span class="font-medium text-ink">{{ m.name }}</span>
                    <span class="text-[0.7rem] text-ink-faint">→</span>
                    <input
                      :ref="(el) => (editMappingInput = el as HTMLInputElement | null)"
                      :value="editingUpstreamDraft"
                      type="text"
                      :aria-label="`${m.name} 上游名`"
                      :placeholder="m.name"
                      class="glass-field h-auto w-40 px-1.5 py-0.5 font-mono text-[0.7rem] outline-none"
                      @input="editingUpstreamDraft = ($event.target as HTMLInputElement).value"
                      @keydown.enter.prevent="commitEditMapping(ep, m)"
                      @keydown.esc.prevent="cancelEditMapping"
                      @blur="commitEditMapping(ep, m)"
                    />
                  </template>
                  <button
                    v-else
                    type="button"
                    class="cursor-pointer rounded font-medium text-ink hover:text-accent-deep"
                    :title="`编辑 ${m.name} 的上游映射`"
                    @click="startEditMapping(ep, m)"
                  >
                    {{ m.name
                    }}<span v-if="m.upstream" class="ml-1 text-[0.7rem] text-accent-deep"
                      >→{{ m.upstream }}</span
                    >
                  </button>
                  <button
                    type="button"
                    class="grid size-3.5 place-items-center rounded-full text-ink-faint hover:bg-danger/20 hover:text-danger"
                    title="移除模型"
                    @click="removeModel(ep, mi)"
                  >
                    ×
                  </button>
                </span>
              </div>

              <!-- 手动添加输入框 / 批量粘贴 -->
              <div class="relative">
                <input
                  v-model="modelDraft[ep.protocol]"
                  type="text"
                  :aria-label="`${PROTOCOL_LABEL[ep.protocol]} 模型输入`"
                  placeholder="输入模型名（支持批量粘贴或 别名=上游名 映射），按回车添加"
                  class="glass-field w-full px-3 py-1.5 pr-16 font-mono text-xs outline-none"
                  @keydown.enter.prevent="addModel(ep)"
                  @paste="(e) => onModelPaste(e, ep)"
                />
                <button
                  v-if="(modelDraft[ep.protocol] ?? '').trim()"
                  type="button"
                  class="absolute top-1/2 right-1.5 -translate-y-1/2 rounded bg-accent px-2 py-0.5 text-xs text-white"
                  @click="addModel(ep)"
                >
                  添加
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
                <span class="font-medium">协议转换</span>
                <span class="text-ink-faint">允许其他协议的客户端打到这个端点</span>
              </label>

              <div v-if="ep.transcode.enabled" class="mt-2 flex flex-wrap gap-2 pl-5">
                <label
                  v-for="p in ALL_PROTOCOLS"
                  :key="p"
                  class="flex items-center gap-1.5 text-xs"
                  :class="p === ep.protocol ? 'cursor-not-allowed opacity-40' : 'cursor-pointer'"
                >
                  <input
                    type="checkbox"
                    :checked="isAccepted(ep, p)"
                    :disabled="p === ep.protocol"
                    class="accent-[var(--color-accent)]"
                    @change="toggleAccepted(ep, p)"
                  />
                  <ProtocolBadge :protocol="p" />
                  <span v-if="p === ep.protocol" class="text-ink-faint">原生</span>
                </label>
              </div>
              <p v-if="ep.transcode.enabled" class="mt-1.5 pl-5 text-[0.7rem] text-ink-faint">
                未勾选的协议打过来会被直接拒绝，而不是硬转。
              </p>
            </div>
          </article>
        </div>
      </section>

      <!-- 高级 -->
      <section class="glass glass-specular p-5">
        <button
          type="button"
          class="flex w-full items-center justify-between text-left"
          :aria-expanded="showAdvanced"
          @click="showAdvanced = !showAdvanced"
        >
          <span>
            <span class="text-sm font-semibold text-ink-soft uppercase">高级</span>
            <span class="ml-2 text-xs text-ink-faint">
              参数覆盖、自定义请求头、空回复重试、代理、测试模型、备注
            </span>
          </span>
          <span class="text-xs text-ink-faint">{{ showAdvanced ? '收起' : '展开' }}</span>
        </button>

        <div v-if="showAdvanced" class="mt-4 flex flex-col gap-4">
          <label class="flex flex-col gap-1.5">
            <span class="text-xs font-medium text-ink-soft">
              参数覆盖（JSON）
              <span class="font-normal text-ink-faint">
                — 顶层键合并进请求体；值为 null 表示删除该字段；键名为协议名
                （chat/messages/…）且值为对象时只对该协议生效
              </span>
            </span>
            <textarea
              v-model="paramOverrideText"
              rows="5"
              spellcheck="false"
              placeholder='{ "temperature": 0.7, "logprobs": null, "gemini": { "generationConfig": { "topK": 40 } } }'
              class="glass-field px-3 py-2 font-mono text-xs leading-relaxed outline-none"
            ></textarea>
            <span v-if="paramOverrideError" class="text-xs text-danger" role="alert">
              {{ paramOverrideError }}
            </span>
          </label>

          <label class="flex flex-col gap-1.5">
            <span class="text-xs font-medium text-ink-soft">
              自定义请求头
              <span class="font-normal text-ink-faint">
                — 每行一条 Name: Value，随所有上游调用发送；鉴权头由网关掌管不可覆盖
              </span>
            </span>
            <textarea
              v-model="headersText"
              rows="3"
              spellcheck="false"
              placeholder="x-site-token: abc123"
              class="glass-field px-3 py-2 font-mono text-xs leading-relaxed outline-none"
            ></textarea>
            <span v-if="headersError" class="text-xs text-danger" role="alert">
              {{ headersError }}
            </span>
          </label>

          <div>
            <span class="text-xs font-medium text-ink-soft">上游 200 空回复重试</span>
            <p class="mt-1 text-[0.7rem] text-ink-faint">
              留空继承全局设置；填写 0 可为本渠道关闭对应限制。耗时按“完成时刻 − 首字节时刻”计算。
            </p>
            <div class="mt-2 grid grid-cols-1 gap-4 sm:grid-cols-2">
              <label class="flex flex-col gap-1.5">
                <span class="text-xs text-ink-soft">判定窗口（秒）</span>
                <input
                  v-model.number="form.empty_response_retry.window_secs"
                  type="number"
                  min="0"
                  max="3600"
                  step="1"
                  placeholder="留空继承全局"
                  class="glass-field tabular px-3 py-2 text-sm outline-none"
                  @change="normalizeEmptyRetryOverride('window_secs')"
                />
              </label>
              <label class="flex flex-col gap-1.5">
                <span class="text-xs text-ink-soft">最大重试次数</span>
                <input
                  v-model.number="form.empty_response_retry.max_retries"
                  type="number"
                  min="0"
                  max="100"
                  step="1"
                  placeholder="留空继承全局"
                  class="glass-field tabular px-3 py-2 text-sm outline-none"
                  @change="normalizeEmptyRetryOverride('max_retries')"
                />
              </label>
            </div>
          </div>

          <div class="grid grid-cols-1 gap-4 sm:grid-cols-2">
            <label class="flex flex-col gap-1.5">
              <span class="text-xs font-medium text-ink-soft">
                出站代理<span class="font-normal text-ink-faint">，http/socks5</span>
              </span>
              <input
                v-model="form.proxy"
                type="text"
                placeholder="socks5://127.0.0.1:1080"
                class="glass-field px-3 py-2 font-mono text-sm outline-none"
              />
            </label>

            <label class="flex flex-col gap-1.5">
              <span class="text-xs font-medium text-ink-soft">
                测试模型<span class="font-normal text-ink-faint">，连通性测试与定时重测用</span>
              </span>
              <input
                v-model="form.test_model"
                type="text"
                placeholder="留空用端点第一个模型"
                class="glass-field px-3 py-2 font-mono text-sm outline-none"
              />
            </label>
          </div>

          <label class="flex flex-col gap-1.5">
            <span class="text-xs font-medium text-ink-soft">
              备注<span class="font-normal text-ink-faint">，仅自己可见</span>
            </span>
            <input
              v-model="form.note"
              type="text"
              placeholder="主力站，月底记得续费"
              class="glass-field px-3 py-2 text-sm outline-none"
            />
          </label>
        </div>
      </section>

      <!-- 校验与操作 -->
      <div v-if="validation.length > 0" class="glass border-warning/30 p-4">
        <p class="mb-2 text-xs font-medium text-warning">保存前需要修正：</p>
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
          {{ saving ? '保存中…' : isEdit ? '保存修改' : '创建渠道' }}
        </button>

        <button
          type="button"
          class="glass-button-ghost px-4 py-2.5 text-sm"
          :disabled="saving || destroying"
          @click="router.push('/channels')"
        >
          取消
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
            删除渠道
          </button>
          <div v-else class="ml-auto flex items-center gap-2">
            <button
              type="button"
              class="inline-flex items-center gap-1 rounded-lg bg-danger px-3.5 py-2 text-sm font-medium text-white hover:brightness-105 disabled:opacity-50"
              :disabled="destroying"
              @click="destroy"
            >
              <AppIcon v-if="destroying" name="spinner" class="animate-spin" :size="13" />
              {{ destroying ? '删除中…' : '确认删除' }}
            </button>
            <button
              type="button"
              class="glass-button-ghost px-3 py-2 text-sm"
              :disabled="destroying"
              @click="pendingDelete = false"
            >
              取消
            </button>
          </div>
        </template>
      </div>
    </form>

    <!-- 探测模型弹窗 -->
    <DialogRoot v-model:open="probeDialog.open">
      <DialogPortal>
        <DialogOverlay
          class="fixed inset-0 z-50 bg-black/40 backdrop-blur-md data-[state=closed]:opacity-0 data-[state=open]:opacity-100"
        />
        <DialogContent
          class="glass-thick glass-specular fixed top-1/2 left-1/2 z-50 flex max-h-[85vh] w-[calc(100%-2rem)] max-w-2xl -translate-x-1/2 -translate-y-1/2 flex-col !bg-canvas/95 p-6 shadow-2xl outline-none dark:!bg-[#12141c]/95"
        >
          <DialogTitle class="flex items-center justify-between text-lg font-semibold">
            <span class="flex items-center gap-2">
              <AppIcon name="globe" :size="20" class="text-accent" />
              上游模型在线探测
            </span>
            <DialogClose
              class="rounded-lg p-1 text-ink-faint transition-colors hover:bg-ink/5 hover:text-ink cursor-pointer"
            >
              <AppIcon name="x" :size="18" />
            </DialogClose>
          </DialogTitle>

          <DialogDescription class="mt-1 text-xs text-ink-faint">
            已向上游地址探测真实可用模型列表。勾选需要接入的模型后一键导入当前端点。
          </DialogDescription>

          <!-- 加载中 -->
          <div
            v-if="probeDialog.loading"
            class="flex flex-col items-center justify-center py-16 text-center"
          >
            <div
              class="size-8 animate-spin rounded-full border-2 border-accent border-t-transparent"
            ></div>
            <p class="mt-3 text-sm text-ink-soft">正在向目标上游发送模型列表探测请求…</p>
            <p class="mt-1 text-xs text-ink-faint">
              走 {{ probeDialog.protocol }} 协议模型列表接口
            </p>
          </div>

          <!-- 探测失败 -->
          <div
            v-else-if="probeDialog.error"
            class="my-4 rounded-xl border border-danger/30 bg-danger/10 p-4"
          >
            <p class="text-sm font-semibold text-danger">探测失败</p>
            <p class="mt-1 text-xs text-danger/90">{{ probeDialog.error }}</p>
            <p class="mt-2 text-[0.75rem] text-ink-faint">
              请检查上方渠道地址（Base URL）是否正确、API
              密钥是否已填写且有效，若为私有网络请检查网络连通性。
            </p>
            <div class="mt-3 flex justify-end gap-2">
              <DialogClose as="template">
                <button type="button" class="glass-button-ghost px-3 py-1.5 text-xs">关闭</button>
              </DialogClose>
              <button
                v-if="probeDialog.targetEndpoint"
                type="button"
                class="glass-button-primary px-3 py-1.5 text-xs font-medium"
                @click="openProbeDialog(probeDialog.targetEndpoint)"
              >
                重试
              </button>
            </div>
          </div>

          <!-- 探测成功，显示模型多选列表 -->
          <div v-else class="mt-4 flex min-h-0 flex-1 flex-col gap-3">
            <!-- 搜索与快速操作栏 -->
            <div class="flex flex-wrap items-center justify-between gap-2">
              <div class="relative min-w-56 flex-1">
                <input
                  v-model="probeDialog.filterQuery"
                  type="search"
                  placeholder="搜索上游模型 ID…"
                  class="glass-field w-full px-3 py-1.5 text-xs outline-none"
                />
              </div>
              <div class="flex items-center gap-2 text-xs">
                <span class="text-ink-faint">
                  发现 {{ probeDialog.models.length }} 个 · 已选 {{ probeDialog.selected.size }} 个
                </span>
                <button
                  type="button"
                  class="glass-button-ghost px-2 py-1 text-xs"
                  @click="selectAllFiltered"
                >
                  全选{{ probeDialog.filterQuery ? '当前' : '' }}
                </button>
                <button
                  type="button"
                  class="glass-button-ghost px-2 py-1 text-xs"
                  @click="deselectAllFiltered"
                >
                  全不选
                </button>
              </div>
            </div>

            <!-- 模型列表滚动区域 -->
            <div
              v-if="probeDialog.models.length === 0"
              class="py-10 text-center text-sm text-ink-faint"
            >
              上游返回了空模型列表。
            </div>
            <div
              v-else-if="filteredProbeModels.length === 0"
              class="py-10 text-center text-sm text-ink-faint"
            >
              没有匹配 "{{ probeDialog.filterQuery }}" 的模型
            </div>
            <div
              v-else
              class="glass max-h-72 min-h-36 flex-1 divide-y divide-ink/5 overflow-y-auto rounded-xl p-2.5"
            >
              <div
                v-for="m in filteredProbeModels"
                :key="m.id"
                class="flex cursor-pointer items-center justify-between rounded-lg px-2.5 py-1.5 transition-colors hover:bg-ink/5"
                @click="toggleProbeSelected(m.id)"
              >
                <div class="min-w-0 flex-1 pr-2">
                  <p class="truncate font-mono text-xs font-medium text-ink">{{ m.id }}</p>
                  <p
                    v-if="m.display_name && m.display_name !== m.id"
                    class="truncate text-[0.7rem] text-ink-faint"
                  >
                    {{ m.display_name }}
                  </p>
                </div>
                <div
                  class="grid size-5 shrink-0 place-items-center rounded border transition-colors"
                  :class="
                    probeDialog.selected.has(m.id)
                      ? 'border-accent bg-accent text-white'
                      : 'border-ink/20 bg-transparent'
                  "
                >
                  <AppIcon v-if="probeDialog.selected.has(m.id)" name="check" :size="12" />
                </div>
              </div>
            </div>

            <!-- 底部确定按钮 -->
            <div class="mt-2 flex items-center justify-end gap-3 border-t border-ink/8 pt-2">
              <button
                type="button"
                class="glass-button-ghost px-4 py-2 text-xs"
                @click="probeDialog.open = false"
              >
                取消
              </button>
              <button
                type="button"
                class="glass-button-primary px-4 py-2 text-xs font-medium disabled:opacity-50"
                :disabled="probeDialog.selected.size === 0"
                @click="applyProbeModels"
              >
                导入所选 ({{ probeDialog.selected.size }}) 个模型
              </button>
            </div>
          </div>
        </DialogContent>
      </DialogPortal>
    </DialogRoot>
  </div>
</template>
