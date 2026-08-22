import { ref, computed } from 'vue'
import { defineStore } from 'pinia'
import { channels } from '@/api/client'
import type {
  Channel,
  Protocol,
  ProbeResult,
  ChannelTestResult,
  UpstreamAddress,
} from '@refract/contracts'
import { toErrorMessage, withLoading, withStoreError } from './shared'

/**
 * 渠道列表及其全部增删改查动作。
 * 约定：查询类动作（fetch/toggleEnabled）吞掉错误写入 error，不抛出；
 * 写类动作（create/update/remove/probe/test）写入 error 后**继续抛出**，
 * 让调用方（表单、弹窗）能拿到具体失败原因做内联提示。
 */
export const useChannelsStore = defineStore('channels', () => {
  const items = ref<Channel[]>([])
  const loading = ref(false)
  const error = ref<string | null>(null)

  /** id → 渠道。日志页按 channel_id 反查渠道名时比遍历数组快。 */
  const byId = computed(() => new Map(items.value.map((ch) => [ch.id, ch])))
  /** 轻量选项，给下拉框用，避免把整条 Channel 泄漏给纯展示组件。 */
  const options = computed(() => items.value.map((ch) => ({ id: ch.id, name: ch.name })))

  async function fetch() {
    const rows = await withLoading(loading, error, () => channels.list())
    if (rows) items.value = rows
  }

  async function create(ch: Channel): Promise<Channel> {
    const created = await withStoreError(error, () => channels.create(ch))
    // 新建后直接并入列表：列表页不必再整页刷新，编辑页也能立即跳回。
    items.value.push(created)
    return created
  }

  async function update(ch: Channel): Promise<Channel> {
    const updated = await withStoreError(error, () => channels.update(ch))
    const i = items.value.findIndex((item) => item.id === updated.id)
    if (i !== -1) items.value[i] = updated
    return updated
  }

  async function remove(id: number) {
    await withStoreError(error, () => channels.remove(id))
    items.value = items.value.filter((ch) => ch.id !== id)
  }

  /**
   * 开关渠道。先改本地（体验上的即时反馈），失败回滚并写入 error。
   * 不抛出 —— 模板里直接绑定调用，抛出会变成未处理的 rejection。
   */
  async function toggleEnabled(id: number, enabled: boolean) {
    error.value = null
    const i = items.value.findIndex((ch) => ch.id === id)
    const previous = i !== -1 ? items.value[i]!.enabled : null
    if (i !== -1) items.value[i]!.enabled = enabled
    try {
      const res = await channels.setEnabled(id, enabled)
      if (i !== -1) items.value[i]!.enabled = res.enabled
    } catch (e) {
      if (i !== -1 && previous !== null) items.value[i]!.enabled = previous
      error.value = toErrorMessage(e)
    }
  }

  /** 探测上游真实模型列表。返回结果让调用方决定是否一键同步。 */
  function probe(id: number, protocol?: Protocol): Promise<ProbeResult> {
    return withStoreError(error, () => channels.probe(id, protocol))
  }

  /** 在未保存时直接按草稿参数探测上游真实模型列表。 */
  function probeDirect(spec: {
    protocol: Protocol
    address?: UpstreamAddress
    credential?: string | null
    proxy?: string | null
  }): Promise<ProbeResult> {
    return withStoreError(error, () => channels.probeDirect(spec))
  }

  /** 发最小真实请求验证渠道连通性。 */
  function test(id: number, protocol?: Protocol, model?: string): Promise<ChannelTestResult> {
    return withStoreError(error, () => channels.test(id, protocol, model))
  }

  /** 复制渠道。副本以禁用状态并入列表，等用户改完再启用。 */
  async function duplicate(id: number): Promise<Channel> {
    const copy = await withStoreError(error, () => channels.duplicate(id))
    items.value.push(copy)
    return copy
  }

  /** 批量启用/禁用/删除。完成后整表重拉 —— 批量操作后的部分成功状态太多，重拉最可靠。 */
  async function bulk(ids: number[], action: 'enable' | 'disable' | 'delete'): Promise<number> {
    const { affected } = await withStoreError(error, () => channels.bulk(ids, action))
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
})
