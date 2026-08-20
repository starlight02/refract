import { describe, expect, it, beforeEach } from 'vite-plus/test'
import { setActivePinia, createPinia } from 'pinia'
import { useToastStore } from '../toast'

describe('toast store', () => {
  beforeEach(() => {
    setActivePinia(createPinia())
  })

  it('adds and dismisses toasts', () => {
    const store = useToastStore()
    expect(store.items).toHaveLength(0)

    const id1 = store.success('Saved successfully')
    expect(store.items).toHaveLength(1)
    expect(store.items[0]!.text).toBe('Saved successfully')
    expect(store.items[0]!.tone).toBe('success')

    const id2 = store.danger('An error occurred', 'Detail error message')
    expect(store.items).toHaveLength(2)
    expect(store.items[1]!.text).toBe('An error occurred')
    expect(store.items[1]!.description).toBe('Detail error message')
    expect(store.items[1]!.tone).toBe('danger')

    store.dismiss(id1)
    expect(store.items).toHaveLength(1)
    expect(store.items[0]!.id).toBe(id2)

    store.dismiss(id2)
    expect(store.items).toHaveLength(0)
  })

  it('clears all toasts and timers', () => {
    const store = useToastStore()
    store.info('Info 1')
    store.warning('Warning 1')
    expect(store.items).toHaveLength(2)

    store.clear()
    expect(store.items).toHaveLength(0)
  })
})
