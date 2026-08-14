//! 鉴权。
//!
//! 两套独立的凭据，各管一摊：
//! - **网关密钥**（`sk-refract-...`）：客户端调用 `/v1/...` 用，可限模型、限额度。
//! - **管理令牌**：调用 `/api/...` 用，权限是「改配置」。
//!
//! 为什么不用同一套：推理密钥会被写进各种客户端配置文件、粘进聊天窗口、
//! 存进第三方工具。它泄漏的概率远高于管理令牌。如果两者相同，一次泄漏就
//! 意味着攻击者能改渠道配置、看全部日志、导出所有上游密钥。

use std::sync::Arc;

use async_trait::async_trait;
use refract_core::{Channel, ErrorKind, GatewayError, Protocol};
use refract_store::{ApiKey, ApiKeyRepo, SettingsRepo};
use warp::Filter;

use crate::error::{ApiError, ProtocolRejection};
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
            scopes: vec![PrincipalScope::Gateway],
            api_key: None,
        }
    }

    /// 构造由 API 密钥认证的身份。
    pub fn from_api_key(key: ApiKey) -> Self {
        Self {
            owner_id: key.owner_id,
            scopes: vec![PrincipalScope::Gateway],
            api_key: Some(Box::new(key)),
        }
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

/// 当前个人部署使用的单用户认证器。
#[derive(Debug, Clone)]
pub struct SingleUserAuthenticator {
    key_repo: ApiKeyRepo,
    require_auth: bool,
    owner_id: i64,
}

impl SingleUserAuthenticator {
    /// 创建单用户认证器。
    pub fn new(key_repo: ApiKeyRepo, require_auth: bool, owner_id: i64) -> Self {
        Self {
            key_repo,
            require_auth,
            owner_id,
        }
    }
}

#[async_trait]
impl Authenticator for SingleUserAuthenticator {
    async fn authenticate(&self, token: Option<&str>) -> Result<Principal, GatewayError> {
        if !self.require_auth {
            return Ok(Principal::local(self.owner_id));
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

        Ok(Principal::from_api_key(key))
    }
}

/// 从各协议的请求头里提取 bearer/api-key。
///
/// 四家的头不一样，且客户端会用它们**原本**的头来调我们 —— 只认
/// `Authorization` 会让 Anthropic SDK 和 Google SDK 直接 401。
fn extract_token(
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

/// Gemini 允许把密钥放 query string 里。
#[derive(Debug, serde::Deserialize)]
struct KeyQuery {
    key: Option<String>,
}

/// 网关鉴权过滤器。
///
/// `protocol` 决定失败时错误体的形状 —— 401 也要让客户端 SDK 能读懂。
pub fn authenticate(
    authenticator: Arc<dyn Authenticator>,
    protocol: Protocol,
) -> impl Filter<Extract = (Principal,), Error = warp::Rejection> + Clone {
    warp::header::optional::<String>("authorization")
        .and(warp::header::optional::<String>("x-api-key"))
        .and(warp::header::optional::<String>("x-goog-api-key"))
        .and(warp::query::<KeyQuery>())
        .and(warp::header::optional::<String>("sec-websocket-protocol"))
        .and_then(move |auth, xapi, xgoog, q: KeyQuery, protocols| {
            let authenticator = authenticator.clone();
            async move {
                let token = extract_token(auth, xapi, xgoog, q.key, protocols);
                authenticator
                    .authenticate(token.as_deref())
                    .await
                    .map_err(|error| ProtocolRejection::reject(error, protocol))
            }
        })
}

/// 管理接口鉴权。
///
/// 未设置管理令牌时放行 —— 首次启动必须能进去设置它，否则用户被自己锁在外面。
pub fn admin_auth(state: AppState) -> impl Filter<Extract = (), Error = warp::Rejection> + Clone {
    warp::header::optional::<String>("authorization")
        .and(warp::header::optional::<String>("x-admin-token"))
        .and_then(move |auth: Option<String>, x_admin: Option<String>| {
            let state = state.clone();
            async move {
                let repo = SettingsRepo::new(state.db().clone());
                let expected: Option<String> = repo
                    .get(refract_store::settings_repo::KEY_ADMIN_TOKEN_HASH)
                    .await
                    .map_err(|e| {
                        warp::reject::custom(ApiError(crate::error::store_to_gateway(e)))
                    })?;

                let Some(expected_hash) = expected.filter(|h| !h.trim().is_empty()) else {
                    // 尚未设置管理令牌：放行，让用户能完成初始配置。
                    return Ok(());
                };

                let token = extract_token(auth, x_admin, None, None, None).ok_or_else(|| {
                    warp::reject::custom(ApiError(GatewayError::unauthenticated(
                        "missing admin token",
                    )))
                })?;

                if constant_time_eq(
                    refract_store::ApiKeyRepo::hash(&token).as_bytes(),
                    expected_hash.as_bytes(),
                ) {
                    Ok(())
                } else {
                    Err(warp::reject::custom(ApiError(GatewayError::new(
                        ErrorKind::PermissionDenied,
                        "invalid admin token",
                    ))))
                }
            }
        })
        .untuple_one()
}

/// 恒时字节比较。
///
/// 比较的双方都是 SHA-256 的十六进制输出，时序侧信道在 hash 域上本就
/// 不可利用（泄漏的是 hash 前缀而非明文）；这里仍然用恒时实现，是让
/// 鉴权路径不依赖「上一层恰好做了 hash」这个前提。长度不等时提前返回
/// 只泄漏长度 —— hash 输出定长，无信息量。
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
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

    async fn authenticator(require_auth: bool) -> (SingleUserAuthenticator, ApiKeyRepo) {
        let db = Database::open_in_memory().await.unwrap();
        let repo = ApiKeyRepo::new(db);
        (
            SingleUserAuthenticator::new(
                repo.clone(),
                require_auth,
                refract_core::DEFAULT_OWNER_ID,
            ),
            repo,
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
        let (authenticator, _) = authenticator(false).await;

        let principal = authenticator.authenticate(None).await.unwrap();

        assert_eq!(principal.owner_id, refract_core::DEFAULT_OWNER_ID);
        assert_eq!(principal.key_id(), None);
        assert!(principal.has_scope(PrincipalScope::Gateway));
    }

    #[tokio::test]
    async fn single_user_authenticator_validates_and_attaches_the_api_key() {
        let (authenticator, repo) = authenticator(true).await;
        let (key, plaintext) = repo
            .create(
                refract_core::DEFAULT_OWNER_ID,
                NewApiKey {
                    name: "client".into(),
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
        let (authenticator, repo) = authenticator(true).await;
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
}
