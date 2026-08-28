import { ref, computed } from 'vue'
import { defineStore } from 'pinia'
import { type ApiScope, scopedChannels } from '@/api/client'
import type {
  Channel,
  Protocol,
  ProbeResult,
  ChannelTestResult,
  UpstreamAddress,
} from '@refract/contracts'
import { isSuccess, settled } from '@/utils/effect'
import { toErrorMessage, withLoading, withStoreError } from './shared'

/**
 * 渠道列表及其全部增删改查动作。
 * 约定：查询类动作（fetch/toggleEnabled）吞掉错误写入 error，不抛出；
 * 写类动作（create/update/remove/probe/test）写入 error 后**继续抛出**，
 * 让调用方（表单、弹窗）能拿到具体失败原因做内联提示。
 */
export function useChannelsStore(scope: ApiScope = 'admin') {
  return defineStore(`channels:${scope}`, () => {
    const api = scopedChannels(scope)
    const items = ref<Channel[]>([])
    const loading = ref(false)
    const error = ref<string | null>(null)

    /** id → 渠道。日志页按 channel_id 反查渠道名时比遍历数组快。 */
    const byId = computed(() => new Map(items.value.map((ch) => [ch.id, ch])))
    /** 轻量选项，给下拉框用，避免把整条 Channel 泄漏给纯展示组件。 */
    const options = computed(() => items.value.map((ch) => ({ id: ch.id, name: ch.name })))

    async function fetch() {
      const rows = await withLoading(loading, error, () => api.list())
      if (rows) items.value = rows
    }

    async function create(ch: Channel): Promise<Channel> {
      const created = await withStoreError(error, () => api.create(ch))
      items.value.push(created)
      return created
    }

    async function update(ch: Channel): Promise<Channel> {
      const updated = await withStoreError(error, () => api.update(ch))
      const i = items.value.findIndex((item) => item.id === updated.id)
      if (i !== -1) items.value[i] = updated
      return updated
    }

    async function remove(id: number) {
      await withStoreError(error, () => api.remove(id))
      items.value = items.value.filter((ch) => ch.id !== id)
    }

    async function toggleEnabled(id: number, enabled: boolean) {
      error.value = null
      const i = items.value.findIndex((ch) => ch.id === id)
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

    function probe(id: number, protocol?: Protocol): Promise<ProbeResult> {
      return withStoreError(error, () => api.probe(id, protocol))
    }

    function probeDirect(spec: {
      protocol: Protocol
      address?: UpstreamAddress
      credential?: string | null
      proxy?: string | null
    }): Promise<ProbeResult> {
      return withStoreError(error, () => api.probeDirect(spec))
    }

    function test(id: number, protocol?: Protocol, model?: string): Promise<ChannelTestResult> {
      return withStoreError(error, () => api.test(id, protocol, model))
    }

    async function duplicate(id: number): Promise<Channel> {
      const copy = await withStoreError(error, () => api.duplicate(id))
      items.value.push(copy)
      return copy
    }

    async function bulk(ids: number[], action: 'enable' | 'disable' | 'delete'): Promise<number> {
      const { affected } = await withStoreError(error, () => api.bulk(ids, action))
      await fetch()
      return affected
    }

    return {
      items,
      loading,
      error,
      byId,
      options,
      fetch,
      create,
      update,
      remove,
      toggleEnabled,
      probe,
      probeDirect,
      test,
      duplicate,
      bulk,
    }
  })()
}
