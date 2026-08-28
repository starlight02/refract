//! 邮件外发：验证码投递。
//!
//! SMTP 未配置时退化为 `tracing::info!` 输出验证码（开发模式默认）。
//! 生产部署通过 `mail.smtp_url` + `mail.from` 启用真实外发。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use refract_store::CodePurpose;

/// 邮件外发错误。
#[derive(Debug, thiserror::Error)]
pub enum MailError {
    /// SMTP 投递失败。
    #[error("smtp delivery failed: {0}")]
    Smtp(String),
}

/// 邮件外发器。
#[derive(Debug, Clone)]
pub struct Mailer {
    smtp_url: Option<String>,
    from: String,
    /// 每个收件人最近一次发出的明文验证码（仅供 dev-codes 钩子）。
    last_codes: Arc<Mutex<HashMap<String, String>>>,
}

impl Mailer {
    /// 从 settings 构造。smtp_url 为空 → 所有发送退化为 log。
    pub fn new(smtp_url: Option<String>, from: String) -> Self {
        Self {
            smtp_url,
            from,
            last_codes: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 发送 6 位验证码。
    pub async fn send_code(
        &self,
        to: &str,
        purpose: CodePurpose,
        code: &str,
    ) -> Result<(), MailError> {
        if let Ok(mut map) = self.last_codes.lock() {
            map.insert(to.to_ascii_lowercase(), code.to_owned());
        }
        let subject = match purpose {
            CodePurpose::VerifyEmail => "Refract 邮箱验证码",
            CodePurpose::ResetPassword => "Refract 密码重置码",
        };
        let body = format!(
            "您的验证码是：{code}\n\n有效期 10 分钟，最多可尝试 5 次。\n如果您没有请求此验证码，请忽略本邮件。"
        );
        match self.smtp_url.as_deref() {
            Some(url) => self.send_smtp(url, to, subject, &body).await,
            None => {
                tracing::info!(
                    to,
                    ?purpose,
                    code,
                    "dev mode: verification code (no SMTP configured)"
                );
                Ok(())
            }
        }
    }

    /// 最近一次发给该邮箱的明文验证码。
    pub fn last_code(&self, email: &str) -> Option<String> {
        self.last_codes
            .lock()
            .ok()?
            .get(&email.to_ascii_lowercase())
            .cloned()
    }

    async fn send_smtp(
        &self,
        url: &str,
        to: &str,
        subject: &str,
        body: &str,
    ) -> Result<(), MailError> {
        // 极简 SMTP 客户端：submission + STARTTLS。
        // lettre 0.11 的 async API 与本项目 tokio 生态兼容，直接复用。
        use lettre::message::Message;
        use lettre::transport::smtp::authentication::Credentials;
        use lettre::{AsyncSmtpTransport, AsyncTransport, Tokio1Executor};

        let url =
            url::Url::parse(url).map_err(|e| MailError::Smtp(format!("invalid smtp url: {e}")))?;
        let host = url
            .host_str()
            .ok_or_else(|| MailError::Smtp("smtp url missing host".into()))?;
        let port = url.port().unwrap_or(587);
        let username = url.username();
        let password = url.password().unwrap_or("");

        let message = Message::builder()
            .from(
                self.from
                    .parse()
                    .map_err(|e| MailError::Smtp(format!("invalid from address: {e}")))?,
            )
            .to(to
                .parse()
                .map_err(|e| MailError::Smtp(format!("invalid to address: {e}")))?)
            .subject(subject)
            .body(body.to_owned())
            .map_err(|e| MailError::Smtp(e.to_string()))?;

        let transport = if username.is_empty() {
            AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(host)
                .port(port)
                .build()
        } else {
            let creds = Credentials::new(username.to_owned(), password.to_owned());
            AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(host)
                .map_err(|e| MailError::Smtp(e.to_string()))?
                .port(port)
                .credentials(creds)
                .build()
        };

        transport
            .send(message)
            .await
            .map_err(|e| MailError::Smtp(e.to_string()))?;
        Ok(())
    }
}

/// 密码哈希（Argon2id，m=19MiB t=2 p=1）。
fn password_hasher() -> argon2::Argon2<'static> {
    let params =
        argon2::Params::new(19 * 1024, 2, 1, None).expect("static Argon2id parameters are valid");
    argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params)
}

/// 密码哈希（Argon2id，m=19MiB t=2 p=1）。
pub fn hash_password(password: &str) -> Result<String, argon2::password_hash::Error> {
    use argon2::password_hash::{PasswordHasher, SaltString};
    // `rand` 0.10 的 ThreadRng 与 `password-hash` 的 rand_core 0.6 不兼容，
    // 用 OS RNG 直接生成盐，避免跨版本 RNG 桥接。
    let salt = SaltString::generate(&mut argon2::password_hash::rand_core::OsRng);
    password_hasher()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
}

/// 校验密码。
pub fn verify_password(password: &str, hash: &str) -> Result<bool, argon2::password_hash::Error> {
    use argon2::password_hash::{PasswordHash, PasswordVerifier};
    let parsed = PasswordHash::new(hash)?;
    Ok(password_hasher()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok())
}

/// 对不存在账号做假哈希用的常量（抹平登录时间侧信道）。
pub const DUMMY_PASSWORD_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHRzb21lc2FsdA$AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_hash_roundtrip() {
        let hash = hash_password("correct horse battery staple").unwrap();
        assert!(verify_password("correct horse battery staple", &hash).unwrap());
        assert!(!verify_password("wrong", &hash).unwrap());
    }

    #[test]
    fn dummy_hash_is_valid_argon2() {
        // 只验证格式可解析，不验证密码内容。
        assert!(verify_password("anything", DUMMY_PASSWORD_HASH).is_ok());
    }
}
