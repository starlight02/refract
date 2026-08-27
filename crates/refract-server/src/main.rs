//! Refract 服务器入口。
//!
//! 单二进制部署：加载配置 → 打开数据库 → 装配状态 → 启动 xitca-web → 优雅关闭。
//! 前端产物通过 `rust-embed` 编译进二进制，部署时只需拷贝一个文件。

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine as _;
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
    /// 是否强制重置管理员账号并重新生成初始凭据文件。
    reset_admin: bool,
    /// TLS 证书 PEM。与 `tls_key` 成对出现时启用 HTTPS（HTTP/1.1+HTTP/2）和 HTTP/3。
    tls_cert: Option<String>,
    /// TLS 私钥 PEM。与 `tls_cert` 成对出现时启用 HTTPS 和 HTTP/3。
    tls_key: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([127, 0, 0, 1], 3939)),
            database: "refract.db".to_owned(),
            require_auth: false,
            upstream_timeout_secs: 300,
            stream_idle_timeout_secs: 120,
            shutdown_grace_secs: 30,
            proxy: None,
            master_key: None,
            reset_admin: false,
            tls_cert: None,
            tls_key: None,
        }
    }
}

impl Config {
    fn load() -> Result<Self> {
        let mut config: Self = Figment::new()
            .merge(Toml::file("refract.toml"))
            .merge(Env::prefixed("REFRACT_"))
            .extract()
            .context("failed to load configuration")?;

        // 命令行参数显式指定 `--reset-admin` 优先
        let cli_reset = std::env::args().any(|arg| arg == "--reset-admin");
        if cli_reset {
            config.reset_admin = true;
        }
        Ok(config)
    }
    fn upstream(&self) -> UpstreamClientConfig {
        UpstreamClientConfig {
            timeout: Duration::from_secs(self.upstream_timeout_secs),
            stream_idle_timeout: Duration::from_secs(self.stream_idle_timeout_secs),
            proxy: self.proxy.clone(),
            ..UpstreamClientConfig::default()
        }
    }

    fn tls(&self, addr: SocketAddr) -> Result<Option<refract_api::TlsListen>> {
        match (
            nonempty_path(self.tls_cert.as_deref()),
            nonempty_path(self.tls_key.as_deref()),
        ) {
            (None, None) => Ok(None),
            (Some(cert), Some(key)) => Ok(Some(refract_api::TlsListen {
                addr,
                cert_pem: PathBuf::from(cert),
                key_pem: PathBuf::from(key),
            })),
            _ => anyhow::bail!("tls_cert and tls_key must both be set to enable HTTPS/HTTP/3"),
        }
    }
}

fn main() -> Result<()> {
    init_tracing();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to build tokio runtime")?;

    let (config, db, state) = rt.block_on(async {
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
                tracing::info!(
                    "channel credentials stored in plaintext (no master key configured)"
                );
            }
        }
        apply_bootstrap_admin_token(&config, &state).await?;
        enforce_exposure_policy(&config, &state).await?;
        warn_on_empty_config(&state);
        Ok::<_, anyhow::Error>((config, db, state))
    })?;

    let maintenance = rt.spawn(log_retention_loop(state.clone()));
    // 自动禁用渠道的定时重测自愈（间隔从设置读取，0 = 关闭）。
    let retest = rt.spawn(refract_api::notify::auto_retest_loop(state.clone()));
    // 数据库自动备份循环（间隔从设置读取，0 = 关闭）。
    let backup = rt.spawn(refract_api::backup::auto_backup_loop(state.clone()));

    // 显式配置套接字（允许地址/端口重用），支持开发与重启时瞬时接管监听
    let (std_listener, local) = rt.block_on(async {
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
        let std_listener = listener
            .into_std()
            .context("failed to convert listener to std")?;
        std_listener
            .set_nonblocking(true)
            .context("failed to set listener nonblocking")?;
        Ok::<_, anyhow::Error>((std_listener, local))
    })?;

    tracing::info!(address = %local, "refract is listening");

    let tls = config.tls(local)?;
    if tls.is_some() {
        tracing::info!(address = %local, "HTTPS (HTTP/1.1+HTTP/2) and HTTP/3 enabled");
    }

    // 优雅关闭必须有上限：挂着的 SSE 长连接可以合法地存活几分钟，无上限的
    // drain 会让 systemd/docker 在超时后直接 SIGKILL —— 那比我们主动截断更糟。
    let (handle, wait) = refract_api::start_server(state, std_listener, tls)
        .context("failed to start xitca-server")?;
    let force = handle.clone();
    let grace = Duration::from_secs(config.shutdown_grace_secs);
    let (drained_tx, drained_rx) = tokio::sync::oneshot::channel::<()>();
    rt.spawn(async move {
        shutdown_signal().await;
        handle.stop(true);
        tokio::select! {
            _ = drained_rx => {}
            () = tokio::time::sleep(grace) => {
                tracing::warn!(
                    grace_secs = grace.as_secs(),
                    "in-flight requests did not drain in time; forcing shutdown"
                );
                force.stop(false);
            }
        }
    });

    wait().context("xitca-server exited with error")?;
    let _ = drained_tx.send(());

    maintenance.abort();
    retest.abort();
    backup.abort();
    rt.block_on(async {
        let _ = maintenance.await;
        let _ = retest.await;
        let _ = backup.await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        db.close().await;
    });
    tracing::info!("shutdown complete");
    Ok(())
}

#[cfg(unix)]
fn write_owner_only_file(path: &Path, content: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true).mode(0o600);
    let mut file = options.open(path)?;
    file.write_all(content.as_bytes())?;
    file.flush()?;
    Ok(())
}

fn nonempty_path(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

#[cfg(not(unix))]
fn write_owner_only_file(path: &Path, content: &str) -> std::io::Result<()> {
    std::fs::write(path, content)
}

/// 首次启动或 `--reset-admin` 时签发管理令牌。
///
/// 1. 生成 `admin@localhost` 与高熵随机 `adm_…`。
/// 2. 哈希写入 settings，并置 `auth.initialized = true`。
/// 3. 明文写入数据目录 `.admin_token`（0600）。
/// 4. 10 分钟后删除该文件。
/// 5. 已初始化且未 `--reset-admin` 时不改库，只清残留文件。
///
/// 不接受环境变量或 toml 覆盖：能改启动配置的人已经能读库。
async fn apply_bootstrap_admin_token(config: &Config, state: &AppState) -> Result<()> {
    let settings = state.settings_repo();
    let data_dir = Path::new(&config.database)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let token_file_path = data_dir.join(".admin_token");

    let is_initialized = settings
        .get::<bool>(refract_store::settings_repo::KEY_AUTH_INITIALIZED)
        .await
        .unwrap_or(None)
        .unwrap_or(false);

    if is_initialized && !config.reset_admin {
        if token_file_path.exists() {
            let _ = tokio::fs::remove_file(&token_file_path).await;
        }
        return Ok(());
    }

    let random_bytes: [u8; 32] = {
        use rand::RngExt as _;
        rand::rng().random()
    };
    let admin_token = format!(
        "adm_{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random_bytes)
    );

    let hash = refract_store::ApiKeyRepo::hash(&admin_token);
    settings
        .bootstrap_admin(&hash, "admin@localhost")
        .await
        .context("failed to persist bootstrap admin credentials")?;

    let now = chrono::Utc::now();
    let expires_at = now + chrono::Duration::minutes(10);
    let content = format!(
        "# Refract Initial Bootstrap Admin Credentials\n\
         # Generated at: {}\n\
         # Expires at:   {}\n\
         # NOTICE: This file is restricted to 0600 permissions and will be automatically deleted in 10 minutes (TTL).\n\n\
         username=admin@localhost\n\
         admin_token={}\n",
        now.to_rfc3339(),
        expires_at.to_rfc3339(),
        admin_token
    );

    write_owner_only_file(&token_file_path, &content).with_context(|| {
        format!(
            "failed to write bootstrap token file to `{}`",
            token_file_path.display()
        )
    })?;

    println!(
        "\n\
        ╔══════════════════════════════════════════════════════════════════════════════════════╗\n\
        ║ Refract Initial Bootstrap Admin Credentials Generated                                ║\n\
        ║                                                                                      ║\n\
        ║   Default Account: admin@localhost                                                   ║\n\
        ║   Token File:      {:<69} ║\n\
        ║   File Mode:       0600 (Owner read/write only)                                      ║\n\
        ║   Valid For:       10 minutes (file will self-destruct after timeout)                ║\n\
        ╚══════════════════════════════════════════════════════════════════════════════════════╝\n",
        token_file_path.display()
    );
    tracing::info!(
        account = "admin@localhost",
        path = %token_file_path.display(),
        ttl_minutes = 10,
        "bootstrap admin credentials generated and written to hidden file (0600 permissions)"
    );

    let cleanup_path = token_file_path.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(600)).await;
        if let Err(e) = tokio::fs::remove_file(&cleanup_path).await {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(path = %cleanup_path.display(), %e, "failed to delete expired .admin_token file");
            }
        } else {
            tracing::info!(path = %cleanup_path.display(), "bootstrap .admin_token file has been automatically removed (10m TTL expired)");
        }
    });

    Ok(())
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
/// 个局域网。远程部署必须保留首次签发的管理令牌，并打开网关 API key。
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
        "refusing to listen on non-loopback address {} without both a management token and require_auth=true; start on 127.0.0.1 first and do not clear the issued admin token",
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
    async fn issued_token_allows_non_loopback_when_require_auth() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("refract.db");
        let db = Database::open(db_path.to_str().unwrap()).await.unwrap();
        let client = UpstreamClient::new(Default::default()).unwrap();
        let state = AppState::bootstrap(db, client, true).await.unwrap();
        let config = Config {
            listen: SocketAddr::from(([0, 0, 0, 0], 3939)),
            require_auth: true,
            database: db_path.to_str().unwrap().to_owned(),
            ..Config::default()
        };

        apply_bootstrap_admin_token(&config, &state).await.unwrap();

        let stored: String = state
            .settings_repo()
            .get(refract_store::settings_repo::KEY_ADMIN_TOKEN_HASH)
            .await
            .unwrap()
            .unwrap();
        assert!(!stored.is_empty());
        assert!(
            !stored.starts_with("adm_"),
            "store must keep the hash, not the plaintext"
        );
        let content = std::fs::read_to_string(temp_dir.path().join(".admin_token")).unwrap();
        assert!(content.contains("admin_token=adm_"));
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
        state
            .settings_repo()
            .set(refract_store::settings_repo::KEY_AUTH_INITIALIZED, &true)
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

    #[tokio::test]
    async fn first_run_generates_admin_localhost_and_hidden_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("refract.db");
        let db = Database::open(db_path.to_str().unwrap()).await.unwrap();
        let client = UpstreamClient::new(Default::default()).unwrap();
        let state = AppState::bootstrap(db, client, false).await.unwrap();

        let config = Config {
            database: db_path.to_str().unwrap().to_owned(),
            ..Config::default()
        };

        // 首次运行：自动生成默认账号与 .admin_token 隐藏文件
        apply_bootstrap_admin_token(&config, &state).await.unwrap();

        let token_file = temp_dir.path().join(".admin_token");
        assert!(
            token_file.exists(),
            ".admin_token hidden file must be created on first run"
        );

        let content = std::fs::read_to_string(&token_file).unwrap();
        assert!(content.contains("username=admin@localhost"));
        assert!(content.contains("admin_token=adm_"));

        let username: String = state
            .settings_repo()
            .get(refract_store::settings_repo::KEY_ADMIN_USERNAME)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(username, "admin@localhost");

        let is_init: bool = state
            .settings_repo()
            .get(refract_store::settings_repo::KEY_AUTH_INITIALIZED)
            .await
            .unwrap()
            .unwrap();
        assert!(is_init);

        // 二次运行：不应重新生成，且遗留文件被清理
        apply_bootstrap_admin_token(&config, &state).await.unwrap();
        assert!(
            !token_file.exists(),
            "subsequent run must clean up any remaining .admin_token file"
        );

        // 强制 --reset-admin：应重新生成新 token
        let reset_config = Config {
            database: db_path.to_str().unwrap().to_owned(),
            reset_admin: true,
            ..Config::default()
        };
        apply_bootstrap_admin_token(&reset_config, &state)
            .await
            .unwrap();
        assert!(
            token_file.exists(),
            "reset_admin must recreate .admin_token"
        );
    }

    #[test]
    fn tls_is_off_without_paths() {
        let addr = SocketAddr::from(([127, 0, 0, 1], 3939));
        assert!(Config::default().tls(addr).unwrap().is_none());
    }

    #[test]
    fn tls_rejects_cert_without_key() {
        let addr = SocketAddr::from(([127, 0, 0, 1], 3939));
        let config = Config {
            tls_cert: Some("cert.pem".into()),
            ..Config::default()
        };
        let error = config.tls(addr).expect_err("cert without key");
        assert!(error.to_string().contains("tls_cert and tls_key"));
    }

    #[test]
    fn tls_rejects_key_without_cert() {
        let addr = SocketAddr::from(([127, 0, 0, 1], 3939));
        let config = Config {
            tls_key: Some("key.pem".into()),
            ..Config::default()
        };
        let error = config.tls(addr).expect_err("key without cert");
        assert!(error.to_string().contains("tls_cert and tls_key"));
    }

    #[test]
    fn tls_treats_blank_paths_as_disabled() {
        let addr = SocketAddr::from(([127, 0, 0, 1], 3939));
        let config = Config {
            tls_cert: Some("  ".into()),
            tls_key: Some("".into()),
            ..Config::default()
        };
        assert!(config.tls(addr).unwrap().is_none());
    }

    #[test]
    fn tls_binds_same_addr_when_both_paths_set() {
        let addr = SocketAddr::from(([127, 0, 0, 1], 3939));
        let config = Config {
            tls_cert: Some("/data/cert.pem".into()),
            tls_key: Some("/data/key.pem".into()),
            ..Config::default()
        };
        let tls = config.tls(addr).unwrap().expect("enabled");
        assert_eq!(tls.addr, addr);
        assert_eq!(tls.cert_pem.as_os_str(), "/data/cert.pem");
        assert_eq!(tls.key_pem.as_os_str(), "/data/key.pem");
    }
}
