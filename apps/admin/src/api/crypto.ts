/**
 * 传输层端到端加密 (Web Crypto ECDH P-256 + AES-256-GCM)。
 *
 * 在浏览器沙箱内利用原生 Web Cryptography API：
 * 1. 从 `/api/crypto/public-key` 获取并缓存服务端的 P-256 公钥。
 * 2. 对每个发往后端的写操作（POST / PUT），生成一次性临时密钥对 (Ephemeral Keypair)。
 * 3. 协商 256 位共享密钥并采用 AES-256-GCM 加密完整 JSON Payload。
 * 4. 传输过程中网线上只有高熵密文与临时公钥，即使局域网抓包也无法嗅探明文 API 密钥。
 */

interface PublicKeyResponse {
  algorithm: string
  named_curve: string
  public_key_spki: string
  public_key_raw: string
}

export interface EncryptedEnvelope {
  __encrypted: true
  ephemeral_pub: string
  iv: string
  ciphertext: string
}

let cachedServerPublicKey: CryptoKey | null = null
let pendingFetch: Promise<CryptoKey> | null = null

/** Base64 字符串转 Uint8Array */
function base64ToUint8Array(base64: string): Uint8Array {
  const binary = window.atob(base64)
  const bytes = new Uint8Array(binary.length)
  for (let i = 0; i < binary.length; i += 1) {
    bytes[i] = binary.charCodeAt(i)
  }
  return bytes
}

/** ArrayBuffer/Uint8Array 转 Base64 字符串 */
function arrayBufferToBase64(buffer: ArrayBuffer | Uint8Array): string {
  const bytes = buffer instanceof Uint8Array ? buffer : new Uint8Array(buffer)
  let binary = ''
  for (let i = 0; i < bytes.byteLength; i += 1) {
    binary += String.fromCharCode(bytes[i]!)
  }
  return window.btoa(binary)
}

/**
 * 获取并缓存服务端 P-256 ECDH 公钥。
 */
export async function getServerPublicKey(): Promise<CryptoKey> {
  if (cachedServerPublicKey) return cachedServerPublicKey
  if (pendingFetch) return pendingFetch

  pendingFetch = (async () => {
    try {
      const res = await fetch('/api/crypto/public-key')
      if (!res.ok) {
        throw new Error(`Failed to fetch server public key: HTTP ${res.status}`)
      }
      const json = (await res.json()) as { data: PublicKeyResponse }
      const rawBytes = base64ToUint8Array(json.data.public_key_raw)

      const cryptoKey = await window.crypto.subtle.importKey(
        'raw',
        rawBytes.buffer as ArrayBuffer,
        { name: 'ECDH', namedCurve: 'P-256' },
        true,
        [] as KeyUsage[],
      )
      cachedServerPublicKey = cryptoKey
      return cryptoKey
    } finally {
      pendingFetch = null
    }
  })()

  return pendingFetch
}

/**
 * 加密 JSON 载荷为 EncryptedEnvelope。
 * 若环境不支持 Web Crypto 或公钥拉取失败，返回原载荷（后端兼容）。
 */
export async function encryptPayload(payload: unknown): Promise<unknown> {
  if (!window.crypto?.subtle || payload === undefined || payload === null) {
    return payload
  }

  try {
    const serverPubKey = await getServerPublicKey()

    // 1. 生成客户端一次性 Ephemeral 密钥对
    const clientKeyPair = await window.crypto.subtle.generateKey(
      { name: 'ECDH', namedCurve: 'P-256' },
      true,
      ['deriveBits'],
    )

    // 2. 导出客户端临时公钥 (Raw 65 字节)
    const clientPubRaw = await window.crypto.subtle.exportKey('raw', clientKeyPair.publicKey)
    const ephemeralPubB64 = arrayBufferToBase64(clientPubRaw)

    // 3. ECDH 协商 256 位共享密钥 (Z)
    const sharedBits = await window.crypto.subtle.deriveBits(
      { name: 'ECDH', public: serverPubKey },
      clientKeyPair.privateKey,
      256,
    )

    // 4. 将共享密钥作为 AES-GCM-256 密钥导入
    const aesKey = await window.crypto.subtle.importKey(
      'raw',
      sharedBits,
      { name: 'AES-GCM', length: 256 },
      false,
      ['encrypt'] as KeyUsage[],
    )

    // 5. 生成 12 字节随机 IV
    const iv = window.crypto.getRandomValues(new Uint8Array(12))

    // 6. AES-GCM 加密 JSON 字符串
    const jsonString = JSON.stringify(payload)
    const encodedPlaintext = new TextEncoder().encode(jsonString)
    const ciphertextBuffer = await window.crypto.subtle.encrypt(
      { name: 'AES-GCM', iv },
      aesKey,
      encodedPlaintext,
    )

    const envelope: EncryptedEnvelope = {
      __encrypted: true,
      ephemeral_pub: ephemeralPubB64,
      iv: arrayBufferToBase64(iv),
      ciphertext: arrayBufferToBase64(ciphertextBuffer),
    }

    return envelope
  } catch (e) {
    console.warn('[Refract] Web Crypto E2E encryption fallback to plaintext:', e)
    return payload
  }
}
