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
        let channels = ChannelRepo::new(db.clone())
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
        Ok(Self {
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
            }),
        })
    }

    /// 进程内指标计数器。
    pub fn metrics(&self) -> &crate::metrics::GatewayMetrics {
        &self.inner.metrics
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

    /// 渠道仓储。
    pub fn channel_repo(&self) -> ChannelRepo {
        ChannelRepo::new(self.inner.db.clone())
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
        RouteExecutor::new(
            self.inner.client.clone(),
            self.inner.codecs,
            self.health_repo(),
            refract_router::RouterConfig::default(),
        )
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
            address: UpstreamAddress::default(),
            endpoints: vec![ep],
            tags: Vec::new(),
            timeout_secs: 0,
            proxy: None,
            param_override: None,
            note: None,
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
