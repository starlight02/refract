import { ref } from 'vue'
import { defineStore } from 'pinia'
import { keys } from '@/api/client'
import type { ApiKey, NewApiKey, CreatedApiKey } from '@/api/types'
import { toErrorMessage } from './shared'

/**
 * 网关自身 API 密钥的管理。
 * 注意 create 返回的 plaintext 只在创建那一刻出现一次，
 * 所以创建成功后视图必须立刻展示，之后无法再取回。
 */
export const useKeysStore = defineStore('keys', () => {
  const items = ref<ApiKey[]>([])
  const loading = ref(false)
  const error = ref<string | null>(null)

  async function fetch() {
    loading.value = true
    error.value = null
    try {
      items.value = await keys.list()
    } catch (e) {
      error.value = toErrorMessage(e)
    } finally {
      loading.value = false
    }
  }

  /** 创建密钥。返回 CreatedApiKey 给弹窗展示一次性明文；失败抛出以便弹窗保持打开。 */
  async function create(spec: NewApiKey): Promise<CreatedApiKey> {
    error.value = null
    try {
      const created = await keys.create(spec)
      items.value.unshift(created.key)
      return created
    } catch (e) {
      error.value = toErrorMessage(e)
      throw e
    }
  }

  async function remove(id: number) {
    error.value = null
    try {
      await keys.remove(id)
      items.value = items.value.filter((k) => k.id !== id)
    } catch (e) {
      error.value = toErrorMessage(e)
      throw e
    }
  }

  /** 开关密钥，同样先本地后回滚。 */
  async function toggleEnabled(id: number, enabled: boolean) {
    error.value = null
    const i = items.value.findIndex((k) => k.id === id)
    const previous = i !== -1 ? items.value[i]!.enabled : null
    if (i !== -1) items.value[i]!.enabled = enabled
    try {
      const res = await keys.setEnabled(id, enabled)
      if (i !== -1) items.value[i]!.enabled = res.enabled
    } catch (e) {
      if (i !== -1 && previous !== null) items.value[i]!.enabled = previous
      error.value = toErrorMessage(e)
      throw e
    }
  }

  return { items, loading, error, fetch, create, remove, toggleEnabled }
})
