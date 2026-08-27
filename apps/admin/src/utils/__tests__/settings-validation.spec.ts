import { describe, expect, it } from 'vite-plus/test'

import {
  affinitySettingsValid,
  backupValid,
  breakerValid,
  emptyResponseRetryValid,
  ipLimitsValid,
  limitsValid,
  notifyValid,
  policyValid,
  pricingValid,
  retentionValid,
} from '../settings-validation'
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

function policy(over: Partial<RoutingPolicy> = {}): RoutingPolicy {
  return {
    native_first: true,
    selection: 'weighted_random',
    max_attempts: 3,
    retry_same_channel: true,
    max_upstream_calls: 8,
    ...over,
  }
}

describe('policyValid', () => {
  it('accepts the default policy', () => {
    expect(policyValid(policy())).toBe(true)
  })

  it('allows unlimited retries and upstream calls', () => {
    expect(policyValid(policy({ max_attempts: 0, max_upstream_calls: 0 }))).toBe(true)
  })

  it('rejects out of range retries and non-integers', () => {
    expect(policyValid(policy({ max_attempts: 33 }))).toBe(false)
    expect(policyValid(policy({ max_attempts: 1.5 }))).toBe(false)
    expect(policyValid(policy({ max_upstream_calls: 256 }))).toBe(false)
  })
})

describe('retentionValid', () => {
  it('accepts 1–3650 inclusive', () => {
    expect(retentionValid(1)).toBe(true)
    expect(retentionValid(3650)).toBe(true)
  })

  it('rejects 0, 3651 and fractions', () => {
    expect(retentionValid(0)).toBe(false)
    expect(retentionValid(3651)).toBe(false)
    expect(retentionValid(1.2)).toBe(false)
  })
})

describe('breakerValid', () => {
  const ok: BreakerPolicy = {
    failure_threshold: 5,
    base_cooldown_secs: 30,
    max_cooldown_secs: 900,
  }

  it('accepts a legal policy and threshold 0 (disabled)', () => {
    expect(breakerValid(ok)).toBe(true)
    expect(breakerValid({ ...ok, failure_threshold: 0 })).toBe(true)
  })

  it('rejects max cooldown below base', () => {
    expect(breakerValid({ ...ok, max_cooldown_secs: 10 })).toBe(false)
  })
})

describe('pricingValid', () => {
  const row = (over: Partial<ModelPrice> = {}): ModelPrice => ({
    pattern: 'gpt-*',
    input_per_m: 1,
    output_per_m: 2,
    cached_input_per_m: null,
    cache_write_per_m: null,
    ...over,
  })

  it('accepts non-empty patterns and non-negative prices', () => {
    expect(pricingValid([row()])).toBe(true)
    expect(pricingValid([row({ cached_input_per_m: 0.1, cache_write_per_m: 0 })])).toBe(true)
  })

  it('rejects blank patterns and negative prices', () => {
    expect(pricingValid([row({ pattern: '  ' })])).toBe(false)
    expect(pricingValid([row({ input_per_m: -1 })])).toBe(false)
  })
})

describe('notifyValid', () => {
  const n = (over: Partial<NotifySettings> = {}): NotifySettings => ({
    webhook_url: '',
    retest_minutes: 30,
    ...over,
  })

  it('allows empty url and http(s) urls', () => {
    expect(notifyValid(n())).toBe(true)
    expect(notifyValid(n({ webhook_url: 'https://example.com/hook' }))).toBe(true)
  })

  it('rejects non-http urls and out of range minutes', () => {
    expect(notifyValid(n({ webhook_url: 'ftp://x' }))).toBe(false)
    expect(notifyValid(n({ retest_minutes: 1441 }))).toBe(false)
  })
})

describe('limitsValid / ipLimitsValid / backupValid', () => {
  const limits = (over: Partial<GlobalLimits> = {}): GlobalLimits => ({
    rpm: 0,
    tpm: 0,
    max_concurrency: 0,
    ...over,
  })

  it('accepts zeros (unlimited) and rejects over-cap', () => {
    expect(limitsValid(limits())).toBe(true)
    expect(limitsValid(limits({ rpm: 1_000_001 }))).toBe(false)
    expect(ipLimitsValid({ rpm: 0 } satisfies IpLimits)).toBe(true)
    expect(ipLimitsValid({ rpm: 1_000_001 })).toBe(false)
  })

  it('accepts legal backup settings', () => {
    const backup: BackupSettings = { directory: null, interval_hours: 24, keep: 5 }
    expect(backupValid(backup)).toBe(true)
    expect(backupValid({ ...backup, keep: 0 })).toBe(false)
  })
})

describe('emptyResponseRetryValid', () => {
  const p = (over: Partial<EmptyResponseRetryPolicy> = {}): EmptyResponseRetryPolicy => ({
    window_secs: 3,
    max_retries: 5,
    reject_nonstandard_200: false,
    ...over,
  })

  it('accepts zeros (disabled) and rejects over-cap', () => {
    expect(emptyResponseRetryValid(p({ window_secs: 0, max_retries: 0 }))).toBe(true)
    expect(emptyResponseRetryValid(p({ window_secs: 3601 }))).toBe(false)
  })
})

describe('affinitySettingsValid', () => {
  const settings = (over: Partial<AffinitySettings> = {}): AffinitySettings => ({
    enabled: true,
    switch_on_success: true,
    keep_on_channel_disabled: false,
    max_entries: 100,
    default_ttl_secs: 1800,
    rules: [
      {
        name: 'by-key',
        model_regex: '',
        path_regex: '',
        value_regex: '',
        include_model: true,
        skip_retry_on_failure: false,
        sources: [{ kind: 'api_key_id' }],
      },
    ],
    ...over,
  })

  it('skips checks when disabled', () => {
    expect(affinitySettingsValid(settings({ enabled: false, rules: [] }))).toBe(true)
  })

  it('rejects duplicate names, empty sources and bad body paths', () => {
    expect(affinitySettingsValid(settings({ rules: [] }))).toBe(true)
    expect(
      affinitySettingsValid(
        settings({
          rules: [
            {
              name: 'a',
              model_regex: '',
              path_regex: '',
              value_regex: '',
              include_model: true,
              skip_retry_on_failure: false,
              sources: [{ kind: 'body', path: 'user' }],
            },
          ],
        }),
      ),
    ).toBe(false)
  })
})
