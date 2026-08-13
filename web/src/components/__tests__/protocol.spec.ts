import { describe, expect, it } from 'vite-plus/test'

import type { Protocol } from '@/api/types'
import { toggleProtocol, withoutProtocol } from '../protocol'

describe('protocol set wire semantics', () => {
  it('toggles protocol names as a stable JSON array', () => {
    let accepted: Protocol[] = []

    accepted = toggleProtocol(accepted, 'messages')
    accepted = toggleProtocol(accepted, 'chat')

    expect(accepted).toEqual(['chat', 'messages'])
    expect(JSON.stringify(accepted)).toBe('["chat","messages"]')

    accepted = toggleProtocol(accepted, 'chat')
    expect(accepted).toEqual(['messages'])
  })

  it('removes the endpoint native protocol from accepted conversions', () => {
    const accepted: Protocol[] = ['chat', 'responses', 'gemini']

    expect(withoutProtocol(accepted, 'responses')).toEqual(['chat', 'gemini'])
    expect(accepted).toEqual(['chat', 'responses', 'gemini'])
  })
})
