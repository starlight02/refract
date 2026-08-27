/**
 * 渠道编辑器客户端校验，与后端 `Channel::validate` 对齐。
 */
import type { Channel, Protocol } from '@refract/contracts'
import * as m from '@/paraglide/messages'
import { hasOwnAddress, poolCredentials } from '@/utils/channel-form'

export function validateChannel(form: Channel, credentialsText: string): string[] {
  const errors: string[] = []

  if (!form.name.trim()) errors.push(m.val_channel_name_required())
  if (form.endpoints.length === 0) errors.push(m.val_channel_endpoint_required())

  const emptyWindow = form.empty_response_retry.window_secs
  if (
    emptyWindow !== null &&
    (!Number.isInteger(emptyWindow) || emptyWindow < 0 || emptyWindow > 3600)
  ) {
    errors.push(m.val_channel_empty_window())
  }
  const emptyRetries = form.empty_response_retry.max_retries
  if (
    emptyRetries !== null &&
    (!Number.isInteger(emptyRetries) || emptyRetries < 0 || emptyRetries > 100)
  ) {
    errors.push(m.val_channel_empty_retries())
  }

  if (form.kind !== 'aggregate') {
    if (form.endpoints.length !== 1) errors.push(m.val_channel_single_ep())
    else if (form.endpoints[0]!.protocol !== form.kind)
      errors.push(m.val_channel_single_ep_proto_match())
  }

  const seen = new Set<Protocol>()
  const hasDefault = poolCredentials(credentialsText).length > 0
  for (const ep of form.endpoints) {
    if (seen.has(ep.protocol)) errors.push(m.val_channel_dup_proto({ proto: ep.protocol }))
    seen.add(ep.protocol)

    if (ep.transcode.accepted.includes(ep.protocol))
      errors.push(m.val_channel_self_transcode({ proto: ep.protocol }))

    const hasOwn = !!ep.credential && ep.credential.trim() !== ''
    if (!hasOwn && !hasDefault) errors.push(m.val_channel_no_key({ proto: ep.protocol }))

    const addr = hasOwnAddress(ep) ? ep.address : form.address
    if (addr.unofficial && !addr.base_url?.trim())
      errors.push(m.val_channel_unofficial_no_base({ proto: ep.protocol }))
  }

  return errors
}
