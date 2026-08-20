/**
 * Protocol identifiers shared by the management UI and the public site.
 *
 * Keep the wire values here: they are part of the Rust API contract, not a
 * presentation concern of either frontend.
 */
export const PROTOCOLS = [
  {
    id: 'chat',
    name: 'Chat',
    vendor: 'OpenAI',
    path: '/v1/chat/completions',
  },
  {
    id: 'responses',
    name: 'Responses',
    vendor: 'OpenAI',
    path: '/v1/responses',
  },
  {
    id: 'messages',
    name: 'Messages',
    vendor: 'Anthropic',
    path: '/v1/messages',
  },
  {
    id: 'gemini',
    name: 'Gemini',
    vendor: 'Google',
    path: '/v1beta/models/{model}:generateContent',
  },
] as const

export type Protocol = (typeof PROTOCOLS)[number]['id']

export const PROTOCOL_IDS = PROTOCOLS.map((protocol) => protocol.id)

export function protocolById(id: Protocol) {
  return PROTOCOLS.find((protocol) => protocol.id === id) ?? PROTOCOLS[0]
}

export const CHANNEL_KINDS = [
  ...PROTOCOLS.map((protocol) => ({ id: protocol.id, path: protocol.path })),
  { id: 'aggregate', path: '1-4 endpoints' },
] as const
