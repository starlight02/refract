//! 端到端传输加密 (Web Crypto ECDH P-256 + HKDF-SHA256 + AES-256-GCM)。
//!
//! 解决管理端 Web 界面与容器/后端之间在非 TLS 部署下的传输明文问题：
//! 1. 服务端在启动时生成或持有 P-256 密钥对，通过 `GET /api/crypto/public-key` 暴露公钥。
//! 2. 前端 (Web Crypto API) 每次发起敏感写操作时，生成临时 (Ephemeral) P-256 密钥对，
//!    通过 ECDH 计算共享密钥 (Z)，再经 HKDF-SHA256 派生 AES-256-GCM 密钥，加密整个请求体。
//! 3. 服务端在反序列化前透明解密，提供前向安全（Forward Secrecy）并防止抓包嗅探。
//!
//! 密钥派生约定（前端必须严格镜像，见 `apps/admin/src/api/crypto.ts`）：
//! - **salt** = 客户端临时公钥的未压缩 SEC1 点 (65B) ‖ 服务端公钥的未压缩 SEC1 点 (65B)。
//!   无论信封里 `ephemeral_pub` 采用 Raw 还是 SPKI 编码，salt 一律按未压缩点计算。
//! - **info** = [`HKDF_INFO`]，固定域分离字符串。
//! - **AAD** = `"<ephemeral_pub base64>:<iv base64>"` 的 UTF-8 字节，即信封中两个
//!   base64 字段原样拼接，防止信封字段被跨会话重组替换。

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine as _;
use hkdf::Hkdf;
use p256::elliptic_curve::rand_core::OsRng;
use p256::elliptic_curve::sec1::{FromEncodedPoint, ToEncodedPoint};
use p256::pkcs8::EncodePublicKey as _;
use p256::{EncodedPoint, PublicKey, SecretKey};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::sync::Arc;

/// HKDF info 域分离字符串：同时充当协议版本号。
/// 前端 `crypto.ts` 中的 `HKDF_INFO` 常量必须与此逐字节一致。
pub const HKDF_INFO: &[u8] = b"refract-transport-v2";

/// 加密信封请求体格式（前端发送）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncryptedEnvelope {
    /// 标识这是加密信封。
    pub __encrypted: bool,
    /// 客户端一次性临时 P-256 公钥（Raw 未压缩 65 字节或 SPKI Base64）。
    pub ephemeral_pub: String,
    /// 12 字节随机 IV (Base64)。
    pub iv: String,
    /// AES-256-GCM 密文 + 16 字节 Auth Tag (Base64)。
    pub ciphertext: String,
}

/// 公开公钥响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublicKeyResponse {
    /// 算法标识。
    pub algorithm: &'static str,
    /// 椭圆曲线名。
    pub named_curve: &'static str,
    /// SPKI Base64（供浏览器 `crypto.subtle.importKey('spki', ...)` 导入）。
    pub public_key_spki: String,
    /// Raw Uncompressed Point Base64（供 `crypto.subtle.importKey('raw', ...)` 导入）。
    pub public_key_raw: String,
}

/// 服务端传输加密密钥管理器。
#[derive(Clone)]
pub struct TransportCrypto {
    secret: Arc<SecretKey>,
    public_spki_base64: String,
    public_raw_base64: String,
    /// 服务端公钥的未压缩 SEC1 点 (0x04 ‖ X ‖ Y)，用于 HKDF salt。
    public_raw_point: Vec<u8>,
}

impl std::fmt::Debug for TransportCrypto {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TransportCrypto")
            .field("public_raw", &self.public_raw_base64)
            .finish()
    }
}

impl TransportCrypto {
    /// 生成新的随机服务端 P-256 密钥对。
    pub fn new_random() -> Self {
        let secret = SecretKey::random(&mut OsRng);
        let public_key = secret.public_key();

        // 导出 SPKI
        let spki_doc = public_key
            .to_public_key_der()
            .expect("valid P-256 public key must export to SPKI DER");
        let public_spki_base64 =
            base64::engine::general_purpose::STANDARD.encode(spki_doc.as_bytes());

        // 导出 Raw 65 字节 (0x04 || X || Y)
        let raw_bytes = public_key.to_encoded_point(false);
        let public_raw_base64 =
            base64::engine::general_purpose::STANDARD.encode(raw_bytes.as_bytes());

        Self {
            secret: Arc::new(secret),
            public_spki_base64,
            public_raw_base64,
            public_raw_point: raw_bytes.as_bytes().to_vec(),
        }
    }

    /// 获取公钥响应体。
    pub fn public_key_response(&self) -> PublicKeyResponse {
        PublicKeyResponse {
            algorithm: "ECDH",
            named_curve: "P-256",
            public_key_spki: self.public_spki_base64.clone(),
            public_key_raw: self.public_raw_base64.clone(),
        }
    }

    /// 解密客户端发来的加密信封，还原为原始 JSON 字节流。
    pub fn decrypt_envelope(&self, envelope: &EncryptedEnvelope) -> Result<Vec<u8>, String> {
        // 1. 解析客户端临时公钥（支持 Raw 65 字节或 SPKI）
        let client_pub_bytes = base64::engine::general_purpose::STANDARD
            .decode(&envelope.ephemeral_pub)
            .map_err(|e| format!("invalid ephemeral_pub base64: {e}"))?;

        let client_pub: PublicKey = if client_pub_bytes.len() == 65 && client_pub_bytes[0] == 0x04 {
            let point = EncodedPoint::from_bytes(&client_pub_bytes)
                .map_err(|e| format!("invalid SEC1 P-256 point: {e}"))?;
            Option::from(PublicKey::from_encoded_point(&point))
                .ok_or_else(|| "invalid P-256 point coordinates".to_string())?
        } else {
            p256::pkcs8::DecodePublicKey::from_public_key_der(&client_pub_bytes)
                .map_err(|e| format!("invalid public key format (neither raw nor SPKI): {e}"))?
        };

        // 2. ECDH 密钥协商得到共享秘密 Z，再用 HKDF-SHA256 派生 AES-256-GCM 密钥。
        // salt = 客户端临时公钥未压缩点 ‖ 服务端公钥未压缩点，绑定本次会话双方身份；
        // 原始 DH 输出有偏置，直接当对称密钥不符合 SP 800-56A，必须经过 KDF。
        let shared_secret =
            p256::ecdh::diffie_hellman(self.secret.to_nonzero_scalar(), client_pub.as_affine());
        let client_raw_point = client_pub.to_encoded_point(false);
        let mut salt = Vec::with_capacity(130);
        salt.extend_from_slice(client_raw_point.as_bytes());
        salt.extend_from_slice(&self.public_raw_point);
        let mut derived_key = [0u8; 32];
        Hkdf::<Sha256>::new(Some(&salt), shared_secret.raw_secret_bytes().as_ref())
            .expand(HKDF_INFO, &mut derived_key)
            .map_err(|e| format!("HKDF expand failed: {e}"))?;

        // 3. 解析 IV 与密文
        let iv = base64::engine::general_purpose::STANDARD
            .decode(&envelope.iv)
            .map_err(|e| format!("invalid iv base64: {e}"))?;
        let iv_array = <[u8; 12]>::try_from(iv.as_slice())
            .map_err(|_| format!("invalid AES-GCM IV length: expected 12, got {}", iv.len()))?;
        let ciphertext = base64::engine::general_purpose::STANDARD
            .decode(&envelope.ciphertext)
            .map_err(|e| format!("invalid ciphertext base64: {e}"))?;

        // 4. AES-256-GCM 解密，AAD 绑定信封的公钥与 IV 字段，
        // 防止密文/IV/临时公钥被跨会话重组替换。
        let aad = format!("{}:{}", envelope.ephemeral_pub, envelope.iv);
        let cipher = Aes256Gcm::new_from_slice(&derived_key)
            .map_err(|e| format!("failed to init AES-GCM: {e}"))?;
        let plaintext = cipher
            .decrypt(
                &Nonce::from(iv_array),
                Payload {
                    msg: ciphertext.as_ref(),
                    aad: aad.as_bytes(),
                },
            )
            .map_err(|e| {
                format!("failed to decrypt envelope payload (authentication tag mismatch): {e}")
            })?;

        Ok(plaintext)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes_gcm::aead::Payload;

    /// 模拟客户端 (Web Crypto) 按新协议构造加密信封：
    /// HKDF-SHA256(salt = client_raw ‖ server_raw, ikm = Z, info = HKDF_INFO) → AES key，
    /// AAD = "ephemeral_pub_b64:iv_b64"。
    fn client_seal(pk_resp: &PublicKeyResponse, payload: &str) -> EncryptedEnvelope {
        // 1. 客户端生成临时密钥对
        let client_secret = SecretKey::random(&mut OsRng);
        let client_pub = client_secret.public_key();
        let client_pub_raw = client_pub.to_encoded_point(false);
        let client_pub_b64 =
            base64::engine::general_purpose::STANDARD.encode(client_pub_raw.as_bytes());

        // 2. 客户端导入服务端公钥并 ECDH deriveBits
        let server_pub_bytes = base64::engine::general_purpose::STANDARD
            .decode(&pk_resp.public_key_raw)
            .unwrap();
        let server_point = EncodedPoint::from_bytes(&server_pub_bytes).unwrap();
        let server_pub = PublicKey::from_encoded_point(&server_point).unwrap();

        let client_shared =
            p256::ecdh::diffie_hellman(client_secret.to_nonzero_scalar(), server_pub.as_affine());

        // 3. HKDF 派生 AES 密钥（salt 绑定双方公钥）
        let mut salt = Vec::with_capacity(130);
        salt.extend_from_slice(client_pub_raw.as_bytes());
        salt.extend_from_slice(server_point.as_bytes());
        let mut aes_key_bytes = [0u8; 32];
        Hkdf::<Sha256>::new(Some(&salt), client_shared.raw_secret_bytes().as_ref())
            .expand(HKDF_INFO, &mut aes_key_bytes)
            .unwrap();

        // 4. AES-256-GCM 加密，AAD 绑定信封字段
        let iv_bytes = [7u8; 12];
        let iv_b64 = base64::engine::general_purpose::STANDARD.encode(iv_bytes);
        let aad = format!("{client_pub_b64}:{iv_b64}");
        let cipher = Aes256Gcm::new_from_slice(&aes_key_bytes).unwrap();
        let ciphertext_bytes = cipher
            .encrypt(
                &Nonce::from(iv_bytes),
                Payload {
                    msg: payload.as_bytes(),
                    aad: aad.as_bytes(),
                },
            )
            .unwrap();

        EncryptedEnvelope {
            __encrypted: true,
            ephemeral_pub: client_pub_b64,
            iv: iv_b64,
            ciphertext: base64::engine::general_purpose::STANDARD.encode(ciphertext_bytes),
        }
    }

    #[test]
    fn test_e2e_envelope_encryption_decryption() {
        let server_crypto = TransportCrypto::new_random();
        let pk_resp = server_crypto.public_key_response();

        let payload = r#"{"name":"my-channel","credential":"sk-secret-upstream-key-12345"}"#;
        let envelope = client_seal(&pk_resp, payload);

        let decrypted = server_crypto
            .decrypt_envelope(&envelope)
            .expect("decryption must succeed");
        assert_eq!(String::from_utf8(decrypted).unwrap(), payload);
    }

    #[test]
    fn test_spki_encoded_ephemeral_pub_also_decrypts() {
        // salt 按未压缩点计算而非信封里的编码形式，因此信封用 SPKI 编码也应解密成功。
        let server_crypto = TransportCrypto::new_random();
        let pk_resp = server_crypto.public_key_response();
        let payload = r#"{"k":"v"}"#;

        let client_secret = SecretKey::random(&mut OsRng);
        let client_pub = client_secret.public_key();
        let spki_b64 = base64::engine::general_purpose::STANDARD
            .encode(client_pub.to_public_key_der().unwrap().as_bytes());
        let server_pub_bytes = base64::engine::general_purpose::STANDARD
            .decode(&pk_resp.public_key_raw)
            .unwrap();
        let server_point = EncodedPoint::from_bytes(&server_pub_bytes).unwrap();
        let server_pub = PublicKey::from_encoded_point(&server_point).unwrap();
        let shared =
            p256::ecdh::diffie_hellman(client_secret.to_nonzero_scalar(), server_pub.as_affine());
        let mut salt = Vec::with_capacity(130);
        salt.extend_from_slice(client_pub.to_encoded_point(false).as_bytes());
        salt.extend_from_slice(server_point.as_bytes());
        let mut key = [0u8; 32];
        Hkdf::<Sha256>::new(Some(&salt), shared.raw_secret_bytes().as_ref())
            .expand(HKDF_INFO, &mut key)
            .unwrap();
        let iv_bytes = [9u8; 12];
        let iv_b64 = base64::engine::general_purpose::STANDARD.encode(iv_bytes);
        let aad = format!("{spki_b64}:{iv_b64}");
        let cipher = Aes256Gcm::new_from_slice(&key).unwrap();
        let ct = cipher
            .encrypt(
                &Nonce::from(iv_bytes),
                Payload {
                    msg: payload.as_bytes(),
                    aad: aad.as_bytes(),
                },
            )
            .unwrap();
        let spki_envelope = EncryptedEnvelope {
            __encrypted: true,
            ephemeral_pub: spki_b64,
            iv: iv_b64,
            ciphertext: base64::engine::general_purpose::STANDARD.encode(ct),
        };

        let decrypted = server_crypto
            .decrypt_envelope(&spki_envelope)
            .expect("SPKI-encoded envelope must decrypt");
        assert_eq!(String::from_utf8(decrypted).unwrap(), payload);
    }

    #[test]
    fn test_tampered_envelope_rejected() {
        // 把 A 信封的密文挪到 B 信封上（跨会话重组）：AAD 不匹配必须被拒绝。
        let server_crypto = TransportCrypto::new_random();
        let pk_resp = server_crypto.public_key_response();

        let env_a = client_seal(&pk_resp, r#"{"victim":"a"}"#);
        let mut env_b = client_seal(&pk_resp, r#"{"victim":"b"}"#);
        env_b.ciphertext = env_a.ciphertext;

        assert!(
            server_crypto.decrypt_envelope(&env_b).is_err(),
            "recombined envelope must fail authentication"
        );
    }

    #[test]
    fn test_legacy_raw_shared_secret_envelope_rejected() {
        // v1 旧协议（直接用共享秘密当密钥、无 AAD）的信封必须解密失败，
        // 防止实现意外回退到弱路径。
        let server_crypto = TransportCrypto::new_random();
        let pk_resp = server_crypto.public_key_response();

        let client_secret = SecretKey::random(&mut OsRng);
        let client_pub = client_secret.public_key();
        let client_pub_b64 = base64::engine::general_purpose::STANDARD
            .encode(client_pub.to_encoded_point(false).as_bytes());
        let server_pub_bytes = base64::engine::general_purpose::STANDARD
            .decode(&pk_resp.public_key_raw)
            .unwrap();
        let server_pub =
            PublicKey::from_encoded_point(&EncodedPoint::from_bytes(&server_pub_bytes).unwrap())
                .unwrap();
        let shared =
            p256::ecdh::diffie_hellman(client_secret.to_nonzero_scalar(), server_pub.as_affine());
        let iv_bytes = [3u8; 12];
        let cipher = Aes256Gcm::new_from_slice(shared.raw_secret_bytes().as_ref()).unwrap();
        let ct = cipher
            .encrypt(&Nonce::from(iv_bytes), b"legacy" as &[u8])
            .unwrap();
        let legacy = EncryptedEnvelope {
            __encrypted: true,
            ephemeral_pub: client_pub_b64,
            iv: base64::engine::general_purpose::STANDARD.encode(iv_bytes),
            ciphertext: base64::engine::general_purpose::STANDARD.encode(ct),
        };

        assert!(
            server_crypto.decrypt_envelope(&legacy).is_err(),
            "legacy v1 envelope must not decrypt under v2"
        );
    }
}
