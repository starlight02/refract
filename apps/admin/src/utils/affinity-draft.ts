/**
 * 设置页亲和规则的行编辑草稿。
 *
 * 判别联合摊平成 `kind + value` 才能直接 v-model；提交时再还原成
 * `AffinityKeySource`。TTL 空/非法视为「用全局默认」。
 */
import type { AffinityKeySource, AffinityRule, AffinitySettings } from '@refract/contracts'
import { MAX_AFFINITY_TTL_SECS } from '@/utils/settings-validation'

export interface SourceRow {
  kind: 'api_key_id' | 'header' | 'body'
  /** header 名或 body JSON Pointer；api_key_id 不使用。 */
  value: string
}

export interface AffinityRuleDraft {
  name: string
  model_regex: string
  path_regex: string
  value_regex: string
  ttl_secs: number | null
  include_model: boolean
  skip_retry_on_failure: boolean
  sources: SourceRow[]
}

export function sourceToRow(source: AffinityKeySource): SourceRow {
  if (source.kind === 'header') return { kind: 'header', value: source.name }
  if (source.kind === 'body') return { kind: 'body', value: source.path }
  return { kind: 'api_key_id', value: '' }
}

export function rowToSource(row: SourceRow): AffinityKeySource | null {
  if (row.kind === 'header') {
    const name = row.value.trim()
    return name ? { kind: 'header', name } : null
  }
  if (row.kind === 'body') return { kind: 'body', path: row.value.trim() }
  return { kind: 'api_key_id' }
}

export function ruleToDraft(rule: AffinityRule): AffinityRuleDraft {
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

export function draftToRule(draft: AffinityRuleDraft): AffinityRule {
  const sources = draft.sources.map(rowToSource).filter((s): s is AffinityKeySource => s !== null)
  return {
    name: draft.name.trim(),
    model_regex: draft.model_regex,
    path_regex: draft.path_regex,
    value_regex: draft.value_regex,
    ttl_secs:
      draft.ttl_secs !== null && Number.isInteger(draft.ttl_secs) && draft.ttl_secs >= 1
        ? draft.ttl_secs
        : undefined,
    include_model: draft.include_model,
    skip_retry_on_failure: draft.skip_retry_on_failure,
    sources,
  }
}

export function affinityFromDrafts(
  settings: AffinitySettings,
  rules: AffinityRuleDraft[],
): AffinitySettings {
  return {
    ...settings,
    rules: rules.map(draftToRule),
  }
}

export function affinityDraftsValid(
  enabled: boolean,
  settings: AffinitySettings,
  rules: AffinityRuleDraft[],
): boolean {
  if (!enabled) return true
  if (!Number.isInteger(settings.max_entries) || settings.max_entries < 1) return false
  if (
    !Number.isInteger(settings.default_ttl_secs) ||
    settings.default_ttl_secs < 1 ||
    settings.default_ttl_secs > MAX_AFFINITY_TTL_SECS
  ) {
    return false
  }
  const seen = new Set<string>()
  for (const draft of rules) {
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
    if (
      draft.ttl_secs !== null &&
      (!Number.isInteger(draft.ttl_secs) ||
        draft.ttl_secs < 1 ||
        draft.ttl_secs > MAX_AFFINITY_TTL_SECS)
    ) {
      return false
    }
  }
  return true
}

export function emptyAffinityRule(index: number): AffinityRuleDraft {
  return {
    name: `rule-${index}`,
    model_regex: '',
    path_regex: '',
    value_regex: '',
    ttl_secs: null,
    include_model: true,
    skip_retry_on_failure: false,
    sources: [{ kind: 'api_key_id', value: '' }],
  }
}

/** 预设：一键填入常见的亲和规则，省去手填来源。 */
export const AFFINITY_PRESETS: { label: string; desc: string; make: () => AffinityRuleDraft }[] = [
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
