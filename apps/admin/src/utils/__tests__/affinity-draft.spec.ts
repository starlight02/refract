import { describe, expect, it } from 'vite-plus/test'

import {
  affinityDraftsValid,
  affinityFromDrafts,
  draftToRule,
  emptyAffinityRule,
  rowToSource,
  ruleToDraft,
  sourceToRow,
} from '../affinity-draft'
import type { AffinitySettings } from '@refract/contracts'

describe('affinity draft round-trip', () => {
  it('converts header/body/api_key sources', () => {
    expect(sourceToRow({ kind: 'api_key_id' })).toEqual({ kind: 'api_key_id', value: '' })
    expect(rowToSource({ kind: 'header', value: ' X-User-Id ' })).toEqual({
      kind: 'header',
      name: 'X-User-Id',
    })
    expect(rowToSource({ kind: 'header', value: '  ' })).toBeNull()
    expect(rowToSource({ kind: 'body', value: '/user' })).toEqual({ kind: 'body', path: '/user' })
  })

  it('omits invalid TTL so dirty detection matches skip-serialize', () => {
    const draft = emptyAffinityRule(1)
    draft.ttl_secs = 0
    expect(draftToRule(draft).ttl_secs).toBeUndefined()
    draft.ttl_secs = 60
    expect(draftToRule(draft).ttl_secs).toBe(60)
  })

  it('rebuilds settings from drafts', () => {
    const settings: AffinitySettings = {
      enabled: true,
      switch_on_success: true,
      keep_on_channel_disabled: false,
      max_entries: 10,
      default_ttl_secs: 30,
      rules: [],
    }
    const draft = emptyAffinityRule(1)
    draft.name = 'by-key'
    const next = affinityFromDrafts(settings, [draft])
    expect(next.rules).toHaveLength(1)
    expect(ruleToDraft(next.rules[0]!).name).toBe('by-key')
    expect(affinityDraftsValid(true, next, [draft])).toBe(true)
  })
})
