import { describe, expect, it } from 'vite-plus/test'
import { toErrorMessage } from '../error'

describe('toErrorMessage', () => {
  it('uses Error.message when present', () => {
    expect(toErrorMessage(new Error('boom'))).toBe('boom')
  })

  it('falls back for empty or non-Error values', () => {
    expect(toErrorMessage(new Error(''))).toBe('发生未知错误')
    expect(toErrorMessage('nope', '加载失败')).toBe('加载失败')
    expect(toErrorMessage(null, '加载失败')).toBe('加载失败')
  })
})
