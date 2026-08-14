<script setup lang="ts">
/**
 * 渠道编辑器 —— 需求 2/3/4/5 全部落在这一页。
 *
 * 设计要点：**表单结构跟着领域模型走，而不是跟着后端表结构走**。
 * 单协议渠道和聚合渠道在数据上都是「渠道 + 端点数组」，所以这里也用
 * 同一套端点编辑器，只是单协议时锁死数量为 1 并同步协议 —— 用户不会
 * 感到自己在填两种不同的表单。
 */
import { computed, onMounted, ref, watch } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import ProtocolBadge from '@/components/ProtocolBadge.vue'
import GlassSwitch from '@/components/GlassSwitch.vue'
import { PROTOCOL_ORDER, toggleProtocol, withoutProtocol } from '@/components/protocol'
import { useChannelsStore } from '@/stores/channels'
import { channels as channelsApi } from '@/api/client'
import type {
  Channel,
  ChannelEndpoint,
  ChannelKind,
  ModelEntry,
  Protocol,
  UpstreamAddress,
} from '@/api/types'

const route = useRoute()
const router = useRouter()
const store = useChannelsStore()

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
    address: emptyAddress(),
    endpoints: [newEndpoint('chat')],
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
function probeConfigOf(ch: Channel): string {
  return JSON.stringify({
    kind: ch.kind,
    address: ch.address,
    credential: ch.credential,
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
  () => isEdit.value && savedProbeConfig.value !== probeConfigOf(form.value),
)

onMounted(async () => {
  if (!isEdit.value) return
  loading.value = true
  try {
    const ch = await channelsApi.get(editingId.value as number)
    ch.empty_response_retry ??= { window_secs: null, max_retries: null }
    form.value = ch
    tagsText.value = (ch.tags ?? []).join(', ')
    paramOverrideText.value = ch.param_override ? JSON.stringify(ch.param_override, null, 2) : ''
    headersText.value = (ch.extra_headers ?? []).map(([k, v]) => `${k}: ${v}`).join('\n')
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
    savedProbeConfig.value = probeConfigOf(ch)
  } catch (e) {
    saveError.value = e instanceof Error ? e.message : '加载渠道失败'
  } finally {
    loading.value = false
  }
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

function addModel(ep: ChannelEndpoint) {
  const raw = (modelDraft.value[ep.protocol] ?? '').trim()
  modelDraft.value[ep.protocol] = ''
  if (!raw) return
  // 支持 `别名=上游名` 的映射写法，这是模型重命名最省事的输入方式。
  const [name, upstream] = raw.includes('=') ? raw.split('=', 2) : [raw, undefined]
  const entry: ModelEntry = { name: name!.trim(), upstream: upstream?.trim() || null }
  if (!entry.name || ep.models.some((m) => m.name === entry.name)) return
  ep.models.push(entry)
}

function removeModel(ep: ChannelEndpoint, modelIndex: number) {
  ep.models.splice(modelIndex, 1)
}

/** 拉取上游真实模型列表并合并进当前端点。 */
async function probeModels(ep: ChannelEndpoint) {
  if (!isEdit.value || probeStale.value) return
  probing.value[ep.protocol] = true
  probeError.value[ep.protocol] = ''
  try {
    const result = await store.probe(editingId.value as number, ep.protocol)
    const existing = new Set(ep.models.map((m) => m.name))
    for (const m of result.models) {
      if (!existing.has(m.id)) ep.models.push({ name: m.id, upstream: null })
    }
  } catch (e) {
    probeError.value[ep.protocol] = e instanceof Error ? e.message : '探测失败'
  } finally {
    probing.value[ep.protocol] = false
  }
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
    if (!hasOwn && !f.credential.trim())
      errors.push(`端点 ${ep.protocol} 没有密钥，且渠道默认密钥为空`)

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
  saving.value = true
  saveError.value = null

  const parsedHeaders = parseHeaders(headersText.value)
  const payload: Channel = {
    ...form.value,
    tags: tagsText.value
      .split(',')
      .map((t) => t.trim())
      .filter(Boolean),
    param_override: paramOverrideText.value.trim()
      ? (JSON.parse(paramOverrideText.value) as Record<string, unknown>)
      : null,
    extra_headers: typeof parsedHeaders === 'string' ? [] : parsedHeaders,
    proxy: form.value.proxy?.trim() || null,
    note: form.value.note?.trim() || null,
    test_model: form.value.test_model?.trim() || null,
    empty_response_retry: { ...form.value.empty_response_retry },
  }

  try {
    if (isEdit.value) await store.update(payload)
    else await store.create(payload)
    router.push('/channels')
  } catch (e) {
    saveError.value = e instanceof Error ? e.message : '保存失败'
  } finally {
    saving.value = false
  }
}

async function destroy() {
  if (!isEdit.value) return
  try {
    await store.remove(editingId.value as number)
    router.push('/channels')
  } catch (e) {
    saveError.value = e instanceof Error ? e.message : '删除失败'
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

    <div v-if="loading" class="py-16 text-center text-sm text-ink-faint">加载中…</div>

    <form v-else class="flex flex-col gap-4" @submit.prevent="save">
      <!-- 基础信息 -->
      <section class="glass glass-specular p-5">
        <h2 class="mb-4 text-sm font-semibold text-ink-soft uppercase">基础</h2>

        <div class="grid grid-cols-1 gap-4 sm:grid-cols-2">
          <label class="flex flex-col gap-1.5">
            <span class="text-xs font-medium text-ink-soft">名称</span>
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
        <h2 class="mb-1 text-sm font-semibold text-ink-soft uppercase">默认密钥</h2>
        <p class="mb-4 text-xs text-ink-faint">端点未单独配置密钥时继承这里。</p>

        <div class="relative">
          <input
            v-model="form.credential"
            :type="showCredential ? 'text' : 'password'"
            placeholder="sk-..."
            autocomplete="new-password"
            aria-label="默认渠道 API 密钥"
            class="glass-field w-full px-3 py-2 pr-16 font-mono text-sm outline-none"
          />
          <button
            type="button"
            class="absolute top-1/2 right-2 -translate-y-1/2 rounded px-2 py-1 text-xs text-ink-faint hover:text-ink"
            :aria-label="showCredential ? '隐藏默认渠道 API 密钥' : '显示默认渠道 API 密钥'"
            :aria-pressed="showCredential"
            @click="showCredential = !showCredential"
          >
            {{ showCredential ? '隐藏' : '显示' }}
          </button>
        </div>
      </section>

      <!-- 端点 -->
      <section class="glass glass-specular p-5">
        <div class="mb-1 flex items-center justify-between">
          <h2 class="text-sm font-semibold text-ink-soft uppercase">协议端点</h2>
          <button
            v-if="isAggregate && availableProtocols.length > 0"
            type="button"
            class="rounded-lg bg-ink/8 px-3 py-1.5 text-xs font-medium hover:bg-ink/12"
            @click="addEndpoint"
          >
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
            class="rounded-lg border border-ink/10 bg-ink/[0.02] p-4"
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

            <!-- 模型 -->
            <div class="mb-3">
              <div class="mb-1.5 flex items-center justify-between">
                <span class="text-xs font-medium text-ink-soft">
                  模型
                  <span class="font-normal text-ink-faint">
                    支持 <code class="rounded bg-ink/8 px-1">别名=上游名</code> 映射
                  </span>
                </span>
                <button
                  v-if="isEdit"
                  type="button"
                  class="rounded px-2 py-0.5 text-[0.7rem] text-accent hover:bg-accent/10 disabled:opacity-50"
                  :disabled="probing[ep.protocol] || probeStale"
                  :title="
                    probeStale
                      ? '地址或密钥有未保存的修改，探测走的是已保存配置 —— 请先保存'
                      : undefined
                  "
                  @click="probeModels(ep)"
                >
                  {{ probing[ep.protocol] ? '探测中…' : '从上游同步' }}
                </button>
              </div>

              <p v-if="probeStale && isEdit" class="mb-1.5 text-[0.7rem] text-warning">
                地址或密钥有未保存的修改，保存后才能按新配置探测。
              </p>
              <p v-if="probeError[ep.protocol]" class="mb-1.5 text-xs text-danger">
                {{ probeError[ep.protocol] }}
              </p>

              <div v-if="ep.models.length > 0" class="mb-2 flex flex-wrap gap-1.5">
                <span
                  v-for="(m, mi) in ep.models"
                  :key="m.name"
                  class="inline-flex items-center gap-1.5 rounded-lg bg-ink/8 px-2 py-1 font-mono text-xs"
                >
                  {{ m.name
                  }}<span v-if="m.upstream" class="text-ink-faint">→{{ m.upstream }}</span>
                  <button
                    type="button"
                    class="text-ink-faint hover:text-danger"
                    @click="removeModel(ep, mi)"
                  >
                    ×
                  </button>
                </span>
              </div>

              <input
                v-model="modelDraft[ep.protocol]"
                type="text"
                placeholder="gpt-4o 然后回车"
                class="glass-field w-full px-3 py-1.5 font-mono text-xs outline-none"
                @keydown.enter.prevent="addModel(ep)"
              />
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
          {{ saving ? '保存中…' : isEdit ? '保存修改' : '创建渠道' }}
        </button>

        <button
          type="button"
          class="glass-button-ghost px-4 py-2.5 text-sm"
          @click="router.push('/channels')"
        >
          取消
        </button>

        <template v-if="isEdit">
          <button
            v-if="!pendingDelete"
            type="button"
            class="glass-button-ghost glass-button-ghost-danger ml-auto px-4 py-2.5 text-sm !text-ink-faint hover:!text-danger"
            @click="pendingDelete = true"
          >
            删除渠道
          </button>
          <div v-else class="ml-auto flex items-center gap-2">
            <span class="text-xs text-ink-faint">确定删除？</span>
            <button
              type="button"
              class="rounded-full bg-danger px-3.5 py-2 text-sm font-medium text-white hover:brightness-105"
              @click="destroy"
            >
              删除
            </button>
            <button
              type="button"
              class="glass-button-ghost px-3 py-2 text-sm"
              @click="pendingDelete = false"
            >
              取消
            </button>
          </div>
        </template>
      </div>
    </form>
  </div>
</template>
