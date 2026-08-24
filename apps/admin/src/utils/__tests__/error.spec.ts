import { describe, expect, it } from 'vite-plus/test'
import { readErrorEnvelope, toErrorMessage } from '../error'

describe('toErrorMessage', () => {
  it('uses Error.message when present', () => {
    expect(toErrorMessage(new Error('boom'))).toBe('boom')
  })

  it('appends ApiError detail when it adds information', () => {
    const error = Object.assign(new Error('no available channel'), {
      detail: 'all chat endpoints are in cooldown',
    })
    expect(toErrorMessage(error)).toBe('no available channel（all chat endpoints are in cooldown）')
  })

  it('does not duplicate detail already in the message', () => {
    const error = Object.assign(new Error('failed: timeout'), { detail: 'timeout' })
    expect(toErrorMessage(error)).toBe('failed: timeout')
  })

  it('falls back for empty or non-Error values', () => {
    expect(toErrorMessage(new Error(''))).toBe('发生未知错误')
    expect(toErrorMessage('nope', '加载失败')).toBe('加载失败')
    expect(toErrorMessage(null, '加载失败')).toBe('加载失败')
  })
})

describe('readErrorEnvelope', () => {
  it('reads the admin envelope', () => {
    expect(
      readErrorEnvelope(
        JSON.stringify({
          code: 'no_available_channel',
          message: 'no chat-protocol channel provides this model',
          detail: 'tried 3 endpoints',
        }),
        503,
        'Service Unavailable',
      ),
    ).toEqual({
      code: 'no_available_channel',
      message: 'no chat-protocol channel provides this model',
      detail: 'tried 3 endpoints',
    })
  })

  it('reads a nested protocol envelope', () => {
    expect(
      readErrorEnvelope(
        JSON.stringify({ error: { message: 'upstream says no', type: 'upstream_error' } }),
        502,
        'Bad Gateway',
      ),
    ).toEqual({
      code: 'upstream_error',
      message: 'upstream says no',
    })
  })

  it('falls back to the status line for HTML or empty bodies', () => {
    expect(readErrorEnvelope('<html>nope</html>', 502, 'Bad Gateway')).toEqual({
      code: 'http_error',
      message: '502 Bad Gateway',
    })
  })
})
