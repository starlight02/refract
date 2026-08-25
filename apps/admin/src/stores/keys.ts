import { ref } from 'vue'
import { defineStore } from 'pinia'
import { keys } from '@/api/client'
import type { ApiKey, NewApiKey, CreatedApiKey } from '@refract/contracts'
import { isSuccess, settled } from '@/utils/effect'
import { toErrorMessage, withLoading, withStoreError } from './shared'

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
    const rows = await withLoading(loading, error, () => keys.list())
    if (rows) items.value = rows
  }

  /** 创建密钥。返回 CreatedApiKey 给弹窗展示一次性明文；失败抛出以便弹窗保持打开。 */
  async function create(spec: NewApiKey): Promise<CreatedApiKey> {
    const created = await withStoreError(error, () => keys.create(spec))
    items.value.unshift(created.key)
    return created
  }

  /** 编辑治理属性；密钥本体不变。失败抛出以便弹窗保持打开。 */
  async function update(id: number, spec: NewApiKey): Promise<ApiKey> {
    const updated = await withStoreError(error, () => keys.update(id, spec))
    const i = items.value.findIndex((k) => k.id === id)
    if (i !== -1) items.value[i] = updated
    return updated
  }

  /** 已用配额清零。 */
  async function resetUsage(id: number) {
    await withStoreError(error, () => keys.resetUsage(id))
    const i = items.value.findIndex((k) => k.id === id)
    if (i !== -1) items.value[i]!.used_quota = 0
  }

  async function remove(id: number) {
    await withStoreError(error, () => keys.remove(id))
    items.value = items.value.filter((k) => k.id !== id)
  }

  /**
   * 开关密钥，先本地后回滚。
   * 不抛出 —— 模板直接绑定，与渠道开关同一套约定。
   */
  async function toggleEnabled(id: number, enabled: boolean) {
    error.value = null
    const i = items.value.findIndex((k) => k.id === id)
    const previous = i !== -1 ? items.value[i]!.enabled : null
    if (i !== -1) items.value[i]!.enabled = enabled
    const outcome = await settled(() => keys.setEnabled(id, enabled))
    if (isSuccess(outcome)) {
      if (i !== -1) items.value[i]!.enabled = outcome.success.enabled
      return
    }
    if (i !== -1 && previous !== null) items.value[i]!.enabled = previous
    error.value = toErrorMessage(outcome.failure)
  }

  return { items, loading, error, fetch, create, update, resetUsage, remove, toggleEnabled }
})
