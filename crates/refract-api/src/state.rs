//! 应用状态 —— 所有 HTTP 处理器共享的依赖。
//!
//! 设计要点：**渠道配置常驻内存，用 `ArcSwap` 原子替换**。
//!
//! 理由：路由发生在每个请求的热路径上，而渠道配置几分钟才改一次。每次请求
//! 去查一遍 SQLite 是纯粹的浪费 —— 一次路由要遍历所有渠道的所有端点，走
//! 数据库意味着几十次查询。`ArcSwap` 让读端完全无锁（一次原子加载），写端
//! 整体替换快照。
//!
//! 代价是「改配置后要显式刷新」。这个代价是可控的：所有写路径都在
//! [`AppState::reload_channels`] 之后，忘不掉。

use std::sync::Arc;

use arc_swap::ArcSwap;
use refract_core::{Channel, RoutingPolicy};
use refract_protocol::codec::CodecSet;
use refract_router::{RouteExecutor, RoutePlanner};
use refract_store::{
    ApiKeyRepo, ChannelRepo, Database, HealthRepo, LogRepo, SettingsRepo, StoreError,
};
use refract_upstream::UpstreamClient;
use warp::Filter;

use crate::auth::{Authenticator, SingleUserAuthenticator};

/// 网关运行时状态。克隆是浅拷贝。
#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

struct Inner {
    db: Database,
    channels: ArcSwap<Vec<Channel>>,
    policy: ArcSwap<RoutingPolicy>,
    codecs: CodecSet,
    client: UpstreamClient,
    authenticator: Arc<dyn Authenticator>,
    /// 跨请求共享的 round-robin 按模型游标表。
    route_cursors: refract_router::RoundRobinCursors,
    /// 进程内 Prometheus 计数器。
    metrics: crate::metrics::GatewayMetrics,
    /// 共享的健康仓储 —— 它内部有熔断内存缓存，必须全进程同一份，
    /// 否则管理端解除熔断后路由还在用旧缓存。
    health: HealthRepo,
    /// 每密钥速率限制的分钟窗口。
    rate_limiter: crate::rate::RateLimiter,
    /// 模型价表快照。管理端更新后热替换。
    pricing: ArcSwap<Vec<refract_store::ModelPrice>>,
    /// 是否把请求/响应正文写进日志。热路径每请求读一次，用原子布尔。
    capture_bodies: std::sync::atomic::AtomicBool,
    /// 路由事件出口（自动禁用 + webhook 通知的输入源）。
    events: refract_router::EventSender,
    /// 告警 webhook 地址快照。空 = 不通知。
    webhook_url: ArcSwap<Option<String>>,
    /// 网关级全局限制快照。
    global_limits: ArcSwap<refract_store::GlobalLimits>,
    /// HTTP 200 空回复重试全局策略快照。
    empty_response_retry: ArcSwap<refract_core::EmptyResponseRetryPolicy>,
    /// 渠道亲和性引擎：会话级绑定的内存缓存 + 规则编译。
    affinity: refract_router::AffinityEngine,
    /// 多密钥调度器：轮询游标与黏性绑定，与执行器共享同一份。
    keys: refract_router::KeySelector,
    /// 并发上限信号量。`None` = 不限。改上限时整体重建 ——
    /// 旧 permit 归还旧信号量，新请求走新的，无需迁移。
    concurrency: ArcSwap<Option<Arc<tokio::sync::Semaphore>>>,
    ip_limits: ArcSwap<refract_store::IpLimits>,
    ip_limiter: crate::rate::IpRateLimiter,
    webhook_secret: ArcSwap<Option<String>>,
    backup_settings: ArcSwap<refract_store::BackupSettings>,
    master_key: ArcSwap<Option<[u8; 32]>>,
    admin_guard: crate::auth::AdminGuard,
    transport_crypto: crate::crypto::TransportCrypto,
}

/// 由限制值构造并发信号量。0 = 不限（None）。
fn build_semaphore(limits: &refract_store::GlobalLimits) -> Option<Arc<tokio::sync::Semaphore>> {
    (limits.max_concurrency > 0)
        .then(|| Arc::new(tokio::sync::Semaphore::new(limits.max_concurrency as usize)))
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("channels", &self.channels().len())
            .field("authenticator", &self.inner.authenticator)
            .finish_non_exhaustive()
    }
}

impl AppState {
    /// 从数据库装配状态，并载入渠道快照。
    pub async fn bootstrap(
        db: Database,
        client: UpstreamClient,
        require_auth: bool,
    ) -> Result<Self, StoreError> {
        Self::bootstrap_with_master_key(db, client, require_auth, None).await
    }

    /// 从数据库装配状态，可选传入显式主密钥（如来自环境变量 `REFRACT_MASTER_KEY`）。
    pub async fn bootstrap_with_master_key(
        db: Database,
        client: UpstreamClient,
        require_auth: bool,
        explicit_master_key: Option<[u8; 32]>,
    ) -> Result<Self, StoreError> {
        let master_key = match explicit_master_key {
            Some(key) => Some(key),
            None => {
                if let Ok(Some(key_str)) = SettingsRepo::new(db.clone()).master_key().await {
                    refract_store::parse_master_key(&key_str).ok()
                } else {
                    None
                }
            }
        };
        let channels = ChannelRepo::new(db.clone())
            .with_master_key(master_key)
            .list(refract_core::DEFAULT_OWNER_ID)
            .await?;
        let policy = SettingsRepo::new(db.clone()).routing_policy().await?;

        let authenticator = Arc::new(SingleUserAuthenticator::new(
            ApiKeyRepo::new(db.clone()),
            require_auth,
            refract_core::DEFAULT_OWNER_ID,
        ));
        // 熔断缓存从库里预热：重启不能忘记「哪个端点还在熔断中」。
        // 熔断策略同样来自 settings —— 用户调过的阈值重启后必须还在。
        let breaker = SettingsRepo::new(db.clone()).breaker_policy().await?;
        let health = HealthRepo::with_policy(db.clone(), breaker);
        health.warm_cache().await?;
        let pricing = SettingsRepo::new(db.clone()).pricing().await?;
        let capture_bodies = SettingsRepo::new(db.clone()).capture_bodies().await?;
        let webhook_url = SettingsRepo::new(db.clone()).webhook_url().await?;
        let global_limits = SettingsRepo::new(db.clone()).global_limits().await?;
        let empty_response_retry = SettingsRepo::new(db.clone()).empty_response_retry().await?;
        let concurrency = build_semaphore(&global_limits);
        let ip_limits = SettingsRepo::new(db.clone()).ip_limits().await?;
        let webhook_secret = SettingsRepo::new(db.clone()).webhook_secret().await?;
        let backup_settings = SettingsRepo::new(db.clone()).backup_settings().await?;
        let (events, receiver) = tokio::sync::mpsc::unbounded_channel();
        let affinity_settings = SettingsRepo::new(db.clone()).affinity().await?;
        let affinity_engine = refract_router::AffinityEngine::new();
        affinity_engine.load(affinity_settings);
        let keys = refract_router::KeySelector::new();
        let state = Self {
            inner: Arc::new(Inner {
                db,
                channels: ArcSwap::from_pointee(channels),
                policy: ArcSwap::from_pointee(policy),
                codecs: CodecSet::builtin(),
                client,
                authenticator,
                route_cursors: refract_router::RoundRobinCursors::default(),
                metrics: crate::metrics::GatewayMetrics::default(),
                health,
                rate_limiter: crate::rate::RateLimiter::new(),
                pricing: ArcSwap::from_pointee(pricing),
                capture_bodies: std::sync::atomic::AtomicBool::new(capture_bodies),
                events,
                webhook_url: ArcSwap::from_pointee(webhook_url),
                global_limits: ArcSwap::from_pointee(global_limits),
                empty_response_retry: ArcSwap::from_pointee(empty_response_retry),
                affinity: affinity_engine,
                keys,
                concurrency: ArcSwap::from_pointee(concurrency),
                ip_limits: ArcSwap::from_pointee(ip_limits),
                ip_limiter: crate::rate::IpRateLimiter::new(),
                webhook_secret: ArcSwap::from_pointee(webhook_secret),
                backup_settings: ArcSwap::from_pointee(backup_settings),
                master_key: ArcSwap::from_pointee(master_key),
                admin_guard: crate::auth::AdminGuard::new(),
                transport_crypto: crate::crypto::TransportCrypto::new_random(),
            }),
        };
        crate::notify::spawn_event_worker(state.clone(), receiver);
        Ok(state)
    }

    /// 传输层端到端加密管理器。
    pub fn transport_crypto(&self) -> &crate::crypto::TransportCrypto {
        &self.inner.transport_crypto
    }

    /// 当前全局限制快照。
    pub fn global_limits(&self) -> refract_store::GlobalLimits {
        **self.inner.global_limits.load()
    }

    /// 并发上限信号量。
    pub fn concurrency_semaphore(&self) -> Option<Arc<tokio::sync::Semaphore>> {
        self.inner.concurrency.load().as_ref().clone()
    }

    /// 从库里重读全局限制并重建并发信号量。
    pub async fn reload_global_limits(&self) -> Result<(), StoreError> {
        let limits = self.settings_repo().global_limits().await?;
        self.inner
            .concurrency
            .store(Arc::new(build_semaphore(&limits)));
        self.inner.global_limits.store(Arc::new(limits));
        Ok(())
    }

    /// 当前 HTTP 200 空回复重试全局策略。
    pub fn empty_response_retry(&self) -> refract_core::EmptyResponseRetryPolicy {
        **self.inner.empty_response_retry.load()
    }

    /// 从库里重读空回复重试策略。
    pub async fn reload_empty_response_retry(&self) -> Result<(), StoreError> {
        let policy = self.settings_repo().empty_response_retry().await?;
        self.inner.empty_response_retry.store(Arc::new(policy));
        Ok(())
    }

    /// 发出一条路由事件（供不经 executor 的路径使用，如流式终态记录）。
    pub fn emit_router_event(&self, event: refract_router::RouterEvent) {
        let _ = self.inner.events.send(event);
    }

    /// 当前告警 webhook 地址。
    pub fn webhook_url(&self) -> Option<String> {
        self.inner.webhook_url.load().as_ref().clone()
    }

    /// 从库里重读 webhook 地址。管理端保存后调用。
    pub async fn reload_webhook(&self) -> Result<(), StoreError> {
        let url = self.settings_repo().webhook_url().await?;
        self.inner.webhook_url.store(Arc::new(url));
        Ok(())
    }

    /// 是否记录请求/响应正文快照。
    pub fn capture_bodies(&self) -> bool {
        self.inner
            .capture_bodies
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 从库里重读正文快照开关。管理端保存后调用。
    pub async fn reload_capture_bodies(&self) -> Result<(), StoreError> {
        let enabled = SettingsRepo::new(self.inner.db.clone())
            .capture_bodies()
            .await?;
        self.inner
            .capture_bodies
            .store(enabled, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    /// 从库里重读价表并热替换。管理端保存后调用。
    pub async fn reload_pricing(&self) -> Result<(), StoreError> {
        let pricing = SettingsRepo::new(self.inner.db.clone()).pricing().await?;
        self.inner.pricing.store(Arc::new(pricing));
        Ok(())
    }

    /// 按当前价表计算一次请求的成本。没有匹配规则时为 0。
    pub fn cost_for(
        &self,
        model: &str,
        input_tokens: u64,
        output_tokens: u64,
        cached_tokens: u64,
        cache_write_tokens: u64,
    ) -> f64 {
        let pricing = self.inner.pricing.load();
        refract_store::price_for(&pricing, model)
            .map(|price| {
                price.cost(
                    input_tokens,
                    output_tokens,
                    cached_tokens,
                    cache_write_tokens,
                )
            })
            .unwrap_or(0.0)
    }

    /// 进程内指标计数器。
    pub fn metrics(&self) -> &crate::metrics::GatewayMetrics {
        &self.inner.metrics
    }

    /// 每密钥速率限制器。
    pub fn rate_limiter(&self) -> &crate::rate::RateLimiter {
        &self.inner.rate_limiter
    }

    /// 当前渠道快照。读取是一次原子加载，无锁。
    pub fn channels(&self) -> arc_swap::Guard<Arc<Vec<Channel>>> {
        self.inner.channels.load()
    }

    /// 当前路由策略。
    pub fn policy(&self) -> RoutingPolicy {
        (**self.inner.policy.load()).clone()
    }

    /// 当前网关身份认证器。
    pub fn authenticator(&self) -> Arc<dyn Authenticator> {
        self.inner.authenticator.clone()
    }

    /// 数据库句柄。
    pub fn db(&self) -> &Database {
        &self.inner.db
    }

    /// codec 注册表。
    pub fn codecs(&self) -> CodecSet {
        self.inner.codecs
    }

    /// 上游客户端。
    pub fn upstream(&self) -> &UpstreamClient {
        &self.inner.client
    }

    /// 渠道仓储（自动注入当前主密钥）。
    pub fn channel_repo(&self) -> ChannelRepo {
        ChannelRepo::new(self.inner.db.clone()).with_master_key(self.master_key())
    }

    /// 当前单 IP 限速快照。
    pub fn ip_limits(&self) -> refract_store::IpLimits {
        **self.inner.ip_limits.load()
    }

    /// 单 IP 限速器。
    pub fn ip_limiter(&self) -> &crate::rate::IpRateLimiter {
        &self.inner.ip_limiter
    }

    /// 从库里重读单 IP 限速。
    pub async fn reload_ip_limits(&self) -> Result<(), StoreError> {
        let limits = self.settings_repo().ip_limits().await?;
        self.inner.ip_limits.store(Arc::new(limits));
        Ok(())
    }

    /// 告警 webhook 签名密钥。
    pub fn webhook_secret(&self) -> Option<String> {
        self.inner.webhook_secret.load().as_ref().clone()
    }

    /// 从库里重读 webhook 签名密钥。
    pub async fn reload_webhook_secret(&self) -> Result<(), StoreError> {
        let secret = self.settings_repo().webhook_secret().await?;
        self.inner.webhook_secret.store(Arc::new(secret));
        Ok(())
    }

    /// 自动备份设置。
    pub fn backup_settings(&self) -> refract_store::BackupSettings {
        (**self.inner.backup_settings.load()).clone()
    }

    /// 从库里重读自动备份设置。
    pub async fn reload_backup(&self) -> Result<(), StoreError> {
        let backup = self.settings_repo().backup_settings().await?;
        self.inner.backup_settings.store(Arc::new(backup));
        Ok(())
    }

    /// 当前主加密密钥（32 字节）。
    pub fn master_key(&self) -> Option<[u8; 32]> {
        *self.inner.master_key.load().as_ref()
    }

    /// 从库里重读主加密密钥。
    pub async fn reload_master_key(&self) -> Result<(), StoreError> {
        let key = match self.settings_repo().master_key().await? {
            Some(s) => refract_store::parse_master_key(&s).ok(),
            None => None,
        };
        self.inner.master_key.store(Arc::new(key));
        // 密钥变化后刷新渠道快照（重新加解密）
        self.reload_channels().await?;
        Ok(())
    }

    /// 管理面防爆破守卫。
    pub fn admin_guard(&self) -> &crate::auth::AdminGuard {
        &self.inner.admin_guard
    }

    /// 密钥仓储。
    pub fn key_repo(&self) -> ApiKeyRepo {
        ApiKeyRepo::new(self.inner.db.clone())
    }

    /// 日志仓储。
    pub fn log_repo(&self) -> LogRepo {
        LogRepo::new(self.inner.db.clone())
    }

    /// 设置仓储。
    pub fn settings_repo(&self) -> SettingsRepo {
        SettingsRepo::new(self.inner.db.clone())
    }

    /// 健康仓储。返回共享实例的浅拷贝 —— 熔断内存缓存全进程只有一份。
    pub fn health_repo(&self) -> HealthRepo {
        self.inner.health.clone()
    }

    /// 按当前策略构造规划器。
    pub fn planner(&self) -> RoutePlanner {
        RoutePlanner::with_cursors(self.policy(), self.inner.route_cursors.clone())
    }

    /// 构造执行器。
    pub fn executor(&self) -> RouteExecutor {
        let config = refract_router::RouterConfig {
            empty_response_retry: self.empty_response_retry(),
            ..Default::default()
        };
        RouteExecutor::new(
            self.inner.client.clone(),
            self.inner.codecs,
            self.health_repo(),
            config,
        )
        .with_keys(self.inner.keys.clone())
        .with_events(self.inner.events.clone())
    }

    /// 渠道亲和性引擎（网关热路径与 admin 统计共用）。
    pub fn affinity(&self) -> &refract_router::AffinityEngine {
        &self.inner.affinity
    }

    /// 多密钥调度器（渠道删除时清理游标与黏性绑定）。
    pub fn key_selector(&self) -> &refract_router::KeySelector {
        &self.inner.keys
    }

    /// 渠道被删除时：清掉亲和性绑定与密钥轮询游标。
    pub fn forget_channel(&self, channel_id: refract_core::channel::ChannelId) {
        self.inner.affinity.forget_channel(channel_id);
        self.inner.keys.forget_channel(channel_id);
    }

    /// 从库里重读亲和性设置并重编译规则；已有缓存绑定保留。
    pub async fn reload_affinity(&self) -> Result<(), StoreError> {
        let settings = self.settings_repo().affinity().await?;
        self.inner.affinity.load(settings);
        Ok(())
    }

    /// 从数据库重新载入渠道快照。
    ///
    /// 所有修改渠道的写操作**必须**在提交后调用它，否则路由还在用旧配置。
    pub async fn reload_channels(&self) -> Result<(), StoreError> {
        let channels = self
            .channel_repo()
            .list(refract_core::DEFAULT_OWNER_ID)
            .await?;
        self.inner.channels.store(Arc::new(channels));
        Ok(())
    }

    /// 从数据库重新载入路由策略。
    pub async fn reload_policy(&self) -> Result<(), StoreError> {
        let policy = self.settings_repo().routing_policy().await?;
        self.inner.policy.store(Arc::new(policy));
        Ok(())
    }

    /// 从数据库重新载入熔断策略并热更新到共享的健康仓储。
    pub async fn reload_breaker(&self) -> Result<(), StoreError> {
        let breaker = self.settings_repo().breaker_policy().await?;
        self.inner.health.set_policy(breaker);
        Ok(())
    }
}

/// 把应用状态注入 warp 过滤器链的统一 helper。
pub fn with_state(
    state: AppState,
) -> impl warp::Filter<Extract = (AppState,), Error = std::convert::Infallible> + Clone {
    warp::any().map(move || state.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use refract_core::{
        ChannelEndpoint, ChannelKind, Credential, ModelEntry, Protocol, UpstreamAddress,
    };
    use refract_upstream::UpstreamClientConfig;

    async fn state() -> AppState {
        let db = Database::open_in_memory().await.unwrap();
        let client = UpstreamClient::new(UpstreamClientConfig::default()).unwrap();
        AppState::bootstrap(db, client, false).await.unwrap()
    }

    fn sample_channel() -> Channel {
        let mut ep = ChannelEndpoint::new(Protocol::Chat);
        ep.models = vec![ModelEntry::plain("gpt-4o")];
        Channel {
            id: 0,
            owner_id: refract_core::DEFAULT_OWNER_ID,
            name: "test".into(),
            kind: ChannelKind::Single(Protocol::Chat),
            enabled: true,
            priority: 0,
            weight: 1,
            credential: Credential::new("sk-test"),
            credentials: Vec::new(),
            key_strategy: Default::default(),
            address: UpstreamAddress::default(),
            endpoints: vec![ep],
            tags: Vec::new(),
            timeout_secs: 0,
            proxy: None,
            param_override: None,
            note: None,
            auto_disabled: false,
            balance: None,
            balance_updated_at: None,
            extra_headers: Vec::new(),
            test_model: None,
            empty_response_retry: Default::default(),
        }
    }

    #[tokio::test]
    async fn bootstrap_starts_with_no_channels() {
        let state = state().await;
        assert!(state.channels().is_empty());
    }

    #[tokio::test]
    async fn reload_picks_up_new_channels() {
        let state = state().await;
        state
            .channel_repo()
            .create(&sample_channel())
            .await
            .unwrap();

        // 快照是不可变的：写库之后、reload 之前，路由看到的仍是旧配置。
        assert!(state.channels().is_empty());

        state.reload_channels().await.unwrap();
        assert_eq!(state.channels().len(), 1);
    }

    #[tokio::test]
    async fn policy_reload_reflects_settings() {
        let state = state().await;
        assert!(
            state.policy().native_first,
            "default policy is native-first"
        );

        let mut policy = state.policy();
        policy.native_first = false;
        state
            .settings_repo()
            .set_routing_policy(&policy)
            .await
            .unwrap();
        state.reload_policy().await.unwrap();

        assert!(!state.policy().native_first);
    }

    #[tokio::test]
    async fn planner_uses_the_current_policy() {
        let state = state().await;
        let mut policy = state.policy();
        policy.max_attempts = 7;
        state
            .settings_repo()
            .set_routing_policy(&policy)
            .await
            .unwrap();
        state.reload_policy().await.unwrap();

        assert_eq!(state.planner().policy().max_attempts, 7);
    }
}
