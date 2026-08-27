import { describe, expect, it } from 'vite-plus/test'

import {
  blankChannel,
  hasOwnAddress,
  joinSegments,
  looksMasked,
  newEndpoint,
  parseAndAddModels,
  poolCredentials,
  previewUrl,
} from '../channel-form'

describe('blankChannel', () => {
  it('seeds a chat endpoint so the model input renders', () => {
    const channel = blankChannel()
    expect(channel.kind).toBe('chat')
    expect(channel.endpoints).toHaveLength(1)
    expect(channel.endpoints[0]?.protocol).toBe('chat')
  })
})

describe('looksMasked / poolCredentials', () => {
  it('treats ellipsis and bullets as masks', () => {
    expect(looksMasked('sk-a…9f2c')).toBe(true)
    expect(looksMasked('••••')).toBe(true)
    expect(looksMasked('sk-live-real')).toBe(false)
    expect(looksMasked('')).toBe(false)
  })

  it('splits the key pool on lines and drops blanks', () => {
    expect(poolCredentials('sk-1\n\n sk-2 \n')).toEqual(['sk-1', 'sk-2'])
  })
})

describe('parseAndAddModels', () => {
  it('accepts comma lists and alias=upstream mappings', () => {
    const ep = newEndpoint('chat')
    parseAndAddModels(ep, 'gpt-4o, gpt-4o-mini=upstream-mini')
    expect(ep.models).toEqual([
      { name: 'gpt-4o', upstream: null },
      { name: 'gpt-4o-mini', upstream: 'upstream-mini' },
    ])
  })

  it('skips duplicates', () => {
    const ep = newEndpoint('chat')
    parseAndAddModels(ep, 'gpt-4o')
    parseAndAddModels(ep, 'gpt-4o')
    expect(ep.models).toHaveLength(1)
  })
})

describe('previewUrl', () => {
  it('uses official defaults when unofficial is off', () => {
    const ep = newEndpoint('chat')
    expect(previewUrl(ep, blankChannel().address)).toBe(
      'https://api.openai.com/v1/chat/completions',
    )
  })

  it('joins unofficial segments like the backend', () => {
    expect(joinSegments('https://api.example.com/', ['/v1/', 'chat'])).toBe(
      'https://api.example.com/v1/chat',
    )
  })

  it('detects own address overrides', () => {
    const ep = newEndpoint('chat')
    expect(hasOwnAddress(ep)).toBe(false)
    ep.address.unofficial = true
    expect(hasOwnAddress(ep)).toBe(true)
  })
})
