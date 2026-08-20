import { describe, expect, it } from 'vite-plus/test'

import { numOrNull, numOr } from '../num'

describe('numOr', () => {
  it('returns finite numbers unchanged', () => {
    expect(numOr(0, 7)).toBe(0)
    expect(numOr(42, 7)).toBe(42)
    expect(numOr(3.5, 7)).toBe(3.5)
    expect(numOr(-1, 7)).toBe(-1)
  })

  it('falls back on the empty string left by a cleared number input', () => {
    expect(numOr('', 7)).toBe(7)
    expect(numOr('   ', 7)).toBe(7)
  })

  it('parses numeric strings', () => {
    expect(numOr('15', 7)).toBe(15)
    expect(numOr(' 15 ', 7)).toBe(15)
  })

  it('falls back on NaN, Infinity and non-numeric garbage', () => {
    expect(numOr(Number.NaN, 7)).toBe(7)
    expect(numOr(Number.POSITIVE_INFINITY, 7)).toBe(7)
    expect(numOr('abc', 7)).toBe(7)
    expect(numOr(null, 7)).toBe(7)
    expect(numOr(undefined, 7)).toBe(7)
  })
})

describe('numOrNull', () => {
  it('returns finite numbers unchanged', () => {
    expect(numOrNull(0)).toBe(0)
    expect(numOrNull(42)).toBe(42)
    expect(numOrNull(3.5)).toBe(3.5)
  })

  it('normalizes the empty string to null (inherit global semantics)', () => {
    expect(numOrNull('')).toBeNull()
    expect(numOrNull('   ')).toBeNull()
  })

  it('parses numeric strings', () => {
    expect(numOrNull('15')).toBe(15)
  })

  it('normalizes NaN, Infinity and garbage to null', () => {
    expect(numOrNull(Number.NaN)).toBeNull()
    expect(numOrNull(Number.POSITIVE_INFINITY)).toBeNull()
    expect(numOrNull('abc')).toBeNull()
    expect(numOrNull(null)).toBeNull()
    expect(numOrNull(undefined)).toBeNull()
  })
})
