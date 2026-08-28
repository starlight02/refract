import { ref } from 'vue'
import { defineStore } from 'pinia'
import { type ApiScope, scopedKeys } from '@/api/client'
import type { ApiKey, NewApiKey, CreatedApiKey } from '@refract/contracts'
import { isSuccess, settled } from '@/utils/effect'
import { toErrorMessage, withLoading, withStoreError } from './shared'

/**
 * 网关自身 API 密钥的管理。
 * 注意 create 返回的 plaintext 只在创建那一刻出现一次，
 * 所以创建成功后视图必须立刻展示，之后无法再取回。
 */
export function useKeysStore(scope: ApiScope = 'admin') {
  return defineStore(`keys:${scope}`, () => {
    const api = scopedKeys(scope)
    const items = ref<ApiKey[]>([])
    const loading = ref(false)
    const error = ref<string | null>(null)

    async function fetch() {
      const rows = await withLoading(loading, error, () => api.list())
      if (rows) items.value = rows
    }

    async function create(spec: NewApiKey): Promise<CreatedApiKey> {
      const created = await withStoreError(error, () => api.create(spec))
      items.value.unshift(created.key)
      return created
    }

    async function update(id: number, spec: NewApiKey): Promise<ApiKey> {
      const updated = await withStoreError(error, () => api.update(id, spec))
      const i = items.value.findIndex((k) => k.id === id)
      if (i !== -1) items.value[i] = updated
      return updated
    }

    async function resetUsage(id: number) {
      await withStoreError(error, () => api.resetUsage(id))
      const i = items.value.findIndex((k) => k.id === id)
      if (i !== -1) items.value[i]!.used_quota = 0
    }

    async function remove(id: number) {
      await withStoreError(error, () => api.remove(id))
      items.value = items.value.filter((k) => k.id !== id)
    }

    async function toggleEnabled(id: number, enabled: boolean) {
      error.value = null
      const i = items.value.findIndex((k) => k.id === id)
      const previous = i !== -1 ? items.value[i]!.enabled : null
      if (i !== -1) items.value[i]!.enabled = enabled
      const outcome = await settled(() => api.setEnabled(id, enabled))
      if (isSuccess(outcome)) {
        if (i !== -1) items.value[i]!.enabled = outcome.success.enabled
        return
      }
      if (i !== -1 && previous !== null) items.value[i]!.enabled = previous
      error.value = toErrorMessage(outcome.failure)
    }

    return { items, loading, error, fetch, create, update, resetUsage, remove, toggleEnabled }
  })()
}
