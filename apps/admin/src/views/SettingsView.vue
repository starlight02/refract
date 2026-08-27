<script setup lang="ts">
/**
 * 网关设置编排：加载/保存、脏检查、分块重试。
 *
 * 各区块 UI 在 `components/settings/`；这里只负责把后端快照和保存闸门串起来。
 */
import { computed, onBeforeUnmount, onMounted, ref } from 'vue'
import { onBeforeRouteLeave } from 'vue-router'
import { useAction } from '@/composables/useAction'
import GlassSpinner from '@/components/GlassSpinner.vue'
import AppIcon from '@/components/AppIcon.vue'
import RoutingPolicySection from '@/components/settings/RoutingPolicySection.vue'
import AffinitySection from '@/components/settings/AffinitySection.vue'
import BreakerSection from '@/components/settings/BreakerSection.vue'
import DataHotBackupSection from '@/components/settings/DataHotBackupSection.vue'
import GlobalLimitsSection from '@/components/settings/GlobalLimitsSection.vue'
import NotifySection from '@/components/settings/NotifySection.vue'
import PricingSection from '@/components/settings/PricingSection.vue'
import LogRetentionSection from '@/components/settings/LogRetentionSection.vue'
import AdminIdentitySection from '@/components/settings/AdminIdentitySection.vue'
import ConfigBackupSection from '@/components/settings/ConfigBackupSection.vue'
import { data as dataApi, settings } from '@/api/client'
import { orElse } from '@/utils/effect'
import { toErrorMessage } from '@/utils/error'
import {
  affinityDraftsValid,
  affinityFromDrafts,
  ruleToDraft,
  type AffinityRuleDraft,
} from '@/utils/affinity-draft'
import {
  backupValid,
  breakerValid,
  emptyResponseRetryValid,
  ipLimitsValid,
  limitsValid,
  notifyValid,
  policyValid,
  pricingValid,
  retentionValid,
} from '@/utils/settings-validation'
import type {
  AffinitySettings,
  BackupSettings,
  BreakerPolicy,
  EmptyResponseRetryPolicy,
  GlobalLimits,
  IpLimits,
  ModelPrice,
  NotifySettings,
  RoutingPolicy,
} from '@refract/contracts'

const saveSettings = useAction('保存失败', { toast: true })
const reloadSectionAction = useAction('加载失败')
const sectionErrors = ref<Record<string, string | null>>({})
const loading = ref(true)
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
const affinity = ref<AffinitySettings>({
  enabled: false,
  switch_on_success: true,
  keep_on_channel_disabled: false,
  max_entries: 100_000,
  default_ttl_secs: 1800,
  rules: [],
})
const affinityRules = ref<AffinityRuleDraft[]>([])
const webhookSecretConfigured = ref(false)
const masterKeyConfigured = ref(false)

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
let affinitySnapshot = ''

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

function affinityCurrent(): AffinitySettings {
  return affinityFromDrafts(affinity.value, affinityRules.value)
}

const affinityDirty = computed(() => affinitySnapshot !== JSON.stringify(affinityCurrent()))

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

const policyOk = computed(() => policyValid(policy.value))
const retentionOk = computed(() => retentionValid(retentionDays.value))
const breakerOk = computed(() => breakerValid(breaker.value))
const pricingOk = computed(() => pricingValid(pricing.value))
const notifyOk = computed(() => notifyValid(notify.value))
const limitsOk = computed(() => limitsValid(limits.value))
const ipLimitsOk = computed(() => ipLimitsValid(ipLimits.value))
const backupOk = computed(() => backupValid(backupCfg.value))
const emptyRetryOk = computed(() => emptyResponseRetryValid(emptyResponseRetry.value))
const affinityOk = computed(() =>
  affinityDraftsValid(affinity.value.enabled, affinity.value, affinityRules.value),
)

const canSave = computed(
  () =>
    isDirty.value &&
    !saveSettings.busy &&
    policyOk.value &&
    retentionOk.value &&
    breakerOk.value &&
    pricingOk.value &&
    notifyOk.value &&
    emptyRetryOk.value &&
    affinityOk.value &&
    limitsOk.value &&
    ipLimitsOk.value &&
    backupOk.value,
)

function applySettled<T>(
  result: PromiseSettledResult<T>,
  section: string,
  apply: (value: T) => void,
) {
  if (result.status === 'fulfilled') apply(result.value)
  else sectionErrors.value[section] = toErrorMessage(result.reason, '加载失败')
}

async function reloadSection(section: string) {
  sectionErrors.value[section] = null
  const ok = await reloadSectionAction.run(async () => {
    switch (section) {
      case 'policy': {
        const p = await settings.routingPolicy()
        policy.value = p
        policySnapshot = JSON.stringify(p)
        break
      }
      case 'retention': {
        const [r, b] = await Promise.all([settings.logRetention(), settings.logBodies()])
        retentionDays.value = r.days
        retentionSnapshot = r.days
        logBodies.value = b.enabled
        logBodiesSnapshot = b.enabled
        break
      }
      case 'breaker': {
        const b = await settings.breakerPolicy()
        breaker.value = b
        breakerSnapshot = JSON.stringify(b)
        break
      }
      case 'pricing': {
        const pr = await settings.pricing()
        pricing.value = pr
        pricingSnapshot = JSON.stringify(pr)
        break
      }
      case 'limits': {
        const [l, ip] = await Promise.all([settings.globalLimits(), settings.ipLimits()])
        limits.value = l
        limitsSnapshot = JSON.stringify(l)
        ipLimits.value = ip
        ipLimitsSnapshot = JSON.stringify(ip)
        break
      }
      case 'notify': {
        const n = await settings.notify()
        notify.value = { ...n, webhook_url: n.webhook_url ?? '' }
        notifySnapshot = JSON.stringify(notify.value)
        break
      }
      case 'emptyRetry': {
        const er = await settings.emptyResponseRetry()
        emptyResponseRetry.value = er
        emptyResponseRetrySnapshot = JSON.stringify(er)
        break
      }
      case 'affinity': {
        const aff = await settings.affinity()
        affinity.value = aff
        affinityRules.value = aff.rules.map(ruleToDraft)
        affinitySnapshot = JSON.stringify(affinityCurrent())
        break
      }
      case 'backup': {
        const b = await settings.backupSettings()
        backupCfg.value = b
        backupSnapshot = JSON.stringify(b)
        break
      }
      case 'webhookSecret': {
        const ws = await settings.webhookSecret()
        webhookSecretConfigured.value = ws.configured
        break
      }
      case 'masterKey': {
        const mk = await settings.masterKey()
        masterKeyConfigured.value = mk.configured
        break
      }
    }
    return true
  })
  if (ok === undefined) sectionErrors.value[section] = reloadSectionAction.error
}

function onBeforeUnload(e: BeforeUnloadEvent) {
  if (isDirty.value && !saveSettings.busy) {
    e.preventDefault()
    e.returnValue = ''
  }
}

onBeforeRouteLeave(() => {
  if (isDirty.value && !saveSettings.busy) {
    const confirm = window.confirm('有未保存的系统设置，确定要离开吗？')
    if (!confirm) return false
  }
})

onMounted(async () => {
  window.addEventListener('beforeunload', onBeforeUnload)
  const [
    pRes,
    retentionRes,
    bRes,
    pricesRes,
    bodiesRes,
    notifyRes,
    limitsRes,
    emptyRetryRes,
    affRes,
    ipLimitsRes,
    backupRes,
    webhookSecretRes,
    masterKeyRes,
  ] = await Promise.allSettled([
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

  applySettled(pRes, 'policy', (p) => {
    policy.value = p
    policySnapshot = JSON.stringify(p)
  })
  applySettled(retentionRes, 'retention', (r) => {
    retentionDays.value = r.days
    retentionSnapshot = r.days
  })
  applySettled(bRes, 'breaker', (b) => {
    breaker.value = b
    breakerSnapshot = JSON.stringify(b)
  })
  applySettled(pricesRes, 'pricing', (pr) => {
    pricing.value = pr
    pricingSnapshot = JSON.stringify(pr)
  })
  if (bodiesRes.status === 'fulfilled') {
    logBodies.value = bodiesRes.value.enabled
    logBodiesSnapshot = bodiesRes.value.enabled
  } else {
    sectionErrors.value.retention ??= toErrorMessage(bodiesRes.reason, '加载失败')
  }
  applySettled(notifyRes, 'notify', (n) => {
    notify.value = { ...n, webhook_url: n.webhook_url ?? '' }
    notifySnapshot = JSON.stringify(notify.value)
  })
  applySettled(limitsRes, 'limits', (l) => {
    limits.value = l
    limitsSnapshot = JSON.stringify(l)
  })
  applySettled(emptyRetryRes, 'emptyRetry', (er) => {
    emptyResponseRetry.value = er
    emptyResponseRetrySnapshot = JSON.stringify(er)
  })
  applySettled(affRes, 'affinity', (aff) => {
    affinity.value = aff
    affinityRules.value = aff.rules.map(ruleToDraft)
    affinitySnapshot = JSON.stringify(affinityCurrent())
  })
  if (ipLimitsRes.status === 'fulfilled') {
    ipLimits.value = ipLimitsRes.value
    ipLimitsSnapshot = JSON.stringify(ipLimitsRes.value)
  } else {
    sectionErrors.value.limits ??= toErrorMessage(ipLimitsRes.reason, '加载失败')
  }
  applySettled(backupRes, 'backup', (b) => {
    backupCfg.value = b
    backupSnapshot = JSON.stringify(b)
  })
  applySettled(webhookSecretRes, 'webhookSecret', (ws) => {
    webhookSecretConfigured.value = ws.configured
  })
  applySettled(masterKeyRes, 'masterKey', (mk) => {
    masterKeyConfigured.value = mk.configured
  })

  const allSettledList = [
    pRes,
    retentionRes,
    bRes,
    pricesRes,
    bodiesRes,
    notifyRes,
    limitsRes,
    emptyRetryRes,
    affRes,
    ipLimitsRes,
    backupRes,
    webhookSecretRes,
    masterKeyRes,
  ]
  if (allSettledList.every((r) => r.status === 'rejected')) {
    loadError.value = '全部设置项加载失败，请检查网络或后端状态'
  }

  const stats = await orElse(() => dataApi.stats())
  if (stats) dbStats.value = stats
  loading.value = false
})

onBeforeUnmount(() => {
  window.removeEventListener('beforeunload', onBeforeUnload)
})

async function save() {
  saveSettings.clear()
  if (!policyOk.value) {
    saveError.value = '路由策略不合法：最大重试 0–32 次（0 = 不限），单请求上游调用上限 0–255'
    return
  }
  if (!retentionOk.value) {
    saveError.value = '日志保留天数必须是 1–3650 的整数'
    return
  }
  if (!breakerOk.value) {
    saveError.value = '熔断参数不合法：阈值 0–1000，冷却 1–86400 秒且上限不小于起始值'
    return
  }
  if (!pricingOk.value) {
    saveError.value = '价表不合法：模式不能为空，价格必须是非负数字'
    return
  }
  if (!notifyOk.value) {
    saveError.value = '通知设置不合法：webhook 需以 http(s):// 开头，重测间隔 0–1440 分钟'
    return
  }
  if (!limitsOk.value) {
    saveError.value = '全局限制不合法：RPM ≤ 1,000,000，并发 ≤ 100,000'
    return
  }
  if (!ipLimitsOk.value) {
    saveError.value = '单 IP 限制不合法：RPM ≤ 1,000,000'
    return
  }
  if (!backupOk.value) {
    saveError.value = '自动备份不合法：间隔 ≤ 8760 小时，保留 1–100 份'
    return
  }
  if (!emptyRetryOk.value) {
    saveError.value = '空回复重试不合法：判定窗口需为 0–3600 秒，最大重试需为 0–100 次'
    return
  }
  if (!affinityOk.value) {
    saveError.value =
      '亲和规则不合法：名称需非空且唯一、每条规则至少一个来源、header 名不能为空、body 路径需以 / 开头、TTL 需为 1–604800 的整数'
    return
  }
  saveError.value = null
  await saveSettings.run(
    async () => {
      const savedSections: string[] = []
      const step = (name: string, dirty: boolean, task: () => Promise<void>): Promise<void> => {
        if (!dirty) return Promise.resolve()
        return task().then(
          () => {
            savedSections.push(name)
          },
          (error: unknown) => {
            const prefix =
              savedSections.length > 0 ? `部分已保存（${savedSections.join('、')}）。` : ''
            throw new Error(`${prefix}${toErrorMessage(error, '保存失败')}`)
          },
        )
      }
      await step('路由策略', policyDirty.value, async () => {
        const p = await settings.setRoutingPolicy(policy.value)
        policy.value = p
        policySnapshot = JSON.stringify(p)
      })
      await step('日志保留', retentionDirty.value, async () => {
        const retention = await settings.setLogRetention(retentionDays.value)
        retentionDays.value = retention.days
        retentionSnapshot = retention.days
      })
      await step('熔断', breakerDirty.value, async () => {
        const b = await settings.setBreakerPolicy(breaker.value)
        breaker.value = b
        breakerSnapshot = JSON.stringify(b)
      })
      await step('价表', pricingDirty.value, async () => {
        const prices = await settings.setPricing(pricing.value)
        pricing.value = prices
        pricingSnapshot = JSON.stringify(prices)
      })
      await step('正文快照', logBodiesDirty.value, async () => {
        const bodies = await settings.setLogBodies(logBodies.value)
        logBodies.value = bodies.enabled
        logBodiesSnapshot = bodies.enabled
      })
      await step('全局限制', limitsDirty.value, async () => {
        const saved_ = await settings.setGlobalLimits(limits.value)
        limits.value = saved_
        limitsSnapshot = JSON.stringify(saved_)
      })
      await step('IP 限制', ipLimitsDirty.value, async () => {
        const saved_ = await settings.setIpLimits(ipLimits.value)
        ipLimits.value = saved_
        ipLimitsSnapshot = JSON.stringify(saved_)
      })
      await step('自动备份', backupDirty.value, async () => {
        const saved_ = await settings.setBackupSettings({
          ...backupCfg.value,
          directory: backupCfg.value.directory?.trim() || null,
        })
        backupCfg.value = saved_
        backupSnapshot = JSON.stringify(saved_)
      })
      await step('空回复重试', emptyResponseRetryDirty.value, async () => {
        const saved_ = await settings.setEmptyResponseRetry(emptyResponseRetry.value)
        emptyResponseRetry.value = saved_
        emptyResponseRetrySnapshot = JSON.stringify(saved_)
      })
      await step('通知', notifyDirty.value, async () => {
        const saved_ = await settings.setNotify({
          webhook_url: notify.value.webhook_url?.trim() || null,
          retest_minutes: notify.value.retest_minutes,
        })
        notify.value = { ...saved_, webhook_url: saved_.webhook_url ?? '' }
        notifySnapshot = JSON.stringify(notify.value)
      })
      await step('亲和性', affinityDirty.value, async () => {
        const saved_ = await settings.setAffinity(affinityCurrent())
        affinity.value = saved_
        affinityRules.value = saved_.rules.map(ruleToDraft)
        affinitySnapshot = JSON.stringify(affinityCurrent())
      })
    },
    () => '已保存',
  )
}
</script>

<template>
  <div class="mx-auto max-w-2xl pb-16">
    <header class="mb-6">
      <h1 class="text-2xl font-semibold">设置</h1>
      <p class="mt-1 text-sm text-ink-faint">变更需要保存才会生效。</p>
    </header>

    <div v-if="loading" class="py-24 text-center">
      <GlassSpinner size="lg" label="正在拉取系统设置与健康状态…" />
    </div>

    <div v-else-if="loadError" class="glass border-danger/30 p-4 text-sm text-danger">
      {{ loadError }}
    </div>

    <template v-else>
      <RoutingPolicySection
        v-model="policy"
        v-model:empty-retry="emptyResponseRetry"
        :load-error="sectionErrors.policy"
        :empty-retry-error="sectionErrors.emptyRetry"
        :valid="policyOk"
        :empty-retry-valid="emptyRetryOk"
        @retry="reloadSection('policy')"
        @retry-empty-retry="reloadSection('emptyRetry')"
      />

      <AffinitySection
        v-model="affinity"
        v-model:rules="affinityRules"
        :load-error="sectionErrors.affinity"
        :valid="affinityOk"
        @retry="reloadSection('affinity')"
      />

      <BreakerSection
        v-model="breaker"
        :load-error="sectionErrors.breaker"
        :valid="breakerOk"
        @retry="reloadSection('breaker')"
      />

      <DataHotBackupSection :db-stats="dbStats" />

      <GlobalLimitsSection
        v-model="limits"
        v-model:ip-limits="ipLimits"
        :load-error="sectionErrors.limits"
        :valid="limitsOk"
        :ip-valid="ipLimitsOk"
        @retry="reloadSection('limits')"
      />

      <NotifySection
        v-model="notify"
        v-model:secret-configured="webhookSecretConfigured"
        :load-error="sectionErrors.notify"
        :secret-load-error="sectionErrors.webhookSecret"
        :valid="notifyOk"
        :dirty="notifyDirty"
        @retry="reloadSection('notify')"
        @retry-secret="reloadSection('webhookSecret')"
      />

      <PricingSection
        v-model="pricing"
        :load-error="sectionErrors.pricing"
        :valid="pricingOk"
        @retry="reloadSection('pricing')"
      />

      <LogRetentionSection
        v-model="retentionDays"
        v-model:log-bodies="logBodies"
        :load-error="sectionErrors.retention"
        :valid="retentionOk"
        @retry="reloadSection('retention')"
      />

      <div class="mt-5 flex items-center gap-3">
        <button
          type="button"
          class="glass-button-primary px-5 py-2.5 text-sm font-medium disabled:opacity-50"
          :disabled="!canSave"
          @click="save"
        >
          <AppIcon v-if="saveSettings.busy" name="spinner" class="animate-spin mr-1.5" :size="15" />
          {{ saveSettings.busy ? '保存中…' : '保存设置' }}
        </button>

        <p v-if="saveSettings.error ?? saveError" class="ml-2 text-sm text-danger">
          {{ saveSettings.error ?? saveError }}
        </p>
      </div>

      <AdminIdentitySection
        :load-error="sectionErrors.masterKey"
        @retry="reloadSection('masterKey')"
      />

      <ConfigBackupSection :load-error="sectionErrors.backup" @retry="reloadSection('backup')" />
    </template>
  </div>
</template>
