/**
 * 渠道参数覆盖的行编辑草稿。
 *
 * 值一律按 JSON 解析：`0.7` / `true` / `"text"` / `null` / `{"topK":40}`。
 * `null` 是删除语义。协议作用域走显式 `protocols`，不再靠键名猜。
 */
import { PROTOCOL_IDS, type ParamOverride, type Protocol } from '@refract/contracts'
import { parseJson } from '@/utils/effect'

export interface OverrideRow {
  key: string
  valueText: string
}

export interface OverrideDraft {
  common: OverrideRow[]
  protocols: Record<Protocol, OverrideRow[]>
}

export function emptyOverrideRow(): OverrideRow {
  return { key: '', valueText: '' }
}

export function emptyOverrideDraft(): OverrideDraft {
  return {
    common: [emptyOverrideRow()],
    protocols: Object.fromEntries(PROTOCOL_IDS.map((id) => [id, [emptyOverrideRow()]])) as Record<
      Protocol,
      OverrideRow[]
    >,
  }
}

export function formatOverrideValue(value: unknown): string {
  return JSON.stringify(value)
}

export function parseOverrideValue(
  text: string,
): { ok: true; value: unknown } | { ok: false; error: string } {
  const trimmed = text.trim()
  if (!trimmed) return { ok: false, error: '值不能为空；删除字段请填 null' }
  const parsed = parseJson(trimmed)
  if (parsed === undefined) return { ok: false, error: '值必须是合法 JSON（字符串请加引号）' }
  return { ok: true, value: parsed }
}

function rowsFromRecord(record: Record<string, unknown> | undefined): OverrideRow[] {
  const rows = Object.entries(record ?? {}).map(([key, value]) => ({
    key,
    valueText: formatOverrideValue(value),
  }))
  return rows.length > 0 ? rows : [emptyOverrideRow()]
}

export function draftFromOverride(value: ParamOverride | null | undefined): OverrideDraft {
  const draft = emptyOverrideDraft()
  draft.common = rowsFromRecord(value?.common)
  for (const protocol of PROTOCOL_IDS) {
    draft.protocols[protocol] = rowsFromRecord(value?.protocols?.[protocol])
  }
  return draft
}

function recordFromRows(rows: OverrideRow[]): {
  record: Record<string, unknown>
  error: string | null
} {
  const record: Record<string, unknown> = {}
  const seen = new Set<string>()
  for (const row of rows) {
    const key = row.key.trim()
    const valueText = row.valueText.trim()
    if (!key && !valueText) continue
    if (!key) return { record, error: '字段名不能为空' }
    if (seen.has(key)) return { record, error: `字段名重复：${key}` }
    const parsed = parseOverrideValue(row.valueText)
    if (!parsed.ok) return { record, error: `${key}：${parsed.error}` }
    seen.add(key)
    record[key] = parsed.value
  }
  return { record, error: null }
}

export function overrideFromDraft(draft: OverrideDraft): {
  value: ParamOverride | null
  error: string | null
} {
  const common = recordFromRows(draft.common)
  if (common.error) return { value: null, error: common.error }
  const protocols: Partial<Record<Protocol, Record<string, unknown>>> = {}
  for (const protocol of PROTOCOL_IDS) {
    const group = recordFromRows(draft.protocols[protocol])
    if (group.error) return { value: null, error: `${protocol}：${group.error}` }
    if (Object.keys(group.record).length > 0) protocols[protocol] = group.record
  }
  const hasCommon = Object.keys(common.record).length > 0
  const hasProtocols = Object.keys(protocols).length > 0
  if (!hasCommon && !hasProtocols) return { value: null, error: null }
  return {
    value: {
      ...(hasCommon ? { common: common.record } : {}),
      ...(hasProtocols ? { protocols } : {}),
    },
    error: null,
  }
}

export function overrideDraftError(draft: OverrideDraft): string | null {
  return overrideFromDraft(draft).error
}
