//! 鉴权。
//!
//! 两套独立的凭据，各管一摊：
//! - **网关密钥**（`sk-refract-...`）：客户端调用 `/v1/...` 用，可限模型、限额度。
//! - **管理令牌**：调用 `/api/...` 用，权限是「改配置」。
//!
//! 为什么不用同一套：推理密钥会被写进各种客户端配置文件、粘进聊天窗口、
//! 存进第三方工具。它泄漏的概率远高于管理令牌。如果两者相同，一次泄漏就
//! 意味着攻击者能改渠道配置、看全部日志、导出所有上游密钥。

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Mutex, MutexGuard};
use std::time::Instant;

use async_trait::async_trait;
use refract_core::{Channel, ErrorKind, GatewayError, Protocol};
use refract_store::{ApiKey, ApiKeyRepo, SettingsRepo};
use xitca_web::http::HeaderMap;

/// Session Cookie 名称。
pub const SESSION_COOKIE_NAME: &str = "refract_session";
/// Session Cookie 有效期（7天）。
pub const SESSION_MAX_AGE_SECS: u64 = 7 * 24 * 3600;

/// 已验证会话的主体身份。
///
/// 两种来源：用户邮箱+密码登录（`User`），或管理令牌直登的紧急通道（`LegacyAdmin`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionSubject {
    /// 管理令牌直登（紧急恢复通道）。语义上等同于 bootstrap admin 用户。
    LegacyAdmin,
    /// 某个注册用户。
    User(i64),
}

/// 已验证会话解析结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerifiedSession {
    /// 主体身份。
    pub subject: SessionSubject,
    /// 签发时间（unix 秒）。配合 `users.session_revoked_at` 判定失效。
    pub iat: i64,
}

/// 生成用户 Session Ticket 字符串。
/// 格式: `<issued_at>.<expiry>.<user_id>.<signature_hex>`
/// 签名数据: `format!("{user_id}:{issued_at}:{expiry}")`
pub fn create_user_session_ticket(
    session_secret: &[u8; 32],
    user_id: i64,
    ttl_secs: u64,
) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let expiry = now.saturating_add(ttl_secs);
    let msg = format!("{user_id}:{now}:{expiry}");
    let sig_bytes = crate::notify::hmac_sha256(session_secret, msg.as_bytes());
    let sig = hex::encode(sig_bytes);
    format!("{now}.{expiry}.{user_id}.{sig}")
}

/// 验证 Session Ticket，兼容新旧两种格式。
///
/// 新格式四段：`<iat>.<exp>.<user_id>.<sig>`；旧格式三段：`<iat>.<exp>.<sig>`，
/// 旧格式按 `expected_legacy_hash`（管理令牌哈希）验证，对应 `LegacyAdmin` 主体。
pub fn verify_session_ticket(
    ticket: &str,
    session_secret: &[u8; 32],
    expected_legacy_hash: &str,
) -> Option<VerifiedSession> {
    let parts: Vec<&str> = ticket.split('.').collect();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    match parts.as_slice() {
        [issued_str, expiry_str, user_str, sig_hex] => {
            let iat: i64 = issued_str.parse().ok()?;
            let expiry: u64 = expiry_str.parse().ok()?;
            let user_id: i64 = user_str.parse().ok()?;
            if now >= expiry {
                return None;
            }
            let expected_sig_bytes = hex::decode(sig_hex).ok()?;
            let msg = format!("{user_id}:{issued_str}:{expiry_str}");
            let computed_sig = crate::notify::hmac_sha256(session_secret, msg.as_bytes());
            if !constant_time_eq(&computed_sig, &expected_sig_bytes) {
                return None;
            }
            Some(VerifiedSession {
                subject: SessionSubject::User(user_id),
                iat,
            })
        }
        [issued_str, expiry_str, sig_hex] => {
            let iat: i64 = issued_str.parse().ok()?;
            let expiry: u64 = expiry_str.parse().ok()?;
            if now >= expiry {
                return None;
            }
            let expected_sig_bytes = hex::decode(sig_hex).ok()?;
            let msg = format!("{expected_legacy_hash}:{issued_str}:{expiry_str}");
            let computed_sig = crate::notify::hmac_sha256(session_secret, msg.as_bytes());
            if !constant_time_eq(&computed_sig, &expected_sig_bytes) {
                return None;
            }
            Some(VerifiedSession {
                subject: SessionSubject::LegacyAdmin,
                iat,
            })
        }
        _ => None,
    }
}

/// 从 Cookie 请求头中解析指定名称的 Cookie 值。
pub fn extract_cookie_value(cookie_header: &str, cookie_name: &str) -> Option<String> {
    for pair in cookie_header.split(';') {
        let mut parts = pair.trim().splitn(2, '=');
        if let (Some(name), Some(val)) = (parts.next(), parts.next())
            && name.trim() == cookie_name
        {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

/// 请求是否经 HTTPS 到达（含反代终止 TLS）。
///
/// 认 `X-Forwarded-Proto` 首跳与 RFC 7239 `Forwarded: proto=https`。
/// 本机直连 HTTP 时两者都没有，Cookie 不加 `Secure`，避免浏览器丢掉会话。
pub fn request_is_https(headers: &HeaderMap) -> bool {
    if header_string(headers, "x-forwarded-proto").is_some_and(|value| {
        value
            .split(',')
            .next()
            .is_some_and(|proto| proto.trim().eq_ignore_ascii_case("https"))
    }) {
        return true;
    }
    header_string(headers, "forwarded").is_some_and(|value| {
        value.split(',').any(|forwarded| {
            forwarded.split(';').any(|part| {
                let part = part.trim();
                let lower = part.to_ascii_lowercase();
                lower == "proto=https" || lower.starts_with("proto=https")
            })
        })
    })
}

/// 组装 Session Cookie。`secure` 为真时追加 `Secure`。
pub fn session_cookie(value: &str, max_age_secs: u64, secure: bool) -> String {
    let secure = if secure { "; Secure" } else { "" };
    format!(
        "{SESSION_COOKIE_NAME}={value}; HttpOnly; SameSite=Strict; Path=/{secure}; Max-Age={max_age_secs}"
    )
}

use crate::error::{AppError, ProtocolRejection};
use crate::state::AppState;

/// 身份可执行的操作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrincipalScope {
    /// 调用推理网关与查看其模型清单。
    Gateway,
}

/// 已通过网关鉴权的身份。
///
/// 当前单用户实现总是使用默认 owner；owner 和 scope 仍是身份的一部分，使未来
/// 接入多用户认证时无需改写路由、渠道隔离与日志归属逻辑。
#[derive(Debug, Clone)]
pub struct Principal {
    /// 业务数据所属 owner。
    pub owner_id: i64,
    /// 该密钥/会话归属的用户。`None` 表示免鉴权本地模式（语义上等同 bootstrap admin）。
    pub user_id: Option<i64>,
    /// 此身份拥有的能力。
    pub scopes: Vec<PrincipalScope>,
    /// 用于完成鉴权的网关 API 密钥；未要求鉴权时为空。
    pub api_key: Option<Box<ApiKey>>,
}

impl Principal {
    /// 构造未要求凭据的单用户身份。
    pub fn local(owner_id: i64) -> Self {
        Self {
            owner_id,
            user_id: None,
            scopes: vec![PrincipalScope::Gateway],
            api_key: None,
        }
    }

    /// 构造免鉴权模式下绑定到具体用户（bootstrap admin）的身份。
    pub fn local_user(owner_id: i64, user_id: i64) -> Self {
        Self {
            owner_id,
            user_id: Some(user_id),
            scopes: vec![PrincipalScope::Gateway],
            api_key: None,
        }
    }

    /// 构造由 API 密钥认证的身份。
    pub fn from_api_key(key: ApiKey) -> Self {
        Self {
            owner_id: key.owner_id,
            user_id: key.user_id,
            scopes: vec![PrincipalScope::Gateway],
            api_key: Some(Box::new(key)),
        }
    }

    /// 该身份是否允许调用网关。pending_verification 的 key 已在鉴权层拦截，
    /// 这里只处理 user_id 缺失的极端情况（如老库未回填）。
    pub fn gateway_user_id(&self) -> i64 {
        self.user_id.unwrap_or(self.owner_id)
    }

    /// 是否具备指定能力。
    pub fn has_scope(&self, scope: PrincipalScope) -> bool {
        self.scopes.contains(&scope)
    }

    /// 密钥 ID，用于日志归属。
    pub fn key_id(&self) -> Option<i64> {
        self.api_key.as_ref().map(|key| key.id)
    }

    /// 该调用者是否可以使用某个模型。
    pub fn allows_model(&self, model: &str) -> bool {
        self.has_scope(PrincipalScope::Gateway)
            && self
                .api_key
                .as_ref()
                .is_none_or(|key| key.allows_model(model))
    }

    /// 该调用者是否可以路由到指定渠道。
    ///
    /// 密钥未限制标签时放行全部；限制后，渠道至少要有一个匹配标签。判断必须
    /// 在候选收集前完成，否则失败重试可能绕过首选渠道的权限限制。
    ///
    /// `channel.owner_id == self.owner_id` 检查的是 tenant（多用户期恒为 1），
    /// 不是用户。私有渠与共享渠的 `owner_id` 都是 1；用户级可见性由
    /// [`crate::state::AppState::channels_for`] 保证，这里不必再按 `user_id` 过滤。
    pub fn allows_channel(&self, channel: &Channel) -> bool {
        self.has_scope(PrincipalScope::Gateway)
            && channel.owner_id == self.owner_id
            && self.api_key.as_ref().is_none_or(|key| {
                key.allowed_tags.is_empty()
                    || channel
                        .tags
                        .iter()
                        .any(|tag| key.allowed_tags.iter().any(|allowed| allowed == tag))
            })
    }
}

/// 把请求凭据解析为网关身份。
///
/// HTTP 层只依赖这项能力。未来接入会话、OIDC 或多租户认证时，可以替换实现，
/// 不需要改动协议路由和授权判断。
#[async_trait]
pub trait Authenticator: std::fmt::Debug + Send + Sync {
    /// 校验可选凭据并返回身份。
    async fn authenticate(&self, token: Option<&str>) -> Result<Principal, GatewayError>;
}

/// 多用户网关认证器。
///
/// 校验链：API key 存在且可用 → 归属用户存在且 `active` → 钱包余额为正。
/// 用户级状态与余额走内存缓存（见 [`AppState`]），避免热路径打库。
#[derive(Debug, Clone)]
pub struct SingleUserAuthenticator {
    key_repo: ApiKeyRepo,
    user_repo: refract_store::UserRepo,
    wallet_repo: refract_store::WalletRepo,
    /// 余额预检缓存（与 AppState 共享同一份，充值/扣款后主动失效）。
    balance_cache: crate::rate::TtlCache<i64, f64>,
    require_auth: bool,
    owner_id: i64,
    /// 免鉴权模式下的身份归属（bootstrap admin 用户 ID）。
    fallback_user_id: i64,
}

impl SingleUserAuthenticator {
    /// 创建认证器。`balance_cache` 必须与 [`AppState`] 是同一份实例，
    /// 否则充值/调整后的主动失效对预检不可见。
    pub fn new(
        key_repo: ApiKeyRepo,
        user_repo: refract_store::UserRepo,
        wallet_repo: refract_store::WalletRepo,
        balance_cache: crate::rate::TtlCache<i64, f64>,
        require_auth: bool,
        owner_id: i64,
        fallback_user_id: i64,
    ) -> Self {
        Self {
            key_repo,
            user_repo,
            wallet_repo,
            balance_cache,
            require_auth,
            owner_id,
            fallback_user_id,
        }
    }
}

#[async_trait]
impl Authenticator for SingleUserAuthenticator {
    async fn authenticate(&self, token: Option<&str>) -> Result<Principal, GatewayError> {
        if !self.require_auth {
            return Ok(Principal::local_user(self.owner_id, self.fallback_user_id));
        }

        let token = token.ok_or_else(|| {
            GatewayError::unauthenticated(
                "missing API key; send it as `Authorization: Bearer <key>`",
            )
        })?;
        let key = self
            .key_repo
            .find_by_plaintext(token)
            .await
            .map_err(crate::error::store_to_gateway)?
            .ok_or_else(|| GatewayError::unauthenticated("invalid API key"))?;

        if !key.is_usable(chrono::Utc::now()) {
            return Err(GatewayError::unauthenticated(
                "API key is disabled, expired, or out of quota",
            ));
        }
        if key.owner_id != self.owner_id {
            return Err(GatewayError::unauthenticated("invalid API key"));
        }

        let user_id = key.user_id.unwrap_or(self.fallback_user_id);
        let user = self
            .user_repo
            .find_by_id(user_id)
            .await
            .map_err(crate::error::store_to_gateway)?
            .ok_or_else(|| GatewayError::unauthenticated("invalid API key"))?;
        if user.status != refract_store::UserStatus::Active {
            return Err(GatewayError::new(
                ErrorKind::PermissionDenied,
                "account is not active; verify your email or contact the administrator",
            ));
        }
        // 余额预检走共享缓存（60s TTL）。topup/adjust/refund 后 AppState 主动
        // 失效对应条目；余额为负的用户每 60s 最多打一次库，热路径不查 SQLite。
        let balance = match self.balance_cache.get(&user_id) {
            Some(cached) => cached,
            None => {
                let fresh = self
                    .wallet_repo
                    .balance(user_id)
                    .await
                    .map_err(crate::error::store_to_gateway)?;
                self.balance_cache.insert(user_id, fresh);
                fresh
            }
        };
        if balance <= 0.0 {
            // 协议错误体由 details 注入机器可判字段：OpenAI 形状下
            // 客户端读到 {"error":{"type":"insufficient_balance","balance":...}}。
            return Err(GatewayError::new(
                ErrorKind::PermissionDenied,
                format!("insufficient balance: {balance:.4}"),
            )
            .with_details(serde_json::json!({
                "type": "insufficient_balance",
                "balance": balance,
            })));
        }

        Ok(Principal::from_api_key(key))
    }
}

/// 从各协议的请求头里提取 bearer/api-key。
///
/// 四家的头不一样，且客户端会用它们**原本**的头来调我们 —— 只认
/// `Authorization` 会让 Anthropic SDK 和 Google SDK 直接 401。
pub fn extract_token(
    authorization: Option<String>,
    x_api_key: Option<String>,
    x_goog_api_key: Option<String>,
    query_key: Option<String>,
    websocket_protocols: Option<String>,
) -> Option<String> {
    if let Some(raw) = authorization {
        let trimmed = raw.trim();
        let mut parts = trimmed.splitn(2, |character: char| character.is_ascii_whitespace());
        let scheme = parts.next().unwrap_or_default();

        // 大小写不敏感：真实客户端里 `bearer`、`Bearer`、`BEARER` 都出现过。
        if scheme.eq_ignore_ascii_case("bearer") {
            if let Some(rest) = parts.next() {
                let token = rest.trim();
                if !token.is_empty() {
                    return Some(token.to_owned());
                }
            }
        } else if !trimmed.is_empty() {
            // 没有 Bearer 前缀的裸令牌也收：不少简易客户端就这么发。
            return Some(trimmed.to_owned());
        }
    }
    for value in [x_api_key, x_goog_api_key, query_key].into_iter().flatten() {
        let token = value.trim();
        if !token.is_empty() {
            return Some(token.to_owned());
        }
    }
    if let Some(protocols) = websocket_protocols {
        for protocol in protocols.split(',').map(str::trim) {
            if let Some(token) = protocol.strip_prefix("openai-insecure-api-key.")
                && !token.is_empty()
            {
                return Some(token.to_owned());
            }
        }
    }
    None
}

/// 从 `application/x-www-form-urlencoded` 查询串取出 Gemini 的 `key`。
fn query_api_key(query: Option<&str>) -> Option<String> {
    let query = query?;
    url::form_urlencoded::parse(query.as_bytes())
        .find(|(name, _)| name == "key")
        .map(|(_, value)| value.into_owned())
        .filter(|value| !value.is_empty())
}

fn header_string(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// 网关鉴权。
///
/// `protocol` 决定失败时错误体的形状 —— 401 也要让客户端 SDK 能读懂。
pub async fn require_gateway(
    state: &AppState,
    headers: &HeaderMap,
    query: Option<&str>,
    protocol: Protocol,
) -> Result<Principal, AppError> {
    let token = extract_token(
        header_string(headers, "authorization"),
        header_string(headers, "x-api-key"),
        header_string(headers, "x-goog-api-key"),
        query_api_key(query),
        header_string(headers, "sec-websocket-protocol"),
    );
    state
        .authenticator()
        .authenticate(token.as_deref())
        .await
        .map_err(|error| AppError::Protocol(ProtocolRejection::new(error, protocol)))
}

/// 管理面防爆破守卫：按客户端 IP 记录连续失败次数与封禁截止时间。
/// 5 次失败锁定 60 秒（期间直接 403）；成功清零。
#[derive(Debug, Default)]
pub struct AdminGuard {
    failures: Mutex<HashMap<IpAddr, (u32, Instant)>>,
}

impl AdminGuard {
    /// 创建空的管理面防爆破守卫。
    pub fn new() -> Self {
        Self::default()
    }

    fn lock_failures(&self) -> MutexGuard<'_, HashMap<IpAddr, (u32, Instant)>> {
        self.failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 检查该 IP 是否处于封禁状态。封禁中返回剩余秒数。
    pub fn check_locked(&self, ip: IpAddr) -> Option<u64> {
        let now = Instant::now();
        let mut guard = self.lock_failures();
        let (failures, until) = guard.get(&ip).copied()?;
        if failures >= 5 {
            if until > now {
                return Some((until - now).as_secs().max(1));
            }
            guard.remove(&ip);
        }
        None
    }

    /// 记录一次鉴权失败，返回是否触发封禁。
    pub fn record_failure(&self, ip: IpAddr) -> bool {
        let now = Instant::now();
        let mut guard = self.lock_failures();
        let entry = guard.entry(ip).or_insert((0, now));
        entry.0 = entry.0.saturating_add(1);
        if entry.0 >= 5 {
            entry.1 = now + std::time::Duration::from_secs(60);
            true
        } else {
            false
        }
    }

    /// 成功鉴权后清零该 IP 的失败记录。
    pub fn record_success(&self, ip: IpAddr) {
        self.lock_failures().remove(&ip);
    }
}

fn peer_ip(peer: Option<SocketAddr>) -> IpAddr {
    peer.map(|addr| addr.ip())
        .filter(|ip| !ip.is_unspecified())
        .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST))
}

/// 通过管理面/自助面鉴权后的用户身份。
///
/// `user_id == None` 表示走的是「管理令牌直登」紧急通道，语义上等同 bootstrap admin。
#[derive(Debug, Clone)]
pub struct AuthUser {
    /// 用户 ID；令牌直登时为 None。
    pub user_id: Option<i64>,
    /// 邮箱；令牌直登时为管理员账号邮箱。
    pub email: String,
    /// 角色。
    pub role: refract_store::UserRole,
    /// 账号状态（令牌直登视为 Active）。
    pub status: refract_store::UserStatus,
}

impl AuthUser {
    /// 是否管理员。
    pub fn is_admin(&self) -> bool {
        self.role == refract_store::UserRole::Admin
    }

    /// 自助面用的用户 ID。令牌直登时回落到 bootstrap admin 的 ID。
    pub fn effective_user_id(&self, state: &AppState) -> i64 {
        self.user_id.unwrap_or_else(|| state.bootstrap_admin_id())
    }
}

/// 按「IP + 账号」二元组计数的登录防爆破守卫。
///
/// 规则与 IP 版一致：5 次失败锁定 60 秒，成功清零。键是 `(IpAddr, String)`——
/// 账号键为小写邮箱，或字面量 `"__admin_token__"`（管理令牌通道）。
#[derive(Debug, Default)]
pub struct SubjectGuard {
    failures: Mutex<HashMap<(IpAddr, String), (u32, Instant)>>,
}

impl SubjectGuard {
    /// 创建空守卫。
    pub fn new() -> Self {
        Self::default()
    }

    fn lock_failures(&self) -> MutexGuard<'_, HashMap<(IpAddr, String), (u32, Instant)>> {
        self.failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 封禁中返回剩余秒数。
    pub fn check_locked(&self, ip: IpAddr, subject: &str) -> Option<u64> {
        let key = (ip, subject.to_ascii_lowercase());
        let now = Instant::now();
        let mut guard = self.lock_failures();
        let (failures, until) = guard.get(&key).copied()?;
        if failures >= 5 {
            if until > now {
                return Some((until - now).as_secs().max(1));
            }
            guard.remove(&key);
        }
        None
    }

    /// 记录一次失败，返回是否触发封禁。
    pub fn record_failure(&self, ip: IpAddr, subject: &str) -> bool {
        let key = (ip, subject.to_ascii_lowercase());
        let now = Instant::now();
        let mut guard = self.lock_failures();
        let entry = guard.entry(key).or_insert((0, now));
        entry.0 = entry.0.saturating_add(1);
        if entry.0 >= 5 {
            entry.1 = now + std::time::Duration::from_secs(60);
            true
        } else {
            false
        }
    }

    /// 成功后清零。
    pub fn record_success(&self, ip: IpAddr, subject: &str) {
        let key = (ip, subject.to_ascii_lowercase());
        self.lock_failures().remove(&key);
    }
}

/// 把会话 ticket 解析成当前用户。
///
/// 校验链：ticket 签名 → `users.session_revoked_at`（iat 必须晚于撤销点）→
/// 用户存在且未 disabled。用户状态走 30 秒内存缓存，避免热路径每请求打库。
async fn resolve_session_user(
    state: &AppState,
    ticket: &str,
    legacy_hash: &str,
) -> Result<AuthUser, AppError> {
    let session =
        verify_session_ticket(ticket, state.session_secret(), legacy_hash).ok_or_else(|| {
            AppError::Admin(GatewayError::unauthenticated("invalid or expired session"))
        })?;
    match session.subject {
        SessionSubject::LegacyAdmin => {
            let email = state
                .settings_repo()
                .admin_username()
                .await
                .ok()
                .flatten()
                .unwrap_or_else(|| "admin@localhost".to_owned());
            Ok(AuthUser {
                user_id: None,
                email,
                role: refract_store::UserRole::Admin,
                status: refract_store::UserStatus::Active,
            })
        }
        SessionSubject::User(user_id) => {
            let cached = state
                .resolve_user_cached(user_id)
                .await
                .map_err(|error| AppError::Admin(crate::error::store_to_gateway(error)))?
                .ok_or_else(|| {
                    AppError::Admin(GatewayError::unauthenticated(
                        "session user no longer exists",
                    ))
                })?;
            if let Some(revoked_at) = cached.session_revoked_at
                && session.iat <= revoked_at.timestamp()
            {
                return Err(AppError::Admin(GatewayError::unauthenticated(
                    "session has been revoked",
                )));
            }
            if cached.status == refract_store::UserStatus::Disabled {
                return Err(AppError::Admin(GatewayError::new(
                    ErrorKind::PermissionDenied,
                    "account is disabled",
                )));
            }
            Ok(AuthUser {
                user_id: Some(user_id),
                email: cached.email,
                role: cached.role,
                status: cached.status,
            })
        }
    }
}

/// 解析请求里的用户身份：显式管理令牌（仅管理员）或会话 Cookie。
///
/// 所有 `/api` 路由共用。失败即 401/403。
async fn authenticate_admin_surface(
    state: &AppState,
    headers: &HeaderMap,
    peer: Option<SocketAddr>,
) -> Result<AuthUser, AppError> {
    let client_ip = peer_ip(peer);
    let repo = SettingsRepo::new(state.db().clone());
    let expected: Option<String> = repo
        .get(refract_store::settings_repo::KEY_ADMIN_TOKEN_HASH)
        .await
        .map_err(|error| AppError::Admin(crate::error::store_to_gateway(error)))?;
    let expected_hash = expected.filter(|hash| !hash.trim().is_empty());

    // 1. 显式令牌头（CLI / 脚本 / 紧急通道）。只有哈希配置存在时才认。
    if let Some(expected_hash) = expected_hash.as_deref()
        && let Some(token) = extract_token(
            header_string(headers, "authorization"),
            header_string(headers, "x-admin-token"),
            None,
            None,
            None,
        )
    {
        if constant_time_eq(
            ApiKeyRepo::hash(&token).as_bytes(),
            expected_hash.as_bytes(),
        ) {
            state.admin_guard().record_success(client_ip);
            let email = repo
                .admin_username()
                .await
                .ok()
                .flatten()
                .unwrap_or_else(|| "admin@localhost".to_owned());
            return Ok(AuthUser {
                user_id: None,
                email,
                role: refract_store::UserRole::Admin,
                status: refract_store::UserStatus::Active,
            });
        }
        let _locked = state.admin_guard().record_failure(client_ip);
        if let Some(wait_secs) = state.admin_guard().check_locked(client_ip) {
            let err = GatewayError::new(
                ErrorKind::PermissionDenied,
                format!("too many failed login attempts, locked for {wait_secs}s"),
            )
            .with_retry_after(std::time::Duration::from_secs(wait_secs));
            return Err(AppError::Admin(err));
        }
        return Err(AppError::Admin(GatewayError::new(
            ErrorKind::PermissionDenied,
            "invalid admin token",
        )));
    }

    // 2. HttpOnly Session Cookie（Web 控制台）
    if let Some(cookie_str) = header_string(headers, "cookie")
        && let Some(ticket) = extract_cookie_value(&cookie_str, SESSION_COOKIE_NAME)
    {
        let legacy_hash = expected_hash.as_deref().unwrap_or("");
        return resolve_session_user(state, &ticket, legacy_hash).await;
    }

    // 3. 未配置任何凭据时，管理面显式关闭鉴权：以 bootstrap admin 身份放行。
    if expected_hash.is_none() {
        let email = repo
            .admin_username()
            .await
            .ok()
            .flatten()
            .unwrap_or_else(|| "admin@localhost".to_owned());
        return Ok(AuthUser {
            user_id: None,
            email,
            role: refract_store::UserRole::Admin,
            status: refract_store::UserStatus::Active,
        });
    }

    Err(AppError::Admin(GatewayError::unauthenticated(
        "missing admin token or session",
    )))
}

/// `/api/admin/*` 的鉴权：必须是管理员。
pub async fn require_admin_user(
    state: &AppState,
    headers: &HeaderMap,
    peer: Option<SocketAddr>,
) -> Result<AuthUser, AppError> {
    let user = authenticate_admin_surface(state, headers, peer).await?;
    if !user.is_admin() {
        return Err(AppError::Admin(GatewayError::new(
            ErrorKind::PermissionDenied,
            "admin role required",
        )));
    }
    Ok(user)
}

/// `/api/me/*` 的鉴权：任何已登录用户（user 与 admin 均可）。
pub async fn require_me(
    state: &AppState,
    headers: &HeaderMap,
    peer: Option<SocketAddr>,
) -> Result<AuthUser, AppError> {
    authenticate_admin_surface(state, headers, peer).await
}

/// 管理接口鉴权（只校验，不返回身份）。
///
/// 委托 [`require_admin_user`]；需要身份的处理器应直接调用后者。
pub async fn require_admin(
    state: &AppState,
    headers: &HeaderMap,
    peer: Option<SocketAddr>,
) -> Result<(), AppError> {
    require_admin_user(state, headers, peer).await.map(|_| ())
}

/// 恒时字节比较。
///
/// 比较的双方都是 SHA-256 的十六进制输出，时序侧信道在 hash 域上本就
/// 不可利用（泄漏的是 hash 前缀而非明文）；这里仍然用恒时实现，是让
/// 鉴权路径不依赖「上一层恰好做了 hash」这个前提。长度不等时提前返回
/// 只泄漏长度 —— hash 输出定长，无信息量。
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0_u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use refract_store::{ApiKey, Database, NewApiKey};

    #[test]
    fn constant_time_eq_matches_plain_equality() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }

    async fn authenticator(
        require_auth: bool,
    ) -> (SingleUserAuthenticator, ApiKeyRepo, refract_store::Database) {
        let db = Database::open_in_memory().await.unwrap();
        let repo = ApiKeyRepo::new(db.clone());
        // 免鉴权路径回落到 bootstrap admin 用户。
        let user_repo = refract_store::UserRepo::new(db.clone());
        let admin = user_repo
            .create_first_admin_if_empty("admin@localhost", "argon2-placeholder")
            .await
            .unwrap()
            .expect("bootstrap admin created");
        (
            SingleUserAuthenticator::new(
                repo.clone(),
                refract_store::UserRepo::new(db.clone()),
                refract_store::WalletRepo::new(db.clone()),
                crate::rate::TtlCache::new(std::time::Duration::from_secs(60)),
                require_auth,
                refract_core::DEFAULT_OWNER_ID,
                admin.id,
            ),
            repo,
            db,
        )
    }

    #[test]
    fn bearer_prefix_is_stripped_case_insensitively() {
        for header in ["Bearer sk-1", "bearer sk-1", "BEARER sk-1"] {
            assert_eq!(
                extract_token(Some(header.into()), None, None, None, None).as_deref(),
                Some("sk-1")
            );
        }
    }

    #[test]
    fn bare_token_without_bearer_is_accepted() {
        // 简易客户端常常直接把 key 放进 Authorization。
        assert_eq!(
            extract_token(Some("sk-raw".into()), None, None, None, None).as_deref(),
            Some("sk-raw")
        );
    }

    #[test]
    fn anthropic_header_is_recognized() {
        assert_eq!(
            extract_token(None, Some("sk-ant".into()), None, None, None).as_deref(),
            Some("sk-ant")
        );
    }

    #[test]
    fn google_header_is_recognized() {
        assert_eq!(
            extract_token(None, None, Some("AIza".into()), None, None).as_deref(),
            Some("AIza")
        );
    }

    #[test]
    fn gemini_query_key_is_recognized() {
        // Google 的 SDK 会把 key 放 query string。
        assert_eq!(
            extract_token(None, None, None, Some("AIza-q".into()), None).as_deref(),
            Some("AIza-q")
        );
    }

    #[test]
    fn authorization_wins_over_other_sources() {
        assert_eq!(
            extract_token(
                Some("Bearer primary".into()),
                Some("secondary".into()),
                Some("tertiary".into()),
                Some("quaternary".into()),
                Some("realtime, openai-insecure-api-key.fifth".into())
            )
            .as_deref(),
            Some("primary")
        );
    }

    #[test]
    fn empty_and_whitespace_tokens_are_rejected() {
        assert!(extract_token(Some("Bearer    ".into()), None, None, None, None).is_none());
        assert!(extract_token(Some("   ".into()), None, None, None, None).is_none());
        assert!(extract_token(None, Some("  ".into()), None, None, None).is_none());
        assert!(extract_token(None, None, None, None, None).is_none());
    }

    #[test]
    fn realtime_websocket_subprotocol_key_is_recognized() {
        assert_eq!(
            extract_token(
                None,
                None,
                None,
                None,
                Some("realtime, openai-insecure-api-key.sk-refract-browser".into()),
            )
            .as_deref(),
            Some("sk-refract-browser")
        );
    }

    #[test]
    fn local_principal_can_use_any_model() {
        let principal = Principal::local(refract_core::DEFAULT_OWNER_ID);
        assert!(principal.allows_model("anything"));
        assert_eq!(principal.key_id(), None);
        assert!(principal.has_scope(PrincipalScope::Gateway));
    }

    #[test]
    fn api_key_channel_tags_are_enforced() {
        let channel: Channel = serde_json::from_value(serde_json::json!({
            "name": "private relay",
            "kind": "chat",
            "credential": "upstream-key",
            "endpoints": [{ "protocol": "chat" }],
            "tags": ["private", "fast"]
        }))
        .unwrap();
        let now = chrono::Utc::now();
        let principal = Principal::from_api_key(ApiKey {
            id: 1,
            owner_id: refract_core::DEFAULT_OWNER_ID,
            user_id: None,
            name: "restricted".into(),
            key_prefix: "rk-test".into(),
            enabled: true,
            allowed_models: Vec::new(),
            allowed_tags: vec!["private".into()],
            quota: 0,
            used_quota: 0,
            rpm_limit: 0,
            tpm_limit: 0,
            budget: 0.0,
            used_budget: 0.0,
            note: None,
            expires_at: None,
            last_used_at: None,
            created_at: now,
        });
        assert!(principal.allows_channel(&channel));

        let mut other = channel;
        other.tags = vec!["public".into()];
        assert!(!principal.allows_channel(&other));
        assert!(Principal::local(refract_core::DEFAULT_OWNER_ID).allows_channel(&other));

        other.owner_id += 1;
        assert!(!Principal::local(refract_core::DEFAULT_OWNER_ID).allows_channel(&other));
    }

    #[tokio::test]
    async fn single_user_authenticator_returns_local_principal_when_auth_is_optional() {
        let (authenticator, _, _) = authenticator(false).await;

        let principal = authenticator.authenticate(None).await.unwrap();

        assert_eq!(principal.owner_id, refract_core::DEFAULT_OWNER_ID);
        assert_eq!(principal.key_id(), None);
        assert!(principal.has_scope(PrincipalScope::Gateway));
    }

    #[tokio::test]
    async fn single_user_authenticator_validates_and_attaches_the_api_key() {
        let (authenticator, repo, db) = authenticator(true).await;
        // 新用户默认 0 余额，网关会按 insufficient_balance 拒绝；先给 bootstrap admin 充值。
        let admin = refract_store::UserRepo::new(db.clone())
            .find_by_email("admin@localhost")
            .await
            .unwrap()
            .expect("bootstrap admin exists (created by authenticator fixture)");
        refract_store::WalletRepo::new(db)
            .apply(
                admin.id,
                10.0,
                refract_store::LedgerKind::Topup,
                None,
                "test",
            )
            .await
            .unwrap();
        let (key, plaintext) = repo
            .create(
                refract_core::DEFAULT_OWNER_ID,
                NewApiKey {
                    name: "client".into(),
                    user_id: None,
                    allowed_models: vec!["gpt-4o".into()],
                    allowed_tags: vec!["private".into()],
                    quota: 10,
                    rpm_limit: 0,
                    tpm_limit: 0,
                    budget: 0.0,
                    note: None,
                    expires_at: None,
                },
            )
            .await
            .unwrap();

        let principal = authenticator.authenticate(Some(&plaintext)).await.unwrap();

        assert_eq!(principal.owner_id, refract_core::DEFAULT_OWNER_ID);
        assert_eq!(principal.key_id(), Some(key.id));
        assert!(principal.allows_model("gpt-4o"));
        assert!(!principal.allows_model("claude-sonnet"));
    }

    #[tokio::test]
    async fn single_user_authenticator_rejects_missing_invalid_and_disabled_keys() {
        let (authenticator, repo, _) = authenticator(true).await;
        assert_eq!(
            authenticator.authenticate(None).await.unwrap_err().kind,
            ErrorKind::Unauthenticated
        );
        assert_eq!(
            authenticator
                .authenticate(Some("rk-invalid"))
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Unauthenticated
        );

        let (key, plaintext) = repo
            .create(
                refract_core::DEFAULT_OWNER_ID,
                NewApiKey {
                    name: "disabled".into(),
                    ..NewApiKey::default()
                },
            )
            .await
            .unwrap();
        repo.set_enabled(refract_core::DEFAULT_OWNER_ID, key.id, false)
            .await
            .unwrap();

        assert_eq!(
            authenticator
                .authenticate(Some(&plaintext))
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Unauthenticated
        );
    }

    #[test]
    fn test_session_ticket_lifecycle() {
        let secret = [42u8; 32];

        // 新格式：用户 ticket
        let ticket = create_user_session_ticket(&secret, 42, 3600);
        let session = verify_session_ticket(&ticket, &secret, "ignored").unwrap();
        assert_eq!(session.subject, SessionSubject::User(42));
        // 伪造签名：必须拒绝
        assert!(verify_session_ticket("100.200.42.bad_sig", &secret, "ignored").is_none());
        // 格式非法：必须拒绝
        assert!(verify_session_ticket("invalid", &secret, "ignored").is_none());

        // 旧格式：管理令牌 ticket（紧急通道），令牌哈希变化立即失效。
        let legacy = {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let expiry = now + 3600;
            let msg = format!("legacy_hash:{now}:{expiry}");
            let sig = hex::encode(crate::notify::hmac_sha256(&secret, msg.as_bytes()));
            format!("{now}.{expiry}.{sig}")
        };
        let session = verify_session_ticket(&legacy, &secret, "legacy_hash").unwrap();
        assert_eq!(session.subject, SessionSubject::LegacyAdmin);
        assert!(verify_session_ticket(&legacy, &secret, "new_token_hash_456").is_none());
        assert!(verify_session_ticket("100.200.bad_sig", &secret, "legacy_hash").is_none());
    }

    #[test]
    fn test_extract_cookie_value() {
        let header = "theme=dark; refract_session=abc.def.123; lang=zh-CN";
        assert_eq!(
            extract_cookie_value(header, SESSION_COOKIE_NAME),
            Some("abc.def.123".to_string())
        );
        assert_eq!(extract_cookie_value(header, "non_existent"), None);
    }

    #[test]
    fn forwarded_proto_https_marks_the_request_secure() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-proto", "https,http".parse().unwrap());
        assert!(request_is_https(&headers));

        let mut headers = HeaderMap::new();
        headers.insert("forwarded", "for=1.2.3.4;proto=https".parse().unwrap());
        assert!(request_is_https(&headers));

        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-proto", "http".parse().unwrap());
        assert!(!request_is_https(&headers));
        assert!(!request_is_https(&HeaderMap::new()));
    }

    #[test]
    fn session_cookie_adds_secure_only_when_requested() {
        let plain = session_cookie("ticket", 60, false);
        assert!(plain.contains("HttpOnly"));
        assert!(plain.contains("SameSite=Strict"));
        assert!(!plain.contains("Secure"));

        let https = session_cookie("ticket", 60, true);
        assert!(https.contains("Secure"));
        assert!(https.contains("HttpOnly"));
    }

    #[test]
    fn admin_guard_locks_after_five_failures_without_wiping_counts() {
        let guard = AdminGuard::new();
        let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
        for n in 1..=4 {
            assert!(!guard.record_failure(ip), "failure {n} should not lock");
            assert!(guard.check_locked(ip).is_none());
        }
        assert!(guard.record_failure(ip));
        assert!(guard.check_locked(ip).is_some());
        guard.record_success(ip);
        assert!(guard.check_locked(ip).is_none());
    }
}
