//! 凭据静态加密。
//!
//! 渠道凭据默认明文落在 SQLite 里 —— 数据库文件一旦泄漏（备份、拷贝、
//! 误提交），所有上游 API key 等于裸奔。本模块提供 AES-256-GCM 的
//! 对称加密层，格式为：
//!
//! ```text
//! refract.v1. + base64(12 字节随机 nonce || ciphertext + 16 字节 tag)
//! ```
//!
//! AAD 固定为 `b"refract"`：密文脱离本上下文（比如被粘到别的字段）时
//! 解密会失败。版本前缀留给将来换算法；没有前缀的值一律按明文透传，
//! 这是「先明文上线、后加密迁移」的向后兼容保证。

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine as _;
use rand::RngExt as _;

/// 密文版本前缀。没有这个前缀的存储值视为明文。
pub const PREFIX: &str = "refract.v1.";

/// 认证加密关联数据：把密文绑定到「refract 凭据」这一语义上。
const AAD: &[u8] = b"refract";

/// 静态加密层的错误。
#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    /// base64 解码失败。
    #[error("invalid base64: {0}")]
    Base64(#[from] base64::DecodeError),
    /// 密文体太短，连 nonce 都装不下。
    #[error("ciphertext too short")]
    TooShort,
    /// AEAD 解密失败：密钥不对、密文被篡改或 AAD 不匹配。
    #[error("decryption failed")]
    DecryptionFailed,
    /// 解密出的明文不是合法 UTF-8。
    #[error("plaintext is not valid utf8")]
    Utf8(#[from] std::string::FromUtf8Error),
    /// 主密钥长度不对（必须是 32 字节的 base64）。
    #[error("master key must decode to exactly 32 bytes, got {0}")]
    InvalidKeyLength(usize),
}

/// 存储值是否已经是本模块加密过的密文。
pub fn is_encrypted(stored: &str) -> bool {
    stored.starts_with(PREFIX)
}

/// 加密一个凭据，返回带版本前缀的存储形式。
///
/// 每次加密都生成新的随机 nonce —— 同一明文每次落库的密文都不同，
/// 避免「两把相同的 key 在库里长得一样」这种流量分析。
pub fn encrypt_credential(plaintext: &str, key: &[u8; 32]) -> Result<String, CryptoError> {
    let cipher = Aes256Gcm::new_from_slice(key).expect("key is fixed at 32 bytes");
    let mut nonce_bytes = [0_u8; 12];
    rand::rng().fill(&mut nonce_bytes);
    let nonce = Nonce::from(nonce_bytes);

    let ciphertext = cipher
        .encrypt(
            &nonce,
            Payload {
                msg: plaintext.as_bytes(),
                aad: AAD,
            },
        )
        .map_err(|_| CryptoError::DecryptionFailed)?;

    let mut blob = Vec::with_capacity(nonce_bytes.len() + ciphertext.len());
    blob.extend_from_slice(&nonce_bytes);
    blob.extend_from_slice(&ciphertext);
    Ok(format!(
        "{PREFIX}{}",
        base64::engine::general_purpose::STANDARD.encode(blob)
    ))
}

/// 解密一个存储值。
///
/// 无前缀的值按明文原样返回（向后兼容）；带前缀但解密失败时返回错误，
/// 由调用方决定如何兜底 —— 仓储层的约定是「解密失败按明文透传」。
pub fn decrypt_credential(stored: &str, key: &[u8; 32]) -> Result<String, CryptoError> {
    let Some(body) = stored.strip_prefix(PREFIX) else {
        return Ok(stored.to_owned());
    };

    let blob = base64::engine::general_purpose::STANDARD.decode(body)?;
    if blob.len() < 12 {
        return Err(CryptoError::TooShort);
    }

    let cipher = Aes256Gcm::new_from_slice(key).expect("key is fixed at 32 bytes");
    let plaintext = cipher
        .decrypt(
            &Nonce::from(<[u8; 12]>::try_from(&blob[..12]).map_err(|_| CryptoError::TooShort)?),
            Payload {
                msg: &blob[12..],
                aad: AAD,
            },
        )
        .map_err(|_| CryptoError::DecryptionFailed)?;

    Ok(String::from_utf8(plaintext)?)
}

/// 解析 base64 编码的主密钥，必须解码出恰好 32 字节。
///
/// 同时接受标准 base64（含 padding）与 URL-safe 无 padding 两种写法 ——
/// `openssl rand -base64 32` 生成的是前者，用户手敲时常常丢掉 padding。
pub fn parse_master_key(encoded: &str) -> Result<[u8; 32], CryptoError> {
    let trimmed = encoded.trim();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(trimmed)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(trimmed))?;
    if bytes.len() != 32 {
        return Err(CryptoError::InvalidKeyLength(bytes.len()));
    }
    let mut key = [0_u8; 32];
    key.copy_from_slice(&bytes);
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_a() -> [u8; 32] {
        [7_u8; 32]
    }

    fn key_b() -> [u8; 32] {
        [9_u8; 32]
    }

    #[test]
    fn encrypt_decrypt_roundtrips() {
        let secret = "sk-ant-api03-中文-emoji-🚀";
        let encrypted = encrypt_credential(secret, &key_a()).unwrap();
        assert!(is_encrypted(&encrypted));
        assert_ne!(encrypted, secret);
        assert_eq!(decrypt_credential(&encrypted, &key_a()).unwrap(), secret);
    }

    #[test]
    fn same_plaintext_produces_different_ciphertexts() {
        // 随机 nonce：库里两把相同的 key 不能长得一样。
        let a = encrypt_credential("sk-same", &key_a()).unwrap();
        let b = encrypt_credential("sk-same", &key_a()).unwrap();
        assert_ne!(a, b);
        assert_eq!(
            decrypt_credential(&a, &key_a()).unwrap(),
            decrypt_credential(&b, &key_a()).unwrap()
        );
    }

    #[test]
    fn unprefixed_value_passes_through_as_plaintext() {
        // 向后兼容：迁移前的明文原样返回。
        assert_eq!(
            decrypt_credential("sk-legacy-plaintext", &key_a()).unwrap(),
            "sk-legacy-plaintext"
        );
    }

    #[test]
    fn wrong_key_fails_to_decrypt() {
        let encrypted = encrypt_credential("sk-secret", &key_a()).unwrap();
        assert!(decrypt_credential(&encrypted, &key_b()).is_err());
    }

    #[test]
    fn corrupt_ciphertext_fails_to_decrypt() {
        // 前缀在但 body 是垃圾：base64 或 AEAD 校验必须拦下。
        assert!(decrypt_credential("refract.v1.!!!not-base64!!!", &key_a()).is_err());
        assert!(decrypt_credential("refract.v1.YWJj", &key_a()).is_err()); // 太短
        let mut encrypted = encrypt_credential("sk-secret", &key_a()).unwrap();
        // 翻转密文最后一个字符：tag 校验失败。
        let last = encrypted.pop().unwrap();
        let flipped = if last == 'A' { 'B' } else { 'A' };
        encrypted.push(flipped);
        assert!(decrypt_credential(&encrypted, &key_a()).is_err());
    }

    #[test]
    fn is_encrypted_recognizes_prefix_only() {
        assert!(is_encrypted("refract.v1.AAAA"));
        assert!(!is_encrypted("sk-plain"));
        assert!(!is_encrypted("refract.v2.AAAA"));
        assert!(!is_encrypted(""));
    }

    #[test]
    fn parse_master_key_accepts_exactly_32_bytes() {
        let key = [0xAB_u8; 32];
        let encoded = base64::engine::general_purpose::STANDARD.encode(key);
        assert_eq!(parse_master_key(&encoded).unwrap(), key);
        // URL-safe 无 padding 写法同样接受。
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(key);
        assert_eq!(parse_master_key(&encoded).unwrap(), key);
        // 首尾空白容错。
        let padded = format!("  {encoded}\n");
        assert_eq!(parse_master_key(&padded).unwrap(), key);
    }

    #[test]
    fn parse_master_key_rejects_wrong_length_and_garbage() {
        let too_short = base64::engine::general_purpose::STANDARD.encode([0_u8; 16]);
        assert!(matches!(
            parse_master_key(&too_short),
            Err(CryptoError::InvalidKeyLength(16))
        ));
        let too_long = base64::engine::general_purpose::STANDARD.encode([0_u8; 64]);
        assert!(matches!(
            parse_master_key(&too_long),
            Err(CryptoError::InvalidKeyLength(64))
        ));
        assert!(parse_master_key("not base64 at all").is_err());
        assert!(parse_master_key("").is_err());
    }
}
