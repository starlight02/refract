import { describe, expect, it } from 'vite-plus/test'
import { settle, tryParseJson } from '../async'

describe('settle', () => {
  it('returns the resolved value', async () => {
    await expect(settle(Promise.resolve(7))).resolves.toBe(7)
  })

  it('swallows rejection as undefined', async () => {
    await expect(settle(Promise.reject(new Error('nope')))).resolves.toBeUndefined()
  })
})

describe('tryParseJson', () => {
  it('parses objects', () => {
    expect(tryParseJson<{ a: number }>('{"a":1}')).toEqual({ a: 1 })
  })

  it('returns undefined for invalid JSON', () => {
    expect(tryParseJson('not-json')).toBeUndefined()
  })
})
