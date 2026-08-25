import { describe, expect, it } from 'vite-plus/test'
import { isFailure, isSuccess, orElse, parseJson, settled } from '../effect'

describe('parseJson', () => {
  it('parses objects', () => {
    expect(parseJson<{ a: number }>('{"a":1}')).toEqual({ a: 1 })
  })

  it('returns undefined for invalid JSON', () => {
    expect(parseJson('not-json')).toBeUndefined()
  })

  it('returns undefined for a truncated payload rather than throwing', () => {
    expect(parseJson('{"a":')).toBeUndefined()
  })
})

describe('orElse', () => {
  it('returns the resolved value', async () => {
    await expect(orElse(async () => 7)).resolves.toBe(7)
  })

  it('falls back to the supplied value on rejection', async () => {
    await expect(
      orElse(async () => {
        throw new Error('nope')
      }, [] as number[]),
    ).resolves.toEqual([])
  })

  it('falls back to undefined when no fallback is given', async () => {
    await expect(
      orElse(async () => {
        throw new Error('nope')
      }),
    ).resolves.toBeUndefined()
  })
})

describe('settled', () => {
  it('returns a success Result', async () => {
    const outcome = await settled(async () => 3)
    expect(isSuccess(outcome)).toBe(true)
    if (!isSuccess(outcome)) throw new Error('expected success')
    expect(outcome.success).toBe(3)
  })

  it('preserves the original rejection value', async () => {
    const error = new Error('nope')
    const outcome = await settled(async () => {
      throw error
    })
    expect(isFailure(outcome)).toBe(true)
    if (!isFailure(outcome)) throw new Error('expected failure')
    expect(outcome.failure).toBe(error)
  })
})
