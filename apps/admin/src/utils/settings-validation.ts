/**
 * 设置页客户端校验，规则与后端 `validate` 对齐。
 *
 * 抽成纯函数是为了让各区块组件和保存闸门共用同一套边界，
 * 也方便单测覆盖「清空 number input 留下空串」这类 Vue 特有输入。
 */
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

/** 亲和 TTL 上限：7 天，与后端一致。 */
export const MAX_AFFINITY_TTL_SECS = 7 * 24 * 3600

export function policyValid(policy: RoutingPolicy): boolean {
  return (
    Number.isInteger(policy.max_attempts) &&
    policy.max_attempts >= 0 &&
    policy.max_attempts <= 32 &&
    Number.isInteger(policy.max_upstream_calls) &&
    policy.max_upstream_calls >= 0 &&
    policy.max_upstream_calls <= 255
  )
}

export function retentionValid(days: number): boolean {
  return Number.isInteger(days) && days >= 1 && days <= 3650
}

export function breakerValid(breaker: BreakerPolicy): boolean {
  return (
    Number.isInteger(breaker.failure_threshold) &&
    breaker.failure_threshold >= 0 &&
    breaker.failure_threshold <= 1000 &&
    Number.isInteger(breaker.base_cooldown_secs) &&
    breaker.base_cooldown_secs >= 1 &&
    breaker.base_cooldown_secs <= 86_400 &&
    Number.isInteger(breaker.max_cooldown_secs) &&
    breaker.max_cooldown_secs >= breaker.base_cooldown_secs &&
    breaker.max_cooldown_secs <= 86_400
  )
}

export function pricingValid(pricing: ModelPrice[]): boolean {
  return pricing.every(
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
  )
}

export function notifyValid(notify: NotifySettings): boolean {
  const url = notify.webhook_url?.trim() ?? ''
  const urlOk = url === '' || url.startsWith('http://') || url.startsWith('https://')
  const minutes = notify.retest_minutes
  return urlOk && Number.isInteger(minutes) && minutes >= 0 && minutes <= 1440
}

export function limitsValid(limits: GlobalLimits): boolean {
  return (
    Number.isInteger(limits.rpm) &&
    limits.rpm >= 0 &&
    limits.rpm <= 1_000_000 &&
    Number.isInteger(limits.tpm) &&
    limits.tpm >= 0 &&
    limits.tpm <= 1_000_000_000 &&
    Number.isInteger(limits.max_concurrency) &&
    limits.max_concurrency >= 0 &&
    limits.max_concurrency <= 100_000
  )
}

export function ipLimitsValid(ipLimits: IpLimits): boolean {
  return Number.isInteger(ipLimits.rpm) && ipLimits.rpm >= 0 && ipLimits.rpm <= 1_000_000
}

export function backupValid(backup: BackupSettings): boolean {
  return (
    Number.isInteger(backup.interval_hours) &&
    backup.interval_hours >= 0 &&
    backup.interval_hours <= 8760 &&
    Number.isInteger(backup.keep) &&
    backup.keep >= 1 &&
    backup.keep <= 100
  )
}

export function emptyResponseRetryValid(policy: EmptyResponseRetryPolicy): boolean {
  return (
    Number.isInteger(policy.window_secs) &&
    policy.window_secs >= 0 &&
    policy.window_secs <= 3600 &&
    Number.isInteger(policy.max_retries) &&
    policy.max_retries >= 0 &&
    policy.max_retries <= 100
  )
}

export function affinitySettingsValid(settings: AffinitySettings): boolean {
  if (!settings.enabled) return true
  if (!Number.isInteger(settings.max_entries) || settings.max_entries < 1) return false
  if (
    !Number.isInteger(settings.default_ttl_secs) ||
    settings.default_ttl_secs < 1 ||
    settings.default_ttl_secs > MAX_AFFINITY_TTL_SECS
  ) {
    return false
  }
  const seen = new Set<string>()
  for (const rule of settings.rules) {
    const name = rule.name.trim()
    if (!name || seen.has(name)) return false
    seen.add(name)
    if (rule.sources.length === 0) return false
    for (const source of rule.sources) {
      if (source.kind === 'header' && !source.name.trim()) return false
      if (source.kind === 'body') {
        const path = source.path.trim()
        if (path !== '' && !path.startsWith('/')) return false
      }
    }
    if (
      rule.ttl_secs != null &&
      (!Number.isInteger(rule.ttl_secs) ||
        rule.ttl_secs < 1 ||
        rule.ttl_secs > MAX_AFFINITY_TTL_SECS)
    ) {
      return false
    }
  }
  return true
}
