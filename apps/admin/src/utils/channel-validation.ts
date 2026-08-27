/**
 * 渠道编辑器客户端校验，与后端 `Channel::validate` 对齐。
 */
import type { Channel, Protocol } from '@refract/contracts'
import { hasOwnAddress, poolCredentials } from '@/utils/channel-form'

export function validateChannel(form: Channel, credentialsText: string): string[] {
  const errors: string[] = []

  if (!form.name.trim()) errors.push('渠道名不能为空')
  if (form.endpoints.length === 0) errors.push('至少需要一个协议端点')

  const emptyWindow = form.empty_response_retry.window_secs
  if (
    emptyWindow !== null &&
    (!Number.isInteger(emptyWindow) || emptyWindow < 0 || emptyWindow > 3600)
  ) {
    errors.push('空回复判定窗口必须留空，或填写 0–3600 的整数')
  }
  const emptyRetries = form.empty_response_retry.max_retries
  if (
    emptyRetries !== null &&
    (!Number.isInteger(emptyRetries) || emptyRetries < 0 || emptyRetries > 100)
  ) {
    errors.push('空回复最大重试必须留空，或填写 0–100 的整数')
  }

  if (form.kind !== 'aggregate') {
    if (form.endpoints.length !== 1) errors.push('单协议渠道必须恰好一个端点')
    else if (form.endpoints[0]!.protocol !== form.kind)
      errors.push('单协议渠道的端点协议必须与渠道类型一致')
  }

  const seen = new Set<Protocol>()
  const hasDefault = poolCredentials(credentialsText).length > 0
  for (const ep of form.endpoints) {
    if (seen.has(ep.protocol)) errors.push(`协议 ${ep.protocol} 出现了多个端点`)
    seen.add(ep.protocol)

    if (ep.transcode.accepted.includes(ep.protocol))
      errors.push(`端点 ${ep.protocol} 不能把自己的原生协议列为转换目标`)

    const hasOwn = !!ep.credential && ep.credential.trim() !== ''
    if (!hasOwn && !hasDefault) errors.push(`端点 ${ep.protocol} 没有密钥，且渠道默认密钥为空`)

    const addr = hasOwnAddress(ep) ? ep.address : form.address
    if (addr.unofficial && !addr.base_url?.trim())
      errors.push(`端点 ${ep.protocol} 开启了非官方地址但没填 base URL`)
  }

  return errors
}
