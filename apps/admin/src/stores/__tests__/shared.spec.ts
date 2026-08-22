import { describe, expect, it } from 'vite-plus/test'
import { ref } from 'vue'
import { withLoading, withStoreError } from '../shared'

describe('withStoreError', () => {
  it('clears error and returns the value', async () => {
    const error = ref<string | null>('stale')
    const value = await withStoreError(error, async () => 42)
    expect(value).toBe(42)
    expect(error.value).toBeNull()
  })

  it('records and rethrows by default', async () => {
    const error = ref<string | null>(null)
    await expect(
      withStoreError(error, async () => {
        throw new Error('boom')
      }),
    ).rejects.toThrow('boom')
    expect(error.value).toBe('boom')
  })

  it('can swallow and return undefined', async () => {
    const error = ref<string | null>(null)
    const value = await withStoreError(
      error,
      async () => {
        throw new Error('soft')
      },
      { rethrow: false },
    )
    expect(value).toBeUndefined()
    expect(error.value).toBe('soft')
  })
})

describe('withLoading', () => {
  it('toggles loading even when the task fails', async () => {
    const loading = ref(false)
    const error = ref<string | null>(null)
    const value = await withLoading(loading, error, async () => {
      expect(loading.value).toBe(true)
      throw new Error('load failed')
    })
    expect(value).toBeUndefined()
    expect(loading.value).toBe(false)
    expect(error.value).toBe('load failed')
  })
})
