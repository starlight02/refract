import { beforeEach, describe, expect, it } from 'vite-plus/test'
import { createPinia, setActivePinia } from 'pinia'
import { useAction } from '../useAction'
import { useToastStore } from '@/stores/toast'

describe('useAction', () => {
  beforeEach(() => {
    setActivePinia(createPinia())
  })

  it('flags busy while running and clears it on success', async () => {
    const action = useAction('保存失败')
    let observed: boolean | undefined
    const pending = action.run(async () => {
      observed = action.busy
      return 42
    })
    await expect(pending).resolves.toBe(42)
    expect(observed).toBe(true)
    expect(action.busy).toBe(false)
    expect(action.notice).toBeNull()
  })

  it('folds a rejection into a danger notice instead of throwing', async () => {
    const action = useAction('保存失败')
    const value = await action.run(async () => {
      throw new Error('上游拒绝')
    })
    expect(value).toBeUndefined()
    expect(action.notice).toEqual({ tone: 'danger', text: '上游拒绝' })
    expect(action.error).toBe('上游拒绝')
    expect(action.busy).toBe(false)
  })

  it('uses the fallback when the rejection carries no message', async () => {
    const action = useAction('保存失败')
    await action.run(async () => {
      throw 'bare string'
    })
    expect(action.error).toBe('保存失败')
  })

  it('reports the string returned by the success handler', async () => {
    const action = useAction('保存失败')
    await action.run(
      async () => ({ configured: true }),
      (res) => (res.configured ? '密钥已保存。' : '密钥已清除。'),
    )
    expect(action.notice).toEqual({ tone: 'success', text: '密钥已保存。' })
    expect(action.error).toBeNull()
  })

  it('stays silent when the success handler returns nothing', async () => {
    const action = useAction('加载失败')
    await action.run(
      async () => 1,
      () => undefined,
    )
    expect(action.notice).toBeNull()
  })

  it('clears a stale notice when the next run starts', async () => {
    const action = useAction('失败')
    await action.run(async () => {
      throw new Error('第一次失败')
    })
    expect(action.error).toBe('第一次失败')
    await action.run(async () => 'ok')
    expect(action.notice).toBeNull()
  })

  it('mirrors notices into toasts only when asked', async () => {
    const toasts = useToastStore()
    const quiet = useAction('失败')
    await quiet.run(async () => {
      throw new Error('安静失败')
    })
    expect(toasts.items).toHaveLength(0)

    const loud = useAction('失败', { toast: true })
    await loud.run(async () => {
      throw new Error('响亮失败')
    })
    expect(toasts.items).toHaveLength(1)
    expect(toasts.items[0]!.tone).toBe('danger')
    expect(toasts.items[0]!.text).toBe('响亮失败')
  })

  it('treats cancellation as neither success nor failure', async () => {
    const action = useAction('请求失败')
    const pending = action.run(
      (signal) =>
        new Promise<string>((resolve, reject) => {
          const timer = setTimeout(() => resolve('late'), 1_000)
          signal.addEventListener('abort', () => {
            clearTimeout(timer)
            reject(new DOMException('Aborted', 'AbortError'))
          })
        }),
    )
    action.cancel()
    await expect(pending).resolves.toBeUndefined()
    expect(action.notice).toBeNull()
    expect(action.busy).toBe(false)
  })

  it('exposes fail for validation errors that never hit the network', () => {
    const action = useAction('保存失败')
    action.fail('名称不能为空')
    expect(action.error).toBe('名称不能为空')
    action.clear()
    expect(action.notice).toBeNull()
  })
})
