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

/** Web Crypto 不可用或加密失败、回落明文时派发，供界面提示。 */
export const CRYPTO_PLAINTEXT_EVENT = 'refract:crypto-plaintext'

function announcePlaintextFallback(reason: unknown): void {
  console.warn('[Refract] Web Crypto E2E encryption fallback to plaintext:', reason)
  if (typeof window !== 'undefined') {
    window.dispatchEvent(new CustomEvent(CRYPTO_PLAINTEXT_EVENT, { detail: reason }))
  }
}

/**
 * HKDF info 域分离字符串，同时充当协议版本号。
 * 必须与后端 `crates/refract-api/src/crypto.rs` 的 `HKDF_INFO` 逐字节一致。
 */
const HKDF_INFO = new TextEncoder().encode('refract-transport-v2')

/** 缓存的服务端公钥：Web Crypto key + 未压缩 SEC1 点字节（HKDF salt 需要）。 */
interface ServerPublicKeyCache {
  cryptoKey: CryptoKey
  rawBytes: Uint8Array
}

let cachedServerPublicKey: ServerPublicKeyCache | null = null
let pendingFetch: Promise<ServerPublicKeyCache> | null = null

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
 * 返回 Web Crypto key 与其未压缩 SEC1 点字节（构造 HKDF salt 用）。
 */
export async function getServerPublicKey(): Promise<ServerPublicKeyCache> {
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
      const cached = { cryptoKey, rawBytes }
      cachedServerPublicKey = cached
      return cached
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
 *
 * 协议（与后端 `crates/refract-api/src/crypto.rs` 严格镜像）：
 * 1. 客户端生成一次性 P-256 临时密钥对，与服务端公钥做 ECDH 得到共享秘密 Z；
 * 2. HKDF-SHA256(ikm=Z, salt=客户端临时公钥未压缩点‖服务端公钥未压缩点,
 *    info=HKDF_INFO) 派生 AES-256-GCM 密钥；
 * 3. AES-GCM 加密时 additionalData 绑定信封的公钥与 IV 字段
 *    (`"${ephemeral_pub}:${iv}"` 的 UTF-8 字节)，防止字段被跨会话重组。
 *
 * 若环境不支持 Web Crypto 或公钥拉取失败，返回原载荷（后端兼容）。
 */
export function encryptPayload(payload: unknown): Promise<unknown> {
  if (!window.crypto?.subtle || payload === undefined || payload === null) {
    if (payload !== undefined && payload !== null) {
      announcePlaintextFallback('Web Crypto SubtleCrypto is unavailable')
    }
    return Promise.resolve(payload)
  }

  const sealed = gen(function* () {
    const serverPub = yield* promise(() => getServerPublicKey())

    // 1. 生成客户端一次性 Ephemeral 密钥对
    const clientKeyPair = yield* promise(() =>
      window.crypto.subtle.generateKey({ name: 'ECDH', namedCurve: 'P-256' }, true, ['deriveBits']),
    )

    // 2. 导出客户端临时公钥 (Raw 65 字节)
    const clientPubRaw = yield* promise(() =>
      window.crypto.subtle.exportKey('raw', clientKeyPair.publicKey),
    )

    // 3. ECDH 协商 256 位共享密钥 (Z)——仅作 HKDF 输入，不直接当 AES 密钥
    const sharedBits = yield* promise(() =>
      window.crypto.subtle.deriveBits(
        { name: 'ECDH', public: serverPub.cryptoKey },
        clientKeyPair.privateKey,
        256,
      ),
    )

    // 4. HKDF-SHA256 派生 AES-256-GCM 密钥：
    //    salt = 客户端临时公钥未压缩点 ‖ 服务端公钥未压缩点（共 130 字节），
    //    info = 协议域分离字符串。
    const hkdfKey = yield* promise(() =>
      window.crypto.subtle.importKey('raw', sharedBits, 'HKDF', false, ['deriveBits']),
    )
    const salt = new Uint8Array(clientPubRaw.byteLength + serverPub.rawBytes.byteLength)
    salt.set(new Uint8Array(clientPubRaw), 0)
    salt.set(serverPub.rawBytes, clientPubRaw.byteLength)
    const aesBits = yield* promise(() =>
      window.crypto.subtle.deriveBits(
        { name: 'HKDF', hash: 'SHA-256', salt, info: HKDF_INFO },
        hkdfKey,
        256,
      ),
    )

    // 5. 将派生密钥导入为 AES-GCM-256 密钥
    const aesKey = yield* promise(() =>
      window.crypto.subtle.importKey('raw', aesBits, { name: 'AES-GCM', length: 256 }, false, [
        'encrypt',
      ] as KeyUsage[]),
    )

    // 6. 生成 12 字节随机 IV
    const iv = window.crypto.getRandomValues(new Uint8Array(12))

    // 7. AES-GCM 加密，AAD 绑定信封的公钥与 IV 字段
    const ephemeralPubB64 = arrayBufferToBase64(clientPubRaw)
    const ivB64 = arrayBufferToBase64(iv)
    const aadBytes = new TextEncoder().encode(`${ephemeralPubB64}:${ivB64}`)
    const ciphertextBuffer = yield* promise(() =>
      window.crypto.subtle.encrypt(
        { name: 'AES-GCM', iv, additionalData: aadBytes },
        aesKey,
        new TextEncoder().encode(JSON.stringify(payload)),
      ),
    )

    return {
      __encrypted: true,
      ephemeral_pub: ephemeralPubB64,
      iv: ivB64,
      ciphertext: arrayBufferToBase64(ciphertextBuffer),
    } satisfies EncryptedEnvelope
  })

  // 任何一步失败都退回明文：后端两种都收，加密是增强而非前提。
  // catchCause 而不是 catch —— Web Crypto 抛出的是缺陷（defect）而非类型化错误，
  // 只有连缺陷一起接住才能真正保证降级。
  return runPromise(
    sealed.pipe(
      catchCause((cause) => {
        announcePlaintextFallback(squash(cause))
        return succeed(payload)
      }),
    ),
  )
}
