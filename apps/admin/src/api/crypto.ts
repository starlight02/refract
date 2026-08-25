import { squash } from 'effect/Cause'
import { catchCause, ensuring, fail, gen, promise, runPromise, succeed, sync } from 'effect/Effect'

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

  // ensuring 取代 finally：无论成功、失败还是中断，都要把在飞标记清掉，
  // 否则一次失败会把后续所有调用永久钉在这个已拒绝的 promise 上。
  pendingFetch = runPromise(
    gen(function* () {
      const res = yield* promise(() => fetch('/api/crypto/public-key'))
      if (!res.ok) {
        return yield* fail(new Error(`Failed to fetch server public key: HTTP ${res.status}`))
      }
      const json = yield* promise(() => res.json() as Promise<{ data: PublicKeyResponse }>)
      const rawBytes = base64ToUint8Array(json.data.public_key_raw)

      const cryptoKey = yield* promise(() =>
        window.crypto.subtle.importKey(
          'raw',
          rawBytes.buffer as ArrayBuffer,
          { name: 'ECDH', namedCurve: 'P-256' },
          true,
          [] as KeyUsage[],
        ),
      )
      cachedServerPublicKey = cryptoKey
      return cryptoKey
    }).pipe(
      ensuring(
        sync(() => {
          pendingFetch = null
        }),
      ),
    ),
  )

  return pendingFetch
}

/**
 * 加密 JSON 载荷为 EncryptedEnvelope。
 * 若环境不支持 Web Crypto 或公钥拉取失败，返回原载荷（后端兼容）。
 */
export function encryptPayload(payload: unknown): Promise<unknown> {
  if (!window.crypto?.subtle || payload === undefined || payload === null) {
    return Promise.resolve(payload)
  }

  const sealed = gen(function* () {
    const serverPubKey = yield* promise(() => getServerPublicKey())

    // 1. 生成客户端一次性 Ephemeral 密钥对
    const clientKeyPair = yield* promise(() =>
      window.crypto.subtle.generateKey({ name: 'ECDH', namedCurve: 'P-256' }, true, ['deriveBits']),
    )

    // 2. 导出客户端临时公钥 (Raw 65 字节)
    const clientPubRaw = yield* promise(() =>
      window.crypto.subtle.exportKey('raw', clientKeyPair.publicKey),
    )

    // 3. ECDH 协商 256 位共享密钥 (Z)
    const sharedBits = yield* promise(() =>
      window.crypto.subtle.deriveBits(
        { name: 'ECDH', public: serverPubKey },
        clientKeyPair.privateKey,
        256,
      ),
    )

    // 4. 将共享密钥作为 AES-GCM-256 密钥导入
    const aesKey = yield* promise(() =>
      window.crypto.subtle.importKey('raw', sharedBits, { name: 'AES-GCM', length: 256 }, false, [
        'encrypt',
      ] as KeyUsage[]),
    )

    // 5. 生成 12 字节随机 IV
    const iv = window.crypto.getRandomValues(new Uint8Array(12))

    // 6. AES-GCM 加密 JSON 字符串
    const ciphertextBuffer = yield* promise(() =>
      window.crypto.subtle.encrypt(
        { name: 'AES-GCM', iv },
        aesKey,
        new TextEncoder().encode(JSON.stringify(payload)),
      ),
    )

    return {
      __encrypted: true,
      ephemeral_pub: arrayBufferToBase64(clientPubRaw),
      iv: arrayBufferToBase64(iv),
      ciphertext: arrayBufferToBase64(ciphertextBuffer),
    } satisfies EncryptedEnvelope
  })

  // 任何一步失败都退回明文：后端两种都收，加密是增强而非前提。
  // catchCause 而不是 catch —— Web Crypto 抛出的是缺陷（defect）而非类型化错误，
  // 只有连缺陷一起接住才能真正保证降级。
  return runPromise(
    sealed.pipe(
      catchCause((cause) => {
        console.warn('[Refract] Web Crypto E2E encryption fallback to plaintext:', squash(cause))
        return succeed(payload)
      }),
    ),
  )
}
