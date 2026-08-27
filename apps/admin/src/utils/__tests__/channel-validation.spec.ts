import { describe, expect, it } from 'vite-plus/test'

import { blankChannel, newEndpoint } from '../channel-form'
import { validateChannel } from '../channel-validation'

describe('validateChannel', () => {
  it('requires a name and a key', () => {
    const channel = blankChannel()
    expect(validateChannel(channel, '')).toEqual([
      '渠道名不能为空',
      '端点 chat 没有密钥，且渠道默认密钥为空',
    ])
  })

  it('accepts a named chat channel with a pool key', () => {
    const channel = blankChannel()
    channel.name = '主力'
    expect(validateChannel(channel, 'sk-1')).toEqual([])
  })

  it('rejects a native protocol listed as a transcode target', () => {
    const channel = blankChannel()
    channel.name = 'x'
    channel.endpoints[0]!.transcode.accepted = ['chat']
    expect(validateChannel(channel, 'sk-1')).toContain('端点 chat 不能把自己的原生协议列为转换目标')
  })

  it('requires unofficial endpoints to have a base URL', () => {
    const channel = blankChannel()
    channel.name = 'x'
    channel.endpoints[0]!.address.unofficial = true
    expect(validateChannel(channel, 'sk-1')).toContain('端点 chat 开启了非官方地址但没填 base URL')
  })

  it('rejects duplicate protocols on aggregate channels', () => {
    const channel = blankChannel()
    channel.name = 'x'
    channel.kind = 'aggregate'
    channel.endpoints = [newEndpoint('chat'), newEndpoint('chat', 1)]
    expect(validateChannel(channel, 'sk-1')).toContain('协议 chat 出现了多个端点')
  })
})
