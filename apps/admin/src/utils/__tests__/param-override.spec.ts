import { describe, expect, it } from 'vite-plus/test'
import {
  draftFromOverride,
  emptyOverrideDraft,
  overrideFromDraft,
  parseOverrideValue,
} from '../param-override'

describe('parseOverrideValue', () => {
  it('parses JSON literals', () => {
    expect(parseOverrideValue('0.7')).toEqual({ ok: true, value: 0.7 })
    expect(parseOverrideValue('true')).toEqual({ ok: true, value: true })
    expect(parseOverrideValue('null')).toEqual({ ok: true, value: null })
    expect(parseOverrideValue('"text"')).toEqual({ ok: true, value: 'text' })
    expect(parseOverrideValue('{"topK":40}')).toEqual({ ok: true, value: { topK: 40 } })
  })

  it('rejects empty and invalid JSON', () => {
    expect(parseOverrideValue('')).toMatchObject({ ok: false })
    expect(parseOverrideValue('not-json')).toMatchObject({ ok: false })
  })
})

describe('overrideFromDraft', () => {
  it('returns null for an empty draft', () => {
    expect(overrideFromDraft(emptyOverrideDraft())).toEqual({ value: null, error: null })
  })

  it('builds common and protocol groups', () => {
    const draft = emptyOverrideDraft()
    draft.common = [
      { key: 'temperature', valueText: '0.5' },
      { key: 'logprobs', valueText: 'null' },
    ]
    draft.protocols.gemini = [{ key: 'generationConfig', valueText: '{"topK":40}' }]
    expect(overrideFromDraft(draft)).toEqual({
      value: {
        common: { temperature: 0.5, logprobs: null },
        protocols: { gemini: { generationConfig: { topK: 40 } } },
      },
      error: null,
    })
  })

  it('rejects duplicate keys', () => {
    const draft = emptyOverrideDraft()
    draft.common = [
      { key: 'temperature', valueText: '0.1' },
      { key: 'temperature', valueText: '0.9' },
    ]
    expect(overrideFromDraft(draft).error).toContain('重复')
  })

  it('treats protocol names in common as ordinary fields', () => {
    const draft = emptyOverrideDraft()
    draft.common = [{ key: 'messages', valueText: '[{"role":"user"}]' }]
    expect(overrideFromDraft(draft)).toEqual({
      value: { common: { messages: [{ role: 'user' }] } },
      error: null,
    })
  })
})

describe('draftFromOverride', () => {
  it('round-trips a structured override', () => {
    const draft = draftFromOverride({
      common: { temperature: 0.2 },
      protocols: { chat: { top_p: 0.9 } },
    })
    expect(overrideFromDraft(draft).value).toEqual({
      common: { temperature: 0.2 },
      protocols: { chat: { top_p: 0.9 } },
    })
  })
})
