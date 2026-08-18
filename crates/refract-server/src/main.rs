//! Refract 服务器入口。
//!
//! 单二进制部署：加载配置 → 打开数据库 → 装配状态 → 启动 warp → 优雅关闭。
//! 前端产物通过 `rust-embed` 编译进二进制，部署时只需拷贝一个文件。

// warp 的 filter 组合是一棵编译期类型树，整个 API 的路由 or 起来后深度
// 超过了默认的 128 递归上限（rustc 计算 layout 时报错）。
#![recursion_limit = "256"]

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context, Result};
use figment::Figment;
use figment::providers::{Env, Format, Toml};
use refract_api::AppState;
use refract_store::Database;
use refract_upstream::{UpstreamClient, UpstreamClientConfig};
use tracing_subscriber::EnvFilter;

/// mimalloc 替代系统分配器：网关的负载特征是大量小对象的高频分配
/// （每个请求一份 IR、一批 SSE 帧），系统分配器在多线程下的锁竞争
/// 会直接体现在 P99 上。
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// 运行时配置。
///
/// 来源优先级：环境变量 `REFRACT_*` > `refract.toml` > 默认值。
/// 环境变量优先是因为容器部署时改文件比改 env 麻烦得多。
#[derive(Clone, serde::Deserialize)]
#[serde(default)]
struct Config {
    /// 监听地址。默认只听本机 —— 这个网关持有全部上游密钥，
    /// 默认监听 0.0.0.0 会让一次误操作变成密钥泄漏。
    listen: SocketAddr,
    /// SQLite 文件路径。
    database: String,
    /// 是否要求客户端携带网关密钥。
    require_auth: bool,
    /// 启动时设置或轮换管理令牌。只保存哈希，明文不进入日志或数据库。
    admin_token: Option<String>,
    /// 上游请求整体超时（秒）。
    upstream_timeout_secs: u64,
    /// 流式请求的空闲超时（秒）。
    stream_idle_timeout_secs: u64,
    /// 收到关闭信号后等待在途请求排空的上限（秒）。
    shutdown_grace_secs: u64,
    /// 出站代理，形如 `http://host:port`。
    proxy: Option<String>,
    /// 静态加密主密钥（32 字节 base64）。未设置时使用数据库 settings 表记录，或保持明文。
    master_key: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([127, 0, 0, 1], 3939)),
            database: "refract.db".to_owned(),
            require_auth: false,
            admin_token: None,
            upstream_timeout_secs: 300,
            stream_idle_timeout_secs: 120,
            shutdown_grace_secs: 30,
            proxy: None,
            master_key: None,
        }
    }
}

impl Config {
    fn load() -> Result<Self> {
        Figment::new()
            .merge(Toml::file("refract.toml"))
            .merge(Env::prefixed("REFRACT_"))
            .extract()
            .context("failed to load configuration")
    }

    fn upstream(&self) -> UpstreamClientConfig {
        UpstreamClientConfig {
            timeout: Duration::from_secs(self.upstream_timeout_secs),
            stream_idle_timeout: Duration::from_secs(self.stream_idle_timeout_secs),
            proxy: self.proxy.clone(),
            ..UpstreamClientConfig::default()
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let config = Config::load()?;
    tracing::info!(
        listen = %config.listen,
        database = %config.database,
        require_auth = config.require_auth,
        "starting refract"
    );

    let db = Database::open(&config.database)
        .await
        .with_context(|| format!("failed to open database at `{}`", config.database))?;

    let client = UpstreamClient::new(config.upstream())
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("failed to build the upstream HTTP client")?;
    let explicit_master_key = config
        .master_key
        .as_deref()
        .and_then(|s| refract_store::parse_master_key(s).ok());
    if explicit_master_key.is_some() {
        tracing::info!("channel credential encryption enabled (master key from environment)");
    }

    let state = AppState::bootstrap_with_master_key(
        db.clone(),
        client,
        config.require_auth,
        explicit_master_key,
    )
    .await
    .context("failed to load configuration from the database")?;

    if explicit_master_key.is_none() {
        if state.master_key().is_some() {
            tracing::info!(
                "channel credential encryption enabled (master key from database settings)"
            );
        } else {
            tracing::info!("channel credentials stored in plaintext (no master key configured)");
        }
    }
    apply_bootstrap_admin_token(&config, &state).await?;
    enforce_exposure_policy(&config, &state).await?;
    warn_on_empty_config(&state);
    let maintenance = tokio::spawn(log_retention_loop(state.clone()));
    // 自动禁用渠道的定时重测自愈（间隔从设置读取，0 = 关闭）。
    let retest = tokio::spawn(refract_api::notify::auto_retest_loop(state.clone()));
    // 数据库自动备份循环（间隔从设置读取，0 = 关闭）。
    let backup = tokio::spawn(refract_api::backup::auto_backup_loop(state.clone()));
    // 显式配置套接字（允许地址/端口重用），支持开发与重启时瞬时接管监听
    let socket = match config.listen {
        SocketAddr::V4(_) => tokio::net::TcpSocket::new_v4()?,
        SocketAddr::V6(_) => tokio::net::TcpSocket::new_v6()?,
    };
    socket.set_reuseaddr(true)?;
    #[cfg(all(unix, not(target_os = "solaris"), not(target_os = "illumos")))]
    let _ = socket.set_reuseport(true);
    socket
        .bind(config.listen)
        .with_context(|| format!("failed to bind {}", config.listen))?;
    let listener = socket
        .listen(1024)
        .with_context(|| format!("failed to listen on {}", config.listen))?;
    let local = listener.local_addr().unwrap_or(config.listen);

    tracing::info!(address = %local, "refract is listening");

    // 优雅关闭必须有上限：挂着的 SSE 长连接可以合法地存活几分钟，无上限的
    // drain 会让 systemd/docker 在超时后直接 SIGKILL —— 那比我们主动截断更糟。
    let (drain_tx, drain_rx) = tokio::sync::oneshot::channel::<()>();
    let mut server = tokio::spawn(
        warp::serve(refract_api::routes(state))
            .incoming(listener)
            .graceful(async {
                let _ = drain_rx.await;
            })
            .run(),
    );

    shutdown_signal().await;
    let _ = drain_tx.send(());
    let grace = Duration::from_secs(config.shutdown_grace_secs);
    if tokio::time::timeout(grace, &mut server).await.is_err() {
        tracing::warn!(
            grace_secs = config.shutdown_grace_secs,
            "in-flight requests did not drain in time; forcing shutdown"
        );
        server.abort();
        let _ = server.await;
    }

    maintenance.abort();
    let _ = maintenance.await;
    retest.abort();
    let _ = retest.await;
    backup.abort();
    let _ = backup.await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    db.close().await;
    tracing::info!("shutdown complete");
    Ok(())
}

/// 把显式提供的启动令牌写成哈希，支持容器首次启动与声明式轮换。
async fn apply_bootstrap_admin_token(config: &Config, state: &AppState) -> Result<()> {
    let Some(token) = config
        .admin_token
        .as_deref()
        .map(str::trim)
        .filter(|token| !token.is_empty())
    else {
        return Ok(());
    };

    let hash = refract_store::ApiKeyRepo::hash(token);
    state
        .settings_repo()
        .set(refract_store::settings_repo::KEY_ADMIN_TOKEN_HASH, &hash)
        .await
        .context("failed to persist the bootstrap admin token")
}

async fn log_retention_loop(state: AppState) {
    loop {
        let days = state.settings_repo().log_retention_days().await;
        match state.log_repo().prune(days).await {
            Ok(removed) if removed > 0 => {
                tracing::info!(removed, days, "pruned expired request logs");
            }
            Ok(_) => {}
            Err(error) => tracing::warn!(%error, days, "failed to prune expired request logs"),
        }
        tokio::time::sleep(Duration::from_secs(24 * 60 * 60)).await;
    }
}

/// 拒绝把无保护的管理面和推理面暴露到非回环地址。
///
/// 这个服务持有全部上游密钥；`0.0.0.0` 配错一次就可能把管理 API 暴露给整
/// 个局域网。用户仍可远程部署，但必须先设置管理令牌并开启网关 API key。
async fn enforce_exposure_policy(config: &Config, state: &AppState) -> Result<()> {
    if config.listen.ip().is_loopback() {
        return Ok(());
    }

    let admin_token_configured = state
        .settings_repo()
        .get::<String>(refract_store::settings_repo::KEY_ADMIN_TOKEN_HASH)
        .await
        .context("failed to inspect admin authentication settings")?
        .is_some_and(|hash| !hash.trim().is_empty());

    anyhow::ensure!(
        admin_token_configured && config.require_auth,
        "refusing to listen on non-loopback address {} without both an admin token and require_auth=true; start on 127.0.0.1, configure an admin token, and enable gateway authentication first",
        config.listen
    );
    Ok(())
}

/// 日志初始化。
///
/// 默认过滤器把 `hyper`/`sqlx` 的 debug 噪音挡掉，只留我们自己的
/// info —— 否则一次请求会刷出十几行与排障无关的传输层日志。
fn init_tracing() {
    let filter = EnvFilter::try_from_env("REFRACT_LOG")
        .unwrap_or_else(|_| EnvFilter::new("info,hyper=warn,sqlx=warn,reqwest=warn"));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}

/// 空配置时给出可操作的提示。
///
/// 一个没有渠道的网关会对每个请求回 404，用户看到的只是「不工作」。
/// 启动时说清楚下一步该做什么，比让他去翻文档强。
fn warn_on_empty_config(state: &AppState) {
    let channels = state.channels();
    if channels.is_empty() {
        tracing::warn!(
            "no channels configured — open the web UI and add one, \
             or POST /api/channels"
        );
        return;
    }

    let enabled = channels.iter().filter(|c| c.enabled).count();
    let models = state.planner().visible_models(channels.iter()).len();
    tracing::info!(
        channels = channels.len(),
        enabled,
        models,
        "configuration loaded"
    );

    if enabled == 0 {
        tracing::warn!("every channel is disabled — no request can be routed");
    }
}

/// 关闭信号：Ctrl-C，以及 Unix 上的 SIGTERM。
///
/// 只听 Ctrl-C 在容器里是不够的：`docker stop` 发的是 SIGTERM，
/// 收不到就会被 10 秒后的 SIGKILL 硬杀，在途请求全部断掉。
async fn shutdown_signal() {
    let ctrl_c = async {
        if tokio::signal::ctrl_c().await.is_err() {
            // 装不上处理器时永远挂起，把关闭权交给另一个信号源。
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => tracing::info!("received ctrl-c, shutting down"),
        () = terminate => tracing::info!("received SIGTERM, shutting down"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_state(require_auth: bool) -> AppState {
        let db = Database::open_in_memory().await.unwrap();
        let client = UpstreamClient::new(Default::default()).unwrap();
        AppState::bootstrap(db, client, require_auth).await.unwrap()
    }

    #[tokio::test]
    async fn loopback_listener_is_safe_by_default() {
        let state = test_state(false).await;
        assert!(
            enforce_exposure_policy(&Config::default(), &state)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn unprotected_non_loopback_listener_is_rejected() {
        let state = test_state(false).await;
        let config = Config {
            listen: SocketAddr::from(([0, 0, 0, 0], 3939)),
            ..Config::default()
        };
        let error = enforce_exposure_policy(&config, &state)
            .await
            .expect_err("an exposed unprotected listener must be refused");
        assert!(error.to_string().contains("refusing to listen"));
    }

    #[tokio::test]
    async fn protected_non_loopback_listener_is_allowed() {
        let state = test_state(true).await;
        state
            .settings_repo()
            .set(
                refract_store::settings_repo::KEY_ADMIN_TOKEN_HASH,
                &"sha256-placeholder",
            )
            .await
            .unwrap();
        let config = Config {
            listen: SocketAddr::from(([0, 0, 0, 0], 3939)),
            require_auth: true,
            ..Config::default()
        };
        assert!(enforce_exposure_policy(&config, &state).await.is_ok());
    }

    #[tokio::test]
    async fn bootstrap_admin_token_enables_a_safe_first_remote_start() {
        let state = test_state(true).await;
        let config = Config {
            listen: SocketAddr::from(([0, 0, 0, 0], 3939)),
            require_auth: true,
            admin_token: Some("declarative-admin-secret".into()),
            ..Config::default()
        };

        apply_bootstrap_admin_token(&config, &state).await.unwrap();

        let stored: String = state
            .settings_repo()
            .get(refract_store::settings_repo::KEY_ADMIN_TOKEN_HASH)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            stored,
            refract_store::ApiKeyRepo::hash("declarative-admin-secret")
        );
        assert!(!stored.contains("declarative-admin-secret"));
        assert!(enforce_exposure_policy(&config, &state).await.is_ok());
    }

    #[tokio::test]
    async fn missing_bootstrap_token_does_not_replace_an_existing_token() {
        let state = test_state(false).await;
        state
            .settings_repo()
            .set(
                refract_store::settings_repo::KEY_ADMIN_TOKEN_HASH,
                &"existing-hash",
            )
            .await
            .unwrap();

        apply_bootstrap_admin_token(&Config::default(), &state)
            .await
            .unwrap();

        let stored: String = state
            .settings_repo()
            .get(refract_store::settings_repo::KEY_ADMIN_TOKEN_HASH)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored, "existing-hash");
    }
}
