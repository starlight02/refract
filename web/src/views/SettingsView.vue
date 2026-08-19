<script setup lang="ts">
/**
 * 网关设置。
 *
 * 两块内容：
 * 1. 路由策略 —— 是否原生优先、选择模式、最大重试次数。
 *    设计成「先调到满意再保存」，而不是「改一行就存一次」。
 * 2. 日志保留 —— 后台定时清理请求日志的周期。
 * 3. 管理令牌 —— 管理 API 的鉴权。启用后所有 /api 请求都要带令牌，
 *    所以设置成功的同时必须把令牌存进本浏览器，否则下一步请求就 401。
 */
import { computed, onMounted, ref } from 'vue'
import GlassSwitch from '@/components/GlassSwitch.vue'
import AppIcon from '@/components/AppIcon.vue'
import {
  backup,
  backups as backupsApi,
  data as dataApi,
  getAdminToken,
  setAdminToken as storeLocalToken,
  settings,
  type ImportResult,
} from '@/api/client'
import type {
  AffinityKeySource,
  AffinityRule,
  AffinitySettings,
  AffinityStatsResponse,
  BackupFile,
  BackupSettings,
  BreakerPolicy,
  EmptyResponseRetryPolicy,
  GlobalLimits,
  IpLimits,
  ModelPrice,
  NotifySettings,
  RoutingPolicy,
  SelectionMode,
} from '@/api/types'

const loading = ref(true)
const saving = ref(false)
const saved = ref(false)
const loadError = ref<string | null>(null)
const saveError = ref<string | null>(null)

const policy = ref<RoutingPolicy>({
  native_first: true,
  selection: 'weighted_random',
  max_attempts: 3,
  retry_same_channel: true,
  max_upstream_calls: 8,
})
const retentionDays = ref(30)
const breaker = ref<BreakerPolicy>({
  failure_threshold: 5,
  base_cooldown_secs: 30,
  max_cooldown_secs: 900,
})
const pricing = ref<ModelPrice[]>([])
const logBodies = ref(true)
const notify = ref<NotifySettings>({ webhook_url: '', retest_minutes: 30 })
const limits = ref<GlobalLimits>({ rpm: 0, tpm: 0, max_concurrency: 0 })
const ipLimits = ref<IpLimits>({ rpm: 0 })
/** 自动备份配置；directory 为 null 时后端用内置默认目录。 */
const backupCfg = ref<BackupSettings>({ directory: null, interval_hours: 0, keep: 5 })
const emptyResponseRetry = ref<EmptyResponseRetryPolicy>({
  window_secs: 3,
  max_retries: 5,
  reject_nonstandard_200: false,
})
const dbStats = ref<{ db_bytes: number; log_rows: number; oldest_log_at: string | null } | null>(
  null,
)

function fmtBytes(bytes: number): string {
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(1)} MB`
  return `${(bytes / 1024 / 1024 / 1024).toFixed(2)} GB`
}

/** 上一次从后端拉到的快照，用于判断有没有改过。 */
let policySnapshot = ''
let retentionSnapshot = 30
let breakerSnapshot = ''
let pricingSnapshot = '[]'
let logBodiesSnapshot = true
let notifySnapshot = ''
let limitsSnapshot = ''
let ipLimitsSnapshot = ''
let backupSnapshot = ''
let emptyResponseRetrySnapshot = ''

const policyDirty = computed(() => policySnapshot !== JSON.stringify(policy.value))
const retentionDirty = computed(() => retentionSnapshot !== retentionDays.value)
const breakerDirty = computed(() => breakerSnapshot !== JSON.stringify(breaker.value))
const pricingDirty = computed(() => pricingSnapshot !== JSON.stringify(pricing.value))
const logBodiesDirty = computed(() => logBodiesSnapshot !== logBodies.value)
const notifyDirty = computed(() => notifySnapshot !== JSON.stringify(notify.value))
const limitsDirty = computed(() => limitsSnapshot !== JSON.stringify(limits.value))
const ipLimitsDirty = computed(() => ipLimitsSnapshot !== JSON.stringify(ipLimits.value))
const backupDirty = computed(() => backupSnapshot !== JSON.stringify(backupCfg.value))
const emptyResponseRetryDirty = computed(
  () => emptyResponseRetrySnapshot !== JSON.stringify(emptyResponseRetry.value),
)
/** 与后端 GlobalLimits::validate 一致：RPM ≤ 1e6，TPM ≤ 1e9，并发 ≤ 1e5。 */
const limitsValid = computed(
  () =>
    Number.isInteger(limits.value.rpm) &&
    limits.value.rpm >= 0 &&
    limits.value.rpm <= 1_000_000 &&
    Number.isInteger(limits.value.tpm) &&
    limits.value.tpm >= 0 &&
    limits.value.tpm <= 1_000_000_000 &&
    Number.isInteger(limits.value.max_concurrency) &&
    limits.value.max_concurrency >= 0 &&
    limits.value.max_concurrency <= 100_000,
)
const ipLimitsValid = computed(
  () =>
    Number.isInteger(ipLimits.value.rpm) &&
    ipLimits.value.rpm >= 0 &&
    ipLimits.value.rpm <= 1_000_000,
)
/** 与后端 BackupSettings::validate 一致：间隔 ≤ 8760 小时，保留 1–100 份。 */
const backupValid = computed(
  () =>
    Number.isInteger(backupCfg.value.interval_hours) &&
    backupCfg.value.interval_hours >= 0 &&
    backupCfg.value.interval_hours <= 8760 &&
    Number.isInteger(backupCfg.value.keep) &&
    backupCfg.value.keep >= 1 &&
    backupCfg.value.keep <= 100,
)
/** 路由策略数值校验：重试 1–32，上游调用上限 0–255（u8）。 */
const policyValid = computed(
  () =>
    Number.isInteger(policy.value.max_attempts) &&
    policy.value.max_attempts >= 1 &&
    policy.value.max_attempts <= 32 &&
    Number.isInteger(policy.value.max_upstream_calls) &&
    policy.value.max_upstream_calls >= 0 &&
    policy.value.max_upstream_calls <= 255,
)
const notifyValid = computed(() => {
  const url = notify.value.webhook_url?.trim() ?? ''
  const urlOk = url === '' || url.startsWith('http://') || url.startsWith('https://')
  const minutes = notify.value.retest_minutes
  return urlOk && Number.isInteger(minutes) && minutes >= 0 && minutes <= 1440
})
const emptyResponseRetryValid = computed(
  () =>
    Number.isInteger(emptyResponseRetry.value.window_secs) &&
    emptyResponseRetry.value.window_secs >= 0 &&
    emptyResponseRetry.value.window_secs <= 3600 &&
    Number.isInteger(emptyResponseRetry.value.max_retries) &&
    emptyResponseRetry.value.max_retries >= 0 &&
    emptyResponseRetry.value.max_retries <= 100,
)
const isDirty = computed(
  () =>
    policyDirty.value ||
    retentionDirty.value ||
    breakerDirty.value ||
    pricingDirty.value ||
    logBodiesDirty.value ||
    notifyDirty.value ||
    limitsDirty.value ||
    ipLimitsDirty.value ||
    backupDirty.value ||
    emptyResponseRetryDirty.value ||
    affinityDirty.value,
)
const retentionValid = computed(
  () =>
    Number.isInteger(retentionDays.value) &&
    retentionDays.value >= 1 &&
    retentionDays.value <= 3650,
)
/** 与后端 BreakerPolicy::validate 一致的客户端校验。 */
const breakerValid = computed(() => {
  const b = breaker.value
  return (
    Number.isInteger(b.failure_threshold) &&
    b.failure_threshold >= 0 &&
    b.failure_threshold <= 1000 &&
    Number.isInteger(b.base_cooldown_secs) &&
    b.base_cooldown_secs >= 1 &&
    b.base_cooldown_secs <= 86_400 &&
    Number.isInteger(b.max_cooldown_secs) &&
    b.max_cooldown_secs >= b.base_cooldown_secs &&
    b.max_cooldown_secs <= 86_400
  )
})
/** 与后端 ModelPrice::validate 一致：pattern 非空，价格为非负有限数。 */
const pricingValid = computed(() =>
  pricing.value.every(
    (row) =>
      row.pattern.trim() !== '' &&
      Number.isFinite(row.input_per_m) &&
      row.input_per_m >= 0 &&
      Number.isFinite(row.output_per_m) &&
      row.output_per_m >= 0 &&
      (row.cached_input_per_m == null ||
        (Number.isFinite(row.cached_input_per_m) && row.cached_input_per_m >= 0)) &&
      (row.cache_write_per_m == null ||
        (Number.isFinite(row.cache_write_per_m) && row.cache_write_per_m >= 0)),
  ),
)

function addPriceRow() {
  pricing.value.push({
    pattern: '',
    input_per_m: 0,
    output_per_m: 0,
    cached_input_per_m: null,
    cache_write_per_m: null,
  })
}

/** number input 清空时 v-model.number 给空串；归一成 null（后端语义：回落输入价）。 */
function normalizePrice(row: ModelPrice, key: 'cached_input_per_m' | 'cache_write_per_m') {
  const value = row[key]
  if (value === undefined || (typeof value === 'string' && value === '') || Number.isNaN(value)) {
    row[key] = null
  }
}

function removePriceRow(index: number) {
  pricing.value.splice(index, 1)
}

// ── 渠道亲和性 ──

/**
 * 亲和规则编辑用的一行来源。把判别联合摊平成「kind + 一个值」，
 * 才能直接 v-model；提交时再按 kind 还原成 AffinityKeySource。
 */
interface SourceRow {
  kind: 'api_key_id' | 'header' | 'body'
  /** header 名或 body JSON Pointer；api_key_id 不使用。 */
  value: string
}

/** 一条规则的编辑态：来源用行编辑。 */
interface AffinityRuleDraft {
  name: string
  model_regex: string
  path_regex: string
  value_regex: string
  ttl_secs: number | null
  include_model: boolean
  skip_retry_on_failure: boolean
  sources: SourceRow[]
}

const affinity = ref<AffinitySettings>({
  enabled: false,
  switch_on_success: true,
  keep_on_channel_disabled: false,
  max_entries: 100_000,
  default_ttl_secs: 1800,
  rules: [],
})
const affinityRules = ref<AffinityRuleDraft[]>([])
const affinityStats = ref<AffinityStatsResponse | null>(null)
const affinityClearing = ref(false)
const affinityNotice = ref<{ tone: 'success' | 'danger'; text: string } | null>(null)

function sourceToRow(source: AffinityKeySource): SourceRow {
  if (source.kind === 'header') return { kind: 'header', value: source.name }
  if (source.kind === 'body') return { kind: 'body', value: source.path }
  return { kind: 'api_key_id', value: '' }
}

function rowToSource(row: SourceRow): AffinityKeySource | null {
  if (row.kind === 'header') {
    const name = row.value.trim()
    return name ? { kind: 'header', name } : null
  }
  if (row.kind === 'body') return { kind: 'body', path: row.value.trim() }
  return { kind: 'api_key_id' }
}

function ruleToDraft(rule: AffinityRule): AffinityRuleDraft {
  return {
    name: rule.name,
    model_regex: rule.model_regex,
    path_regex: rule.path_regex,
    value_regex: rule.value_regex,
    ttl_secs: rule.ttl_secs ?? null,
    include_model: rule.include_model,
    skip_retry_on_failure: rule.skip_retry_on_failure,
    sources: rule.sources.map(sourceToRow),
  }
}

function draftToRule(draft: AffinityRuleDraft): AffinityRule {
  const sources = draft.sources.map(rowToSource).filter((s): s is AffinityKeySource => s !== null)
  return {
    name: draft.name.trim(),
    model_regex: draft.model_regex,
    path_regex: draft.path_regex,
    value_regex: draft.value_regex,
    // 清空/非法输入视为「用全局默认」：归一成 undefined 省略该字段，
    // 与后端 skip-serialize 一致，避免脏检测因 null 与缺失差异而常亮。
    ttl_secs:
      draft.ttl_secs !== null && Number.isInteger(draft.ttl_secs) && draft.ttl_secs >= 1
        ? draft.ttl_secs
        : undefined,
    include_model: draft.include_model,
    skip_retry_on_failure: draft.skip_retry_on_failure,
    sources,
  }
}
/** 与后端 AffinitySettings::validate 一致的客户端校验；关闭时不拦截。 */
const affinityValid = computed(() => {
  if (!affinity.value.enabled) return true
  const a = affinity.value
  if (!Number.isInteger(a.max_entries) || a.max_entries < 1) return false
  if (!Number.isInteger(a.default_ttl_secs) || a.default_ttl_secs < 1) return false
  const seen = new Set<string>()
  for (const draft of affinityRules.value) {
    const name = draft.name.trim()
    if (!name || seen.has(name)) return false
    seen.add(name)
    const sources = draft.sources.map(rowToSource).filter(Boolean)
    if (sources.length === 0) return false
    for (const row of draft.sources) {
      if (row.kind === 'header' && !row.value.trim()) return false
      if (row.kind === 'body') {
        const path = row.value.trim()
        if (path !== '' && !path.startsWith('/')) return false
      }
    }
    if (draft.ttl_secs !== null && (!Number.isInteger(draft.ttl_secs) || draft.ttl_secs < 1))
      return false
  }
  return true
})

let affinitySnapshot = ''

/** 当前编辑态序列化成后端形状，供脏检测与保存共用。 */
function affinityCurrent(): AffinitySettings {
  return {
    ...affinity.value,
    rules: affinityRules.value.map(draftToRule),
  }
}

const affinityDirty = computed(() => affinitySnapshot !== JSON.stringify(affinityCurrent()))

function addAffinityRule() {
  affinityRules.value.push({
    name: `rule-${affinityRules.value.length + 1}`,
    model_regex: '',
    path_regex: '',
    value_regex: '',
    ttl_secs: null,
    include_model: true,
    skip_retry_on_failure: false,
    sources: [{ kind: 'api_key_id', value: '' }],
  })
}

function removeAffinityRule(index: number) {
  affinityRules.value.splice(index, 1)
}

function addSourceRow(draft: AffinityRuleDraft) {
  draft.sources.push({ kind: 'api_key_id', value: '' })
}

function removeSourceRow(draft: AffinityRuleDraft, index: number) {
  draft.sources.splice(index, 1)
}

/** 预设：一键填入常见的亲和规则，省去手填来源。 */
const AFFINITY_PRESETS: { label: string; desc: string; make: () => AffinityRuleDraft }[] = [
  {
    label: '按网关 API 密钥',
    desc: '同一调用方（下游应用）固定命中同一渠道。',
    make: () => ({
      name: 'by-api-key',
      model_regex: '',
      path_regex: '',
      value_regex: '',
      ttl_secs: null,
      include_model: true,
      skip_retry_on_failure: false,
      sources: [{ kind: 'api_key_id', value: '' }],
    }),
  },
  {
    label: '按自定义请求头 X-User-Id',
    desc: '客户端在请求头带会话/用户 ID 时按它绑定。',
    make: () => ({
      name: 'by-header-user',
      model_regex: '',
      path_regex: '',
      value_regex: '',
      ttl_secs: null,
      include_model: true,
      skip_retry_on_failure: false,
      sources: [{ kind: 'header', value: 'X-User-Id' }],
    }),
  },
  {
    label: '按请求体 user 字段',
    desc: '从请求体 JSON 的 user 字段取值绑定。',
    make: () => ({
      name: 'by-body-user',
      model_regex: '',
      path_regex: '',
      value_regex: '',
      ttl_secs: null,
      include_model: true,
      skip_retry_on_failure: false,
      sources: [{ kind: 'body', value: '/user' }],
    }),
  },
]

function applyAffinityPreset(preset: (typeof AFFINITY_PRESETS)[number]) {
  affinityRules.value.push(preset.make())
}

/** 清空已建立的绑定缓存；不影响规则本身。 */
async function clearAffinityBindings() {
  if (affinityClearing.value) return
  affinityClearing.value = true
  affinityNotice.value = null
  try {
    const res = await settings.clearAffinity()
    affinityNotice.value = { tone: 'success', text: `已清除 ${res.cleared} 条绑定。` }
    await refreshAffinityStats()
  } catch (e) {
    affinityNotice.value = { tone: 'danger', text: e instanceof Error ? e.message : '清除失败' }
  } finally {
    affinityClearing.value = false
  }
}

async function refreshAffinityStats() {
  try {
    affinityStats.value = await settings.affinityStats()
  } catch {
    affinityStats.value = null
  }
}

onMounted(async () => {
  try {
    const [
      p,
      retention,
      b,
      prices,
      bodies,
      notifySettings,
      globalLimits,
      emptyRetry,
      aff,
      ipLimitsLoaded,
      backupLoaded,
      webhookSecretLoaded,
      masterKeyLoaded,
    ] = await Promise.all([
      settings.routingPolicy(),
      settings.logRetention(),
      settings.breakerPolicy(),
      settings.pricing(),
      settings.logBodies(),
      settings.notify(),
      settings.globalLimits(),
      settings.emptyResponseRetry(),
      settings.affinity(),
      settings.ipLimits(),
      settings.backupSettings(),
      settings.webhookSecret(),
      settings.masterKey(),
    ])
    policy.value = p
    policySnapshot = JSON.stringify(p)
    retentionDays.value = retention.days
    retentionSnapshot = retention.days
    breaker.value = b
    breakerSnapshot = JSON.stringify(b)
    pricing.value = prices
    pricingSnapshot = JSON.stringify(prices)
    logBodies.value = bodies.enabled
    logBodiesSnapshot = bodies.enabled
    notify.value = { ...notifySettings, webhook_url: notifySettings.webhook_url ?? '' }
    notifySnapshot = JSON.stringify(notify.value)
    limits.value = globalLimits
    limitsSnapshot = JSON.stringify(globalLimits)
    ipLimits.value = ipLimitsLoaded
    ipLimitsSnapshot = JSON.stringify(ipLimitsLoaded)
    backupCfg.value = backupLoaded
    backupSnapshot = JSON.stringify(backupLoaded)
    webhookSecretConfigured.value = webhookSecretLoaded.configured
    masterKeyConfigured.value = masterKeyLoaded.configured
    emptyResponseRetry.value = emptyRetry
    emptyResponseRetrySnapshot = JSON.stringify(emptyRetry)
    affinity.value = aff
    affinityRules.value = aff.rules.map(ruleToDraft)
    affinitySnapshot = JSON.stringify(affinityCurrent())
    refreshAffinityStats().catch(() => {})
    dataApi
      .stats()
      .then((stats) => (dbStats.value = stats))
      .catch(() => {})
  } catch (e) {
    loadError.value = e instanceof Error ? e.message : '加载失败'
  } finally {
    loading.value = false
  }
})

async function save() {
  if (!policyValid.value) {
    saveError.value = '路由策略不合法：最大重试 1–32 次，单请求上游调用上限 0–255'
    return
  }
  if (!retentionValid.value) {
    saveError.value = '日志保留天数必须是 1–3650 的整数'
    return
  }
  if (!breakerValid.value) {
    saveError.value = '熔断参数不合法：阈值 0–1000，冷却 1–86400 秒且上限不小于起始值'
    return
  }
  if (!pricingValid.value) {
    saveError.value = '价表不合法：模式不能为空，价格必须是非负数字'
    return
  }
  if (!notifyValid.value) {
    saveError.value = '通知设置不合法：webhook 需以 http(s):// 开头，重测间隔 0–1440 分钟'
    return
  }
  if (!limitsValid.value) {
    saveError.value = '全局限制不合法：RPM ≤ 1,000,000，并发 ≤ 100,000'
    return
  }
  if (!ipLimitsValid.value) {
    saveError.value = '单 IP 限制不合法：RPM ≤ 1,000,000'
    return
  }
  if (!backupValid.value) {
    saveError.value = '自动备份不合法：间隔 ≤ 8760 小时，保留 1–100 份'
    return
  }
  if (!emptyResponseRetryValid.value) {
    saveError.value = '空回复重试不合法：判定窗口需为 0–3600 秒，最大重试需为 0–100 次'
    return
  }
  if (!affinityValid.value) {
    saveError.value =
      '亲和规则不合法：名称需非空且唯一、每条规则至少一个来源、header 名不能为空、body 路径需以 / 开头、TTL 需为不小于 1 的整数'
    return
  }
  saving.value = true
  saveError.value = null
  saved.value = false
  try {
    if (policyDirty.value) {
      const p = await settings.setRoutingPolicy(policy.value)
      policy.value = p
      policySnapshot = JSON.stringify(p)
    }
    if (retentionDirty.value) {
      const retention = await settings.setLogRetention(retentionDays.value)
      retentionDays.value = retention.days
      retentionSnapshot = retention.days
    }
    if (breakerDirty.value) {
      const b = await settings.setBreakerPolicy(breaker.value)
      breaker.value = b
      breakerSnapshot = JSON.stringify(b)
    }
    if (pricingDirty.value) {
      const prices = await settings.setPricing(pricing.value)
      pricing.value = prices
      pricingSnapshot = JSON.stringify(prices)
    }
    if (logBodiesDirty.value) {
      const bodies = await settings.setLogBodies(logBodies.value)
      logBodies.value = bodies.enabled
      logBodiesSnapshot = bodies.enabled
    }
    if (limitsDirty.value) {
      const saved_ = await settings.setGlobalLimits(limits.value)
      limits.value = saved_
      limitsSnapshot = JSON.stringify(saved_)
    }
    if (ipLimitsDirty.value) {
      const saved_ = await settings.setIpLimits(ipLimits.value)
      ipLimits.value = saved_
      ipLimitsSnapshot = JSON.stringify(saved_)
    }
    if (backupDirty.value) {
      // 目录留空归一成 null：后端语义「用内置默认目录」，且与 GET 回显形状一致。
      const saved_ = await settings.setBackupSettings({
        ...backupCfg.value,
        directory: backupCfg.value.directory?.trim() || null,
      })
      backupCfg.value = saved_
      backupSnapshot = JSON.stringify(saved_)
    }
    if (emptyResponseRetryDirty.value) {
      const saved_ = await settings.setEmptyResponseRetry(emptyResponseRetry.value)
      emptyResponseRetry.value = saved_
      emptyResponseRetrySnapshot = JSON.stringify(saved_)
    }
    if (notifyDirty.value) {
      const saved_ = await settings.setNotify({
        webhook_url: notify.value.webhook_url?.trim() || null,
        retest_minutes: notify.value.retest_minutes,
      })
      notify.value = { ...saved_, webhook_url: saved_.webhook_url ?? '' }
      notifySnapshot = JSON.stringify(notify.value)
    }
    if (affinityDirty.value) {
      const saved_ = await settings.setAffinity(affinityCurrent())
      affinity.value = saved_
      affinityRules.value = saved_.rules.map(ruleToDraft)
      affinitySnapshot = JSON.stringify(affinityCurrent())
      refreshAffinityStats().catch(() => {})
    }
    saved.value = true
  } catch (e) {
    saveError.value = e instanceof Error ? e.message : '保存失败'
  } finally {
    saving.value = false
  }
}

const notifyTesting = ref(false)
const notifyTestResult = ref<string | null>(null)

async function sendTestNotification() {
  notifyTesting.value = true
  notifyTestResult.value = null
  try {
    await settings.testNotify()
    notifyTestResult.value = '已发送 —— 去通知渠道确认收到'
  } catch (e) {
    notifyTestResult.value = e instanceof Error ? e.message : '发送失败'
  } finally {
    notifyTesting.value = false
  }
}

// ── Webhook 签名密钥 ──

/** 服务端只回 configured 标志，不回明文；草稿为空时 PUT null 清除。 */
const webhookSecretConfigured = ref(false)
const webhookSecretDraft = ref('')
const showWebhookSecret = ref(false)
const webhookSecretBusy = ref(false)
const webhookSecretNotice = ref<{ tone: 'success' | 'danger'; text: string } | null>(null)

async function applyWebhookSecret() {
  if (webhookSecretBusy.value) return
  webhookSecretBusy.value = true
  webhookSecretNotice.value = null
  try {
    const secret = webhookSecretDraft.value.trim()
    const res = await settings.setWebhookSecret(secret || null)
    webhookSecretConfigured.value = res.configured
    webhookSecretDraft.value = ''
    webhookSecretNotice.value = {
      tone: 'success',
      text: secret ? '签名密钥已保存，webhook 请求将携带签名头。' : '签名密钥已清除。',
    }
  } catch (e) {
    webhookSecretNotice.value = {
      tone: 'danger',
      text: e instanceof Error ? e.message : '保存失败',
    }
  } finally {
    webhookSecretBusy.value = false
  }
}

// ── 凭据静态加密 ──

const masterKeyConfigured = ref(false)
const masterKeyDraft = ref('')
const showMasterKey = ref(false)
const masterKeyBusy = ref(false)
const masterKeyNotice = ref<{ tone: 'success' | 'danger'; text: string } | null>(null)

/** 启用或更换主密钥；留空保存即清除（之后新凭据回到明文存储）。 */
async function applyMasterKey() {
  if (masterKeyBusy.value) return
  masterKeyBusy.value = true
  masterKeyNotice.value = null
  try {
    const key = masterKeyDraft.value.trim()
    const res = await settings.setMasterKey(key || null)
    masterKeyConfigured.value = res.configured
    masterKeyDraft.value = ''
    masterKeyNotice.value = {
      tone: 'success',
      text: key ? '主密钥已保存。' : '主密钥已清除，新凭据将明文存储。',
    }
  } catch (e) {
    masterKeyNotice.value = {
      tone: 'danger',
      text: e instanceof Error ? e.message : '保存失败',
    }
  } finally {
    masterKeyBusy.value = false
  }
}

// ── 备份文件管理 ──

const backupFiles = ref<BackupFile[]>([])
const backupListLoading = ref(false)
const backupFileBusy = ref(false)
/** 待确认删除的备份文件名 —— 删除不可恢复，沿用渠道列表的二次确认模式。 */
const pendingBackupDelete = ref<string | null>(null)
const backupFileNotice = ref<{ tone: 'success' | 'danger'; text: string } | null>(null)

async function refreshBackupFiles() {
  backupListLoading.value = true
  try {
    backupFiles.value = await backupsApi.list()
  } catch {
    // 列表失败不阻塞整页：备份目录可能尚未创建。
    backupFiles.value = []
  } finally {
    backupListLoading.value = false
  }
}

/** 立即生成一份备份，成功后刷新列表。 */
async function createBackupFile() {
  if (backupFileBusy.value) return
  backupFileBusy.value = true
  backupFileNotice.value = null
  try {
    const res = await backupsApi.create()
    backupFileNotice.value = { tone: 'success', text: `备份已创建：${res.name}` }
    await refreshBackupFiles()
  } catch (e) {
    backupFileNotice.value = { tone: 'danger', text: e instanceof Error ? e.message : '备份失败' }
  } finally {
    backupFileBusy.value = false
  }
}

/** 带管理令牌下载指定备份（裸 <a href> 不带令牌会 401）。 */
async function downloadBackupFile(name: string) {
  backupFileNotice.value = null
  try {
    await backupsApi.download(name)
    backupFileNotice.value = { tone: 'success', text: `已下载：${name}` }
  } catch (e) {
    backupFileNotice.value = { tone: 'danger', text: e instanceof Error ? e.message : '下载失败' }
  }
}

async function deleteBackupFile(name: string) {
  backupFileNotice.value = null
  try {
    await backupsApi.remove(name)
    pendingBackupDelete.value = null
    backupFileNotice.value = { tone: 'success', text: `已删除：${name}` }
    await refreshBackupFiles()
  } catch (e) {
    backupFileNotice.value = { tone: 'danger', text: e instanceof Error ? e.message : '删除失败' }
  }
}

onMounted(() => {
  refreshBackupFiles().catch(() => {})
})

const SELECTION_OPTIONS: { value: SelectionMode; label: string; desc: string }[] = [
  {
    value: 'weighted_random',
    label: '加权随机（推荐）',
    desc: '同优先级内按权重随机选取。适合多渠道流量分配。',
  },
  { value: 'round_robin', label: '轮询', desc: '同优先级内按顺序轮转。适合等量消耗多家余额。' },
  { value: 'first', label: '固定首选', desc: '总是命中同优先级内第一个可用渠道。简单但单点。' },
]

// ── 管理令牌 ──

/** 本浏览器是否保存了令牌 —— 决定提示文案，也方便排查「为什么弹令牌框」。 */
const hasLocalToken = ref(false)
const tokenDraft = ref('')
const showTokenDraft = ref(false)
const tokenBusy = ref(false)
const tokenNotice = ref<{ tone: 'success' | 'danger'; text: string } | null>(null)

onMounted(() => {
  hasLocalToken.value = getAdminToken() !== null
})

/**
 * 启用或更换服务端令牌。
 *
 * 成功后立刻把新令牌写入本地存储：设置令牌的响应本身不需要令牌，但**下一个**
 * 请求就需要了 —— 不存下来的话用户会被自己刚设置的鉴权立刻锁出去。
 */
async function applyToken() {
  const token = tokenDraft.value.trim()
  if (!token || tokenBusy.value) return
  tokenBusy.value = true
  tokenNotice.value = null
  try {
    await settings.setAdminToken(token)
    storeLocalToken(token)
    hasLocalToken.value = true
    tokenDraft.value = ''
    tokenNotice.value = { tone: 'success', text: '令牌已生效，本浏览器已保存。' }
  } catch (e) {
    tokenNotice.value = {
      tone: 'danger',
      text: e instanceof Error ? e.message : '设置失败',
    }
  } finally {
    tokenBusy.value = false
  }
}

/** 关闭管理鉴权：服务端清除令牌哈希，本地也一并清掉。 */
async function clearToken() {
  if (tokenBusy.value) return
  tokenBusy.value = true
  tokenNotice.value = null
  try {
    await settings.setAdminToken(null)
    storeLocalToken(null)
    hasLocalToken.value = false
    tokenNotice.value = { tone: 'success', text: '管理鉴权已关闭。' }
  } catch (e) {
    tokenNotice.value = {
      tone: 'danger',
      text: e instanceof Error ? e.message : '关闭失败',
    }
  } finally {
    tokenBusy.value = false
  }
}

// ── 数据备份 ──

const exportBusy = ref(false)
const importBusy = ref(false)
const importMode = ref<'merge' | 'replace'>('merge')
const backupNotice = ref<{ tone: 'success' | 'danger'; text: string } | null>(null)
const importFileInput = ref<HTMLInputElement | null>(null)
/** 待确认的替换导入：文件已解析但还没提交，等用户二次确认。 */
const pendingReplace = ref<{ name: string; payload: unknown } | null>(null)

/** 导出全量配置并触发浏览器下载。 */
async function exportBackup() {
  if (exportBusy.value) return
  exportBusy.value = true
  backupNotice.value = null
  try {
    const document_ = await backup.export()
    const blob = new Blob([JSON.stringify(document_, null, 2)], { type: 'application/json' })
    const url = URL.createObjectURL(blob)
    const a = document.createElement('a')
    a.href = url
    a.download = `refract-backup-${new Date().toISOString().slice(0, 10)}.json`
    a.click()
    // 下载由浏览器异步启动，立即 revoke 可能抢在它读取 blob 之前。
    window.setTimeout(() => URL.revokeObjectURL(url), 10_000)
    backupNotice.value = {
      tone: 'success',
      text: '备份已下载。文件包含渠道凭据明文，请像保管密钥一样保管它。',
    }
  } catch (e) {
    backupNotice.value = { tone: 'danger', text: e instanceof Error ? e.message : '导出失败' }
  } finally {
    exportBusy.value = false
  }
}

/** 下载 SQLite 在线热备（带鉴权；裸 <a href> 不带管理令牌会 401）。 */
async function downloadDatabaseBackup() {
  backupNotice.value = null
  try {
    await dataApi.backup()
    backupNotice.value = {
      tone: 'success',
      text: '数据库备份已下载，包含全部请求日志，体积较大请妥善保管。',
    }
  } catch (e) {
    backupNotice.value = { tone: 'danger', text: e instanceof Error ? e.message : '备份失败' }
  }
}

/**
 * 读取选中的备份文件。合并模式直接导入；替换模式先清空再导入、
 * 不可恢复，所以解析后停下来等一次显式确认。
 */
async function importBackup(event: Event) {
  const input = event.target as HTMLInputElement
  const file = input.files?.[0]
  // 允许连续选择同一个文件重新导入。
  input.value = ''
  if (!file || importBusy.value) return

  backupNotice.value = null
  pendingReplace.value = null
  let parsed: unknown
  try {
    parsed = JSON.parse(await file.text())
  } catch {
    backupNotice.value = { tone: 'danger', text: '文件不是有效的 JSON' }
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
  importBusy.value = true
  backupNotice.value = null
  try {
    const result: ImportResult = await backup.import(payload, importMode.value)
    const detail = [
      skippedDetail('渠道', result.skipped_channels ?? []),
      skippedDetail('密钥', result.skipped_keys ?? []),
    ]
      .filter(Boolean)
      .join(' ')
    backupNotice.value = {
      tone: 'success',
      text:
        `导入完成：渠道 +${result.channels_imported}（跳过 ${result.channels_skipped}），` +
        `密钥 +${result.keys_imported}（跳过 ${result.keys_skipped}）。` +
        (detail ? ` ${detail}` : ''),
    }
  } catch (e) {
    backupNotice.value = {
      tone: 'danger',
      text: e instanceof Error ? e.message : '导入失败',
    }
  } finally {
    importBusy.value = false
  }
}
</script>

<template>
  <div class="mx-auto max-w-2xl pb-16">
    <header class="mb-6">
      <h1 class="text-2xl font-semibold">设置</h1>
      <p class="mt-1 text-sm text-ink-faint">变更需要保存才会生效。</p>
    </header>

    <div v-if="loading" class="py-16 text-center text-sm text-ink-faint">加载中…</div>

    <div v-else-if="loadError" class="glass border-danger/30 p-4 text-sm text-danger">
      {{ loadError }}
    </div>

    <template v-else>
      <!-- 路由策略 -->
      <section class="glass glass-specular flex flex-col gap-5 p-5">
        <h2 class="text-sm font-semibold text-ink-soft uppercase">路由策略</h2>

        <!-- 原生优先（需求 6） -->
        <label class="flex cursor-pointer items-center gap-3">
          <GlassSwitch v-model="policy.native_first" label="原生优先" />
          <div>
            <span class="text-sm font-medium">原生优先</span>
            <p class="mt-0.5 text-xs text-ink-faint">
              关闭时路由逻辑与 new-api 一致。打开时命中同一模型的原生协议端点始终排在转换端点之前。
            </p>
          </div>
        </label>

        <!-- 选择模式 -->
        <div>
          <span class="mb-2 block text-sm font-medium text-ink-soft">选择模式</span>
          <div class="flex flex-col gap-2">
            <label
              v-for="o in SELECTION_OPTIONS"
              :key="o.value"
              class="flex cursor-pointer items-start gap-3 rounded-lg border border-ink/8 px-4 py-3 transition-colors duration-150"
              :class="
                policy.selection === o.value
                  ? 'border-accent/40 bg-accent/8'
                  : 'hover:bg-ink/[0.03]'
              "
            >
              <input
                v-model="policy.selection"
                type="radio"
                :value="o.value"
                name="selection"
                class="mt-0.5 accent-[var(--color-accent)]"
              />
              <div>
                <p class="text-sm font-medium">{{ o.label }}</p>
                <p class="mt-0.5 text-xs text-ink-faint">{{ o.desc }}</p>
              </div>
            </label>
          </div>
        </div>

        <!-- 最大重试 -->
        <label class="flex flex-col gap-1.5">
          <span class="text-sm font-medium text-ink-soft">
            最大重试次数
            <span class="ml-2 font-normal text-ink-faint">
              建议 2–3。过大会拉长超时，1 禁用重试。
            </span>
          </span>
          <input
            v-model.number="policy.max_attempts"
            type="number"
            min="1"
            max="10"
            class="glass-field tabular w-32 px-3 py-2 text-sm outline-none"
          />
        </label>

        <!-- 单请求上游调用上限 -->
        <label class="flex flex-col gap-1.5">
          <span class="text-sm font-medium text-ink-soft">
            单请求上游调用上限
            <span class="ml-2 font-normal text-ink-faint">
              含重试在内的上游调用总次数，0 = 不限，默认 8。
            </span>
          </span>
          <input
            v-model.number="policy.max_upstream_calls"
            type="number"
            min="0"
            max="255"
            step="1"
            inputmode="numeric"
            aria-label="单请求上游调用次数上限"
            class="glass-field tabular w-32 px-3 py-2 text-sm outline-none"
          />
        </label>
        <p v-if="!policyValid" class="text-xs text-danger" role="alert">
          最大重试 1–32；上游调用上限 0–255。
        </p>

        <!-- 重试同一渠道 -->
        <label class="flex cursor-pointer items-center gap-3">
          <input
            v-model="policy.retry_same_channel"
            type="checkbox"
            class="accent-[var(--color-accent)]"
          />
          <span class="text-sm">
            重试时允许再次命中同一渠道
            <span class="text-xs text-ink-faint"
              >—— 建议关闭，否则 500 可能只是上游临时故障，原渠道未必恢复</span
            >
          </span>
        </label>

        <!-- HTTP 200 空回复重试 -->
        <div class="border-t border-ink/8 pt-4">
          <span class="text-sm font-medium text-ink-soft">上游 200 空回复重试</span>
          <p class="mt-1 text-xs text-ink-faint">
            上游返回 HTTP 200 但没有文本、推理、拒答或工具调用，且“完成时刻 −
            首字节时刻”不超过判定窗口时，在同一渠道重试。任一值为 0 即关闭。
          </p>
          <div class="mt-3 grid max-w-md grid-cols-1 gap-4 sm:grid-cols-2">
            <label class="flex flex-col gap-1.5">
              <span class="text-xs font-medium text-ink-soft">判定窗口（秒）</span>
              <input
                v-model.number="emptyResponseRetry.window_secs"
                type="number"
                min="0"
                max="3600"
                step="1"
                inputmode="numeric"
                class="glass-field tabular px-3 py-2 text-sm outline-none"
              />
            </label>
            <label class="flex flex-col gap-1.5">
              <span class="text-xs font-medium text-ink-soft">最大重试次数</span>
              <input
                v-model.number="emptyResponseRetry.max_retries"
                type="number"
                min="0"
                max="100"
                step="1"
                inputmode="numeric"
                class="glass-field tabular px-3 py-2 text-sm outline-none"
              />
            </label>
          </div>
          <p v-if="!emptyResponseRetryValid" class="mt-2 text-xs text-danger" role="alert">
            判定窗口需为 0–3600 秒，最大重试需为 0–100 次。
          </p>

          <label class="mt-4 flex cursor-pointer items-center gap-3 border-t border-ink/8 pt-4">
            <GlassSwitch
              v-model="emptyResponseRetry.reject_nonstandard_200"
              label="非标准 200 转为 500"
            />
            <div>
              <span class="text-sm font-medium">非标准 200 转为 500</span>
              <p class="mt-0.5 text-xs text-ink-faint">
                开启后，纯文本、HTML 或无法识别的 JSON/SSE 等不符合渠道协议的 HTTP 200
                响应会转换为不可重试的 500，并返回明确错误提示。
              </p>
            </div>
          </label>
        </div>
      </section>

      <!-- 渠道亲和性 -->
      <section class="glass glass-specular mt-5 flex flex-col gap-4 p-5">
        <label class="flex cursor-pointer items-center gap-3">
          <GlassSwitch v-model="affinity.enabled" label="启用渠道亲和性" />
          <div>
            <span class="text-sm font-semibold text-ink-soft uppercase">渠道亲和性</span>
            <p class="mt-1 text-xs text-ink-faint">
              按规则（API 密钥 / 请求头 /
              请求体字段）把调用方绑定到固定渠道，后续请求优先命中同一渠道。
              仅参与路由选择，不影响密钥池与熔断。改动保存后立即生效。
            </p>
          </div>
        </label>

        <template v-if="affinity.enabled">
          <!-- 预设 -->
          <div class="border-t border-ink/8 pt-4">
            <span class="mb-2 block text-sm font-medium text-ink-soft">常用预设</span>
            <div class="flex flex-wrap gap-2">
              <button
                v-for="preset in AFFINITY_PRESETS"
                :key="preset.label"
                type="button"
                class="glass-button-ghost px-3 py-2 text-sm"
                :title="preset.desc"
                @click="applyAffinityPreset(preset)"
              >
                <AppIcon name="sparkles" :size="14" />
                {{ preset.label }}
              </button>
            </div>
          </div>

          <!-- 规则列表 -->
          <div class="border-t border-ink/8 pt-4">
            <div class="mb-3 flex items-center justify-between">
              <span class="text-sm font-medium text-ink-soft">亲和规则</span>
              <button
                type="button"
                class="glass-button-ghost px-3 py-2 text-sm"
                @click="addAffinityRule"
              >
                <AppIcon name="plus" :size="14" />
                添加规则
              </button>
            </div>
            <p v-if="affinityRules.length === 0" class="text-xs text-ink-faint">
              尚未配置规则；启用后无规则时不产生任何绑定。
            </p>

            <div
              v-for="(draft, ri) in affinityRules"
              :key="ri"
              class="mb-4 rounded-xl border border-ink/8 p-4"
            >
              <div class="flex items-start gap-3">
                <label class="flex flex-1 flex-col gap-1.5">
                  <span class="text-xs font-medium text-ink-soft"
                    >规则名（缓存键的一部分，需唯一）</span
                  >
                  <input
                    v-model="draft.name"
                    type="text"
                    class="glass-field px-3 py-2 text-sm outline-none"
                    placeholder="例如 by-api-key"
                  />
                </label>
                <button
                  type="button"
                  class="glass-button-ghost glass-button-ghost-danger shrink-0 px-2 py-2"
                  :aria-label="`删除规则 ${draft.name}`"
                  @click="removeAffinityRule(ri)"
                >
                  <AppIcon name="trash" :size="13" />
                </button>
              </div>

              <div class="mt-3 grid grid-cols-1 gap-3 sm:grid-cols-3">
                <label class="flex flex-col gap-1.5">
                  <span class="text-xs text-ink-soft">模型正则（空 = 全部模型）</span>
                  <input
                    v-model="draft.model_regex"
                    type="text"
                    class="glass-field px-3 py-2 font-mono text-sm outline-none"
                    placeholder="^(gpt|claude)-.*"
                  />
                </label>
                <label class="flex flex-col gap-1.5">
                  <span class="text-xs text-ink-soft">路径正则（空 = 全部路径）</span>
                  <input
                    v-model="draft.path_regex"
                    type="text"
                    class="glass-field px-3 py-2 font-mono text-sm outline-none"
                    placeholder="/v1/chat/completions"
                  />
                </label>
                <label class="flex flex-col gap-1.5">
                  <span class="text-xs text-ink-soft">取值正则（空 = 原样绑定）</span>
                  <input
                    v-model="draft.value_regex"
                    type="text"
                    class="glass-field px-3 py-2 font-mono text-sm outline-none"
                    placeholder="^user-(\d+)"
                  />
                </label>
              </div>

              <!-- 来源列表 -->
              <div class="mt-3">
                <div class="mb-2 flex items-center justify-between">
                  <span class="text-xs font-medium text-ink-soft"
                    >绑定来源（按顺序取第一个命中值）</span
                  >
                  <button
                    type="button"
                    class="glass-button-ghost px-2 py-1 text-xs"
                    @click="addSourceRow(draft)"
                  >
                    <AppIcon name="plus" :size="12" />
                    来源
                  </button>
                </div>
                <div
                  v-for="(row, si) in draft.sources"
                  :key="si"
                  class="mb-2 flex items-center gap-2"
                >
                  <select
                    v-model="row.kind"
                    class="glass-field w-40 px-2 py-2 text-sm outline-none"
                    aria-label="来源类型"
                  >
                    <option value="api_key_id">调用方 API 密钥</option>
                    <option value="header">请求头</option>
                    <option value="body">请求体字段</option>
                  </select>
                  <input
                    v-if="row.kind !== 'api_key_id'"
                    v-model="row.value"
                    type="text"
                    class="glass-field flex-1 px-3 py-2 font-mono text-sm outline-none"
                    :placeholder="
                      row.kind === 'header'
                        ? '请求头名，如 X-User-Id'
                        : 'JSON 指针，如 /metadata/user_id'
                    "
                  />
                  <button
                    v-if="draft.sources.length > 1"
                    type="button"
                    class="glass-button-ghost shrink-0 px-2 py-2"
                    :aria-label="`删除来源 ${si + 1}`"
                    @click="removeSourceRow(draft, si)"
                  >
                    <AppIcon name="x" :size="13" />
                  </button>
                </div>
              </div>

              <div class="mt-3 grid grid-cols-1 gap-3 sm:grid-cols-3">
                <label class="flex flex-col gap-1.5">
                  <span class="text-xs text-ink-soft">TTL（秒，空 = 用全局默认）</span>
                  <input
                    v-model.number="draft.ttl_secs"
                    type="number"
                    min="1"
                    max="604800"
                    class="glass-field tabular px-3 py-2 text-sm outline-none"
                    placeholder="默认"
                  />
                </label>
                <label class="flex cursor-pointer items-center gap-2 self-end pb-2">
                  <input
                    v-model="draft.include_model"
                    type="checkbox"
                    class="accent-[var(--color-accent)]"
                  />
                  <span class="text-xs text-ink-soft">模型参与绑定键（不同模型分开绑定）</span>
                </label>
                <label class="flex cursor-pointer items-center gap-2 self-end pb-2">
                  <input
                    v-model="draft.skip_retry_on_failure"
                    type="checkbox"
                    class="accent-[var(--color-accent)]"
                  />
                  <span class="text-xs text-ink-soft">失败后不切换其他渠道（保持绑定）</span>
                </label>
              </div>
            </div>
          </div>

          <!-- 全局参数 -->
          <div class="border-t border-ink/8 pt-4">
            <span class="mb-2 block text-sm font-medium text-ink-soft">全局参数</span>
            <div class="grid grid-cols-1 gap-3 sm:grid-cols-2">
              <label class="flex flex-col gap-1.5">
                <span class="text-xs text-ink-soft">最大绑定条数（LRU 上限）</span>
                <input
                  v-model.number="affinity.max_entries"
                  type="number"
                  min="1"
                  class="glass-field tabular w-40 px-3 py-2 text-sm outline-none"
                />
              </label>
              <label class="flex flex-col gap-1.5">
                <span class="text-xs text-ink-soft">默认 TTL（秒，1–604800）</span>
                <input
                  v-model.number="affinity.default_ttl_secs"
                  type="number"
                  min="1"
                  max="604800"
                  class="glass-field tabular w-40 px-3 py-2 text-sm outline-none"
                />
              </label>
              <label class="flex cursor-pointer items-center gap-2">
                <input
                  v-model="affinity.switch_on_success"
                  type="checkbox"
                  class="accent-[var(--color-accent)]"
                />
                <span class="text-xs text-ink-soft">绑定渠道成功后更新 TTL（推荐开启）</span>
              </label>
              <label class="flex cursor-pointer items-center gap-2">
                <input
                  v-model="affinity.keep_on_channel_disabled"
                  type="checkbox"
                  class="accent-[var(--color-accent)]"
                />
                <span class="text-xs text-ink-soft">渠道被禁用时保留绑定（否则失效回退重选）</span>
              </label>
            </div>
          </div>

          <!-- 运行状态 -->
          <div class="border-t border-ink/8 pt-4">
            <div class="flex items-center justify-between">
              <span class="text-sm font-medium text-ink-soft">绑定状态</span>
              <button
                type="button"
                class="glass-button-ghost glass-button-ghost-danger px-3 py-2 text-sm disabled:opacity-50"
                :disabled="affinityClearing"
                @click="clearAffinityBindings"
              >
                <AppIcon name="trash" :size="13" />
                {{ affinityClearing ? '清除中…' : '清空绑定' }}
              </button>
            </div>
            <p v-if="affinityStats" class="mt-2 text-xs text-ink-faint">
              当前绑定
              <span class="font-mono text-ink-soft">{{ affinityStats.stats.entries }}</span> 条
              （容量上限 <span class="font-mono text-ink-soft">{{ affinity.max_entries }}</span
              >），命中
              <span class="font-mono text-ink-soft">{{ affinityStats.stats.hits }}</span> / 未命中
              <span class="font-mono text-ink-soft">{{ affinityStats.stats.misses }}</span
              >，淘汰
              <span class="font-mono text-ink-soft">{{ affinityStats.stats.evictions }}</span
              >。
            </p>
            <p
              v-if="affinityNotice"
              class="mt-2 text-xs"
              :class="affinityNotice.tone === 'success' ? 'text-ink-soft' : 'text-danger'"
              role="status"
            >
              {{ affinityNotice.text }}
            </p>
          </div>

          <p v-if="!affinityValid" class="text-xs text-danger" role="alert">
            规则不合法：名称需非空且唯一；每条规则至少一个来源；请求头名不能为空；body 路径需以 /
            开头；TTL 需为 1–604800 的整数。
          </p>
        </template>
      </section>

      <!-- 熔断 -->
      <section class="glass glass-specular mt-5 flex flex-col gap-4 p-5">
        <div>
          <h2 class="text-sm font-semibold text-ink-soft uppercase">熔断</h2>
          <p class="mt-1 text-xs text-ink-faint">
            端点连续失败达到阈值后暂停参与路由，冷却按指数退避直到上限；期间一次成功即恢复。
            阈值设为 0 关闭熔断。改动立即生效，已在冷却中的端点不受影响。
          </p>
        </div>

        <div class="grid max-w-lg grid-cols-1 gap-4 sm:grid-cols-3">
          <label class="flex flex-col gap-1.5">
            <span class="text-sm font-medium text-ink-soft">失败阈值</span>
            <input
              v-model.number="breaker.failure_threshold"
              type="number"
              min="0"
              max="1000"
              step="1"
              inputmode="numeric"
              aria-label="熔断失败阈值"
              class="glass-field tabular px-3 py-2 text-sm outline-none"
            />
            <span class="text-xs text-ink-faint">连续失败次数，0 关闭</span>
          </label>

          <label class="flex flex-col gap-1.5">
            <span class="text-sm font-medium text-ink-soft">起始冷却（秒）</span>
            <input
              v-model.number="breaker.base_cooldown_secs"
              type="number"
              min="1"
              max="86400"
              step="1"
              inputmode="numeric"
              aria-label="熔断起始冷却秒数"
              class="glass-field tabular px-3 py-2 text-sm outline-none"
            />
            <span class="text-xs text-ink-faint">首次熔断的时长</span>
          </label>

          <label class="flex flex-col gap-1.5">
            <span class="text-sm font-medium text-ink-soft">冷却上限（秒）</span>
            <input
              v-model.number="breaker.max_cooldown_secs"
              type="number"
              min="1"
              max="86400"
              step="1"
              inputmode="numeric"
              aria-label="熔断冷却上限秒数"
              class="glass-field tabular px-3 py-2 text-sm outline-none"
            />
            <span class="text-xs text-ink-faint">退避不超过该值</span>
          </label>
        </div>

        <p v-if="!breakerValid" class="text-xs text-danger" role="alert">
          阈值 0–1000；冷却 1–86400 秒，且上限不能小于起始值。
        </p>
      </section>

      <!-- 数据 -->
      <section class="glass glass-specular mt-5 flex flex-col gap-4 p-5">
        <div>
          <h2 class="text-sm font-semibold text-ink-soft uppercase">数据</h2>
          <p class="mt-1 text-xs text-ink-faint">
            SQLite 在线热备（VACUUM INTO，产物紧凑、可直接恢复使用）。
            配置备份只含渠道与密钥，这里是含全部请求日志的完整数据库。
          </p>
        </div>

        <div v-if="dbStats" class="flex flex-wrap gap-x-6 gap-y-1 text-sm text-ink-soft">
          <span>
            体积 <span class="tabular font-medium">{{ fmtBytes(dbStats.db_bytes) }}</span>
          </span>
          <span>
            日志 <span class="tabular font-medium">{{ dbStats.log_rows.toLocaleString() }}</span> 行
          </span>
          <span v-if="dbStats.oldest_log_at">
            最旧 <span class="tabular font-medium">{{ dbStats.oldest_log_at }}</span>
          </span>
        </div>

        <div>
          <button
            type="button"
            class="glass-button-ghost inline-flex items-center gap-1.5 px-3 py-2 text-sm"
            @click="downloadDatabaseBackup"
          >
            <AppIcon name="download" :size="14" />
            下载数据库备份
          </button>
        </div>
      </section>

      <!-- 全局限制 -->
      <section class="glass glass-specular mt-5 flex flex-col gap-4 p-5">
        <div>
          <h2 class="text-sm font-semibold text-ink-soft uppercase">全局限制</h2>
          <p class="mt-1 text-xs text-ink-faint">
            网关级保险丝，对所有请求生效（包括免鉴权模式）。跑飞的本地 agent
            循环不该原样打穿上游账单。0 表示不限。
          </p>
        </div>

        <div class="grid max-w-xl grid-cols-1 gap-4 sm:grid-cols-2">
          <label class="flex flex-col gap-1.5">
            <span class="text-sm font-medium text-ink-soft">全局 RPM</span>
            <input
              v-model.number="limits.rpm"
              type="number"
              min="0"
              max="1000000"
              step="1"
              inputmode="numeric"
              aria-label="全局每分钟请求数上限"
              class="glass-field tabular px-3 py-2 text-sm outline-none"
            />
            <span class="text-xs text-ink-faint">每分钟请求数上限</span>
          </label>

          <label class="flex flex-col gap-1.5">
            <span class="text-sm font-medium text-ink-soft">全局 TPM</span>
            <input
              v-model.number="limits.tpm"
              type="number"
              min="0"
              max="1000000000"
              step="1000"
              inputmode="numeric"
              aria-label="全局每分钟 token 数上限"
              class="glass-field tabular px-3 py-2 text-sm outline-none"
            />
            <span class="text-xs text-ink-faint">
              每分钟 token 数上限。RPM 挡不住「少量请求 × 巨大上下文」
            </span>
          </label>

          <label class="flex flex-col gap-1.5">
            <span class="text-sm font-medium text-ink-soft">并发上限</span>
            <input
              v-model.number="limits.max_concurrency"
              type="number"
              min="0"
              max="100000"
              step="1"
              inputmode="numeric"
              aria-label="全局并发上限"
              class="glass-field tabular px-3 py-2 text-sm outline-none"
            />
            <span class="text-xs text-ink-faint">同时在途请求数（流式占用直到结束）</span>
          </label>

          <label class="flex flex-col gap-1.5">
            <span class="text-sm font-medium text-ink-soft">单 IP RPM</span>
            <input
              v-model.number="ipLimits.rpm"
              type="number"
              min="0"
              max="1000000"
              step="1"
              inputmode="numeric"
              aria-label="单 IP 每分钟请求数上限"
              class="glass-field tabular px-3 py-2 text-sm outline-none"
            />
            <span class="text-xs text-ink-faint">单 IP 每分钟请求上限，0 = 不限</span>
          </label>
        </div>

        <p v-if="!limitsValid" class="text-xs text-danger" role="alert">
          RPM ≤ 1,000,000；TPM ≤ 1,000,000,000；并发 ≤ 100,000。
        </p>
        <p v-if="!ipLimitsValid" class="text-xs text-danger" role="alert">
          单 IP RPM ≤ 1,000,000。
        </p>
      </section>

      <!-- 通知与自愈 -->
      <section class="glass glass-specular mt-5 flex flex-col gap-4 p-5">
        <div>
          <h2 class="text-sm font-semibold text-ink-soft uppercase">通知与自愈</h2>
          <p class="mt-1 text-xs text-ink-faint">
            熔断、恢复、自动禁用事件推送到 webhook（通用 JSON，可对接 Server酱 / 飞书 / Telegram
            网桥）。连续 3 次凭据错误的渠道会被自动停用， 并按设定间隔重测，通过即自动恢复。
          </p>
        </div>

        <label class="flex flex-col gap-1.5">
          <span class="text-sm font-medium text-ink-soft">Webhook 地址</span>
          <div class="flex items-center gap-2">
            <input
              v-model="notify.webhook_url"
              type="url"
              placeholder="https://example.com/hook（留空关闭通知）"
              aria-label="告警 webhook 地址"
              class="glass-field w-full max-w-xl px-3 py-2 font-mono text-sm outline-none"
            />
            <button
              type="button"
              class="glass-button-ghost shrink-0 px-3 py-2 text-sm"
              :disabled="notifyTesting || notifyDirty || !notify.webhook_url"
              :title="notifyDirty ? '先保存设置再测试' : '发送一条测试通知'"
              @click="sendTestNotification"
            >
              {{ notifyTesting ? '发送中…' : '发送测试' }}
            </button>
          </div>
          <span v-if="notifyTestResult" class="text-xs text-ink-soft">
            {{ notifyTestResult }}
          </span>
        </label>

        <!-- Webhook 签名密钥 -->
        <div class="flex max-w-xl flex-col gap-1.5">
          <span class="text-sm font-medium text-ink-soft">
            Webhook 签名密钥
            <span v-if="webhookSecretConfigured" class="ml-2 font-normal text-success">
              已配置（留空保存则清除）
            </span>
            <span v-else class="ml-2 font-normal text-ink-faint">未配置</span>
          </span>
          <div class="relative">
            <input
              v-model="webhookSecretDraft"
              :type="showWebhookSecret ? 'text' : 'password'"
              :placeholder="
                webhookSecretConfigured ? '新签名密钥；留空保存即清除' : '签名密钥（留空不签名）'
              "
              autocomplete="new-password"
              aria-label="Webhook 签名密钥"
              class="glass-field w-full px-3 py-2 pr-16 font-mono text-sm outline-none"
            />
            <button
              type="button"
              class="absolute top-1/2 right-2 -translate-y-1/2 rounded-md px-2 py-1 text-xs text-ink-faint hover:text-ink"
              :aria-label="showWebhookSecret ? '隐藏签名密钥' : '显示签名密钥'"
              :aria-pressed="showWebhookSecret"
              @click="showWebhookSecret = !showWebhookSecret"
            >
              {{ showWebhookSecret ? '隐藏' : '显示' }}
            </button>
          </div>
          <div class="flex flex-wrap items-center gap-2">
            <button
              type="button"
              class="glass-button-ghost px-3 py-1.5 text-xs disabled:opacity-50"
              :disabled="
                webhookSecretBusy || (!webhookSecretConfigured && !webhookSecretDraft.trim())
              "
              @click="applyWebhookSecret"
            >
              {{ webhookSecretBusy ? '保存中…' : '保存签名密钥' }}
            </button>
            <p
              v-if="webhookSecretNotice"
              class="text-xs"
              :class="webhookSecretNotice.tone === 'success' ? 'text-success' : 'text-danger'"
              role="status"
            >
              {{ webhookSecretNotice.text }}
            </p>
          </div>
          <p class="text-xs text-ink-faint">
            配置后 webhook 请求携带 X-Refract-Signature 头（HMAC-SHA256 签名），接收端可验证来源。
          </p>
        </div>

        <label class="flex max-w-sm flex-col gap-1.5">
          <span class="text-sm font-medium text-ink-soft">自动禁用渠道的重测间隔</span>
          <div class="flex items-center gap-2">
            <input
              v-model.number="notify.retest_minutes"
              type="number"
              min="0"
              max="1440"
              step="1"
              inputmode="numeric"
              aria-label="重测间隔分钟数"
              class="glass-field tabular w-32 px-3 py-2 text-sm outline-none"
            />
            <span class="text-sm text-ink-faint">分钟，0 关闭自愈</span>
          </div>
        </label>

        <p v-if="!notifyValid" class="text-xs text-danger" role="alert">
          webhook 需以 http(s):// 开头；重测间隔 0–1440 分钟。
        </p>
      </section>

      <!-- 模型价表 -->
      <section class="glass glass-specular mt-5 flex flex-col gap-4 p-5">
        <div>
          <h2 class="text-sm font-semibold text-ink-soft uppercase">模型价表</h2>
          <p class="mt-1 text-xs text-ink-faint">
            按「每百万 token」计价，币种自定。模式支持精确模型名或以 * 结尾的前缀
            （精确名优先，其后取最长前缀）。缓存读/写价留空按输入价计（不打折）。
            请求日志按落库当时的价表固化成本。
          </p>
        </div>

        <div v-if="pricing.length > 0" class="flex flex-col gap-2">
          <div
            class="grid grid-cols-[1fr_6rem_6rem_6rem_6rem_2.5rem] items-center gap-2 text-xs text-ink-faint"
          >
            <span>模式</span>
            <span class="text-right">输入 / M</span>
            <span class="text-right">输出 / M</span>
            <span class="text-right">缓存读 / M</span>
            <span class="text-right">缓存写 / M</span>
            <span></span>
          </div>
          <div
            v-for="(row, i) in pricing"
            :key="i"
            class="grid grid-cols-[1fr_6rem_6rem_6rem_6rem_2.5rem] items-center gap-2"
          >
            <input
              v-model="row.pattern"
              type="text"
              placeholder="gpt-4o 或 gpt-4o*"
              :aria-label="`价表第 ${i + 1} 行模式`"
              class="glass-field px-3 py-2 font-mono text-xs outline-none"
            />
            <input
              v-model.number="row.input_per_m"
              type="number"
              min="0"
              step="0.01"
              :aria-label="`价表第 ${i + 1} 行输入单价`"
              class="glass-field tabular px-3 py-2 text-right text-xs outline-none"
            />
            <input
              v-model.number="row.output_per_m"
              type="number"
              min="0"
              step="0.01"
              :aria-label="`价表第 ${i + 1} 行输出单价`"
              class="glass-field tabular px-3 py-2 text-right text-xs outline-none"
            />
            <input
              v-model.number="row.cached_input_per_m"
              type="number"
              min="0"
              step="0.01"
              placeholder="=输入"
              :aria-label="`价表第 ${i + 1} 行缓存读单价`"
              class="glass-field tabular px-3 py-2 text-right text-xs outline-none"
              @change="normalizePrice(row, 'cached_input_per_m')"
            />
            <input
              v-model.number="row.cache_write_per_m"
              type="number"
              min="0"
              step="0.01"
              placeholder="=输入"
              :aria-label="`价表第 ${i + 1} 行缓存写单价`"
              class="glass-field tabular px-3 py-2 text-right text-xs outline-none"
              @change="normalizePrice(row, 'cache_write_per_m')"
            />
            <button
              type="button"
              class="glass-button-ghost glass-button-ghost-danger justify-center px-2 py-2"
              :aria-label="`删除价表第 ${i + 1} 行`"
              @click="removePriceRow(i)"
            >
              <AppIcon name="x" :size="13" />
            </button>
          </div>
        </div>

        <div>
          <button type="button" class="glass-button-ghost px-3 py-2 text-sm" @click="addPriceRow">
            <AppIcon name="plus" :size="14" />
            添加规则
          </button>
        </div>

        <p v-if="!pricingValid" class="text-xs text-danger" role="alert">
          模式不能为空，价格必须是非负数字。
        </p>
      </section>

      <section class="glass glass-specular mt-5 flex flex-col gap-4 p-5">
        <div>
          <h2 class="text-sm font-semibold text-ink-soft uppercase">日志保留</h2>
          <p class="mt-1 text-xs text-ink-faint">
            服务启动时清理一次，之后每 24 小时按当前设置删除过期请求日志。
          </p>
        </div>

        <label class="flex cursor-pointer items-center gap-3">
          <GlassSwitch v-model="logBodies" label="记录请求与响应正文" />
          <span>
            <span class="text-sm font-medium">记录请求与响应正文</span>
            <span class="ml-2 text-xs text-ink-faint">
              排障时可在日志里查看完整请求；正文超过 64KB 截断，流式存聚合文本。
            </span>
          </span>
        </label>

        <label class="flex max-w-sm flex-col gap-1.5">
          <span class="text-sm font-medium text-ink-soft">保留天数</span>
          <div class="flex items-center gap-2">
            <input
              v-model.number="retentionDays"
              type="number"
              min="1"
              max="3650"
              step="1"
              inputmode="numeric"
              class="glass-field tabular w-32 px-3 py-2 text-sm outline-none"
              :aria-invalid="!retentionValid"
            />
            <span class="text-sm text-ink-faint">天</span>
          </div>
          <span v-if="!retentionValid" class="text-xs text-danger" role="alert">
            请输入 1–3650 的整数。
          </span>
        </label>
      </section>

      <!-- 操作栏 -->
      <div class="mt-5 flex items-center gap-3">
        <button
          type="button"
          class="glass-button-primary px-5 py-2.5 text-sm font-medium disabled:opacity-50"
          :disabled="
            saving ||
            !isDirty ||
            !retentionValid ||
            !breakerValid ||
            !pricingValid ||
            !notifyValid ||
            !emptyResponseRetryValid ||
            !affinityValid ||
            !limitsValid ||
            !ipLimitsValid ||
            !backupValid ||
            !policyValid
          "
          @click="save"
        >
          {{ saving ? '保存中…' : '保存设置' }}
        </button>

        <Transition
          enter-active-class="transition-opacity duration-300"
          enter-from-class="opacity-0"
          leave-active-class="transition-opacity duration-200"
          leave-to-class="opacity-0"
        >
          <span v-if="saved" class="text-sm text-success">已保存</span>
        </Transition>

        <p v-if="saveError" class="ml-2 text-sm text-danger">{{ saveError }}</p>
      </div>

      <!-- 管理令牌 -->
      <section class="glass glass-specular mt-5 flex flex-col gap-4 p-5">
        <h2 class="text-sm font-semibold text-ink-soft uppercase">管理令牌</h2>
        <p class="text-xs text-ink-faint">
          启用后，管理界面与 /api 的所有请求都需要携带该令牌。服务端只保存哈希， 令牌本身无法读回 ——
          忘记或丢失时只能在此重新设置覆盖。
        </p>

        <p class="text-xs">
          <span v-if="hasLocalToken" class="text-success">本浏览器已保存令牌。</span>
          <span v-else class="text-ink-faint">本浏览器未保存令牌。</span>
        </p>

        <div class="relative">
          <input
            v-model="tokenDraft"
            :type="showTokenDraft ? 'text' : 'password'"
            placeholder="新令牌（启用或更换）"
            autocomplete="new-password"
            aria-label="新管理令牌"
            class="glass-field w-full px-3 py-2 pr-16 font-mono text-sm outline-none"
          />
          <button
            type="button"
            class="absolute top-1/2 right-2 -translate-y-1/2 rounded-md px-2 py-1 text-xs text-ink-faint hover:text-ink"
            :aria-label="showTokenDraft ? '隐藏管理令牌' : '显示管理令牌'"
            :aria-pressed="showTokenDraft"
            @click="showTokenDraft = !showTokenDraft"
          >
            {{ showTokenDraft ? '隐藏' : '显示' }}
          </button>
        </div>

        <div class="flex flex-wrap items-center gap-3">
          <button
            type="button"
            class="glass-button-primary px-4 py-2 text-sm font-medium disabled:opacity-50"
            :disabled="tokenBusy || !tokenDraft.trim()"
            @click="applyToken"
          >
            {{ tokenBusy ? '处理中…' : '启用或更换' }}
          </button>
          <button
            type="button"
            class="glass-button-ghost glass-button-ghost-danger px-4 py-2 text-sm"
            :disabled="tokenBusy"
            @click="clearToken"
          >
            关闭管理鉴权
          </button>
          <p
            v-if="tokenNotice"
            class="text-xs"
            :class="tokenNotice.tone === 'success' ? 'text-success' : 'text-danger'"
          >
            {{ tokenNotice.text }}
          </p>
        </div>
      </section>

      <!-- 数据备份 -->
      <section class="glass glass-specular mt-5 flex flex-col gap-4 p-5">
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
            :disabled="exportBusy"
            @click="exportBackup"
          >
            <AppIcon name="download" :size="15" />
            {{ exportBusy ? '导出中…' : '导出备份' }}
          </button>

          <button
            type="button"
            class="glass-button-ghost px-4 py-2 text-sm font-medium"
            :disabled="importBusy"
            @click="importFileInput?.click()"
          >
            <AppIcon name="upload" :size="15" />
            {{ importBusy ? '导入中…' : '导入备份' }}
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
              class="rounded-full bg-danger px-3.5 py-1.5 text-xs font-medium text-white hover:brightness-105 disabled:opacity-50"
              :disabled="importBusy"
              @click="confirmReplaceImport"
            >
              确认替换
            </button>
            <button
              type="button"
              class="glass-button-ghost px-3 py-1.5 text-xs"
              @click="pendingReplace = null"
            >
              取消
            </button>
          </div>
        </div>

        <p
          v-if="backupNotice"
          class="text-xs"
          :class="backupNotice.tone === 'success' ? 'text-success' : 'text-danger'"
          role="status"
        >
          {{ backupNotice.text }}
        </p>
      </section>
    </template>
  </div>
</template>
