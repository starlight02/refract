//! 运行时设置仓储。
//!
//! 设置以 JSON 形式存在 `settings` 表，读取时反序列化成强类型。
//! 用一张 KV 表而非固定列，是因为设置项会随功能演进增删，加列要迁移，加键不用。

use refract_core::{AffinitySettings, EmptyResponseRetryPolicy, RoutingPolicy};
use serde::{Serialize, de::DeserializeOwned};
use sqlx::Row;

use crate::db::{Database, StoreError};
use crate::health_repo::BreakerPolicy;

/// 路由策略的设置键。
pub const KEY_ROUTING_POLICY: &str = "routing.policy";
/// 熔断策略的设置键。
pub const KEY_BREAKER_POLICY: &str = "routing.breaker";
/// 管理端令牌的设置键。
pub const KEY_ADMIN_TOKEN_HASH: &str = "auth.admin_token_hash";
/// 是否已完成初次初始化认证凭据。
pub const KEY_AUTH_INITIALIZED: &str = "auth.initialized";
/// 默认管理员账号名。
pub const KEY_ADMIN_USERNAME: &str = "auth.admin_username";
/// 日志保留天数。
pub const KEY_LOG_RETENTION_DAYS: &str = "logs.retention_days";
/// 模型价表。
pub const KEY_PRICING: &str = "billing.pricing";
/// 是否把请求/响应正文快照写进请求日志。
pub const KEY_LOG_BODIES: &str = "logs.capture_bodies";
/// 告警 webhook 地址。
pub const KEY_WEBHOOK_URL: &str = "notify.webhook_url";
/// 自动禁用渠道的重测间隔（分钟）。0 表示关闭。
pub const KEY_RETEST_MINUTES: &str = "channels.auto_retest_minutes";
/// 默认重测间隔。
pub const DEFAULT_RETEST_MINUTES: u32 = 30;
/// 全局限制（网关级 RPM、TPM 与并发上限）。
pub const KEY_GLOBAL_LIMITS: &str = "limits.global";
/// HTTP 200 空回复重试策略。
pub const KEY_EMPTY_RESPONSE_RETRY: &str = "upstream.empty_response_retry";
/// 渠道亲和性设置。
pub const KEY_AFFINITY: &str = "affinity.settings";
/// 单 IP 限速(网关级,按客户端 IP 计)。
pub const KEY_IP_LIMITS: &str = "limits.ip";
/// 告警 webhook 的 HMAC 签名密钥。
pub const KEY_WEBHOOK_SECRET: &str = "notify.webhook_secret";
/// 自动备份设置。
pub const KEY_BACKUP: &str = "backup.settings";
/// 凭据静态加密主密钥(base64 编码的 32 字节)。
pub const KEY_MASTER_KEY: &str = "auth.master_key";
/// SMTP 外发地址（`smtp://user:pass@host:587`）。空 = 验证码走 log（dev mode）。
pub const KEY_SMTP_URL: &str = "mail.smtp_url";
/// 邮件发件人地址。
pub const KEY_MAIL_FROM: &str = "mail.from";
/// 是否暴露 per-user Prometheus 指标。默认关闭，避免高基数 label 伤害采集端。
pub const KEY_METRICS_PER_USER: &str = "metrics.per_user";

/// 网关级全局限制。密钥级限速在免鉴权模式下是零防护 ——
/// 跑飞的本地 agent 迴圈会原样打穿上游账单，这层是最后的保险丝。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GlobalLimits {
    /// 每分钟请求数上限。0 = 不限。
    #[serde(default)]
    pub rpm: u32,
    /// 每分钟 token 数上限。0 = 不限。
    ///
    /// 与 RPM 互补：RPM 挡不住「少量请求 × 巨大上下文」。跑飞的 agent 每次
    /// 带 200k 上下文重发时，RPM=60 之内一分钟仍可烧掉 1200 万 token。
    #[serde(default)]
    pub tpm: u32,
    /// 同时在途请求上限。0 = 不限。
    #[serde(default)]
    pub max_concurrency: u32,
}

impl GlobalLimits {
    /// 校验数值范围。
    pub fn validate(&self) -> Result<(), String> {
        if self.rpm > 1_000_000 {
            return Err("global rpm must be at most 1,000,000".into());
        }
        if self.tpm > 1_000_000_000 {
            return Err("global tpm must be at most 1,000,000,000".into());
        }
        if self.max_concurrency > 100_000 {
            return Err("global concurrency must be at most 100,000".into());
        }
        Ok(())
    }
}

/// 单 IP 限速。全局限制挡的是总量,挡不住「单个客户端把配额吃光」——
/// 一个跑飞的脚本可以在全局 RPM 之内独占所有吞吐,这层按 IP 隔离。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IpLimits {
    /// 每个客户端 IP 每分钟请求数上限。0 = 不限。
    #[serde(default)]
    pub rpm: u32,
}

impl IpLimits {
    /// 校验数值范围。
    pub fn validate(&self) -> Result<(), String> {
        if self.rpm > 1_000_000 {
            return Err("per-ip rpm must be at most 1,000,000".into());
        }
        Ok(())
    }
}

/// 自动备份设置。`interval_hours = 0` 表示关闭自动备份。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BackupSettings {
    /// 备份目录;`None` 时用数据库文件旁的 `backups/`。
    #[serde(default)]
    pub directory: Option<String>,
    /// 备份间隔(小时)。0 = 关闭。
    #[serde(default)]
    pub interval_hours: u32,
    /// 保留份数,超出删最旧。
    #[serde(default = "default_backup_keep")]
    pub keep: u32,
}

/// 默认保留份数。
pub fn default_backup_keep() -> u32 {
    8
}

impl Default for BackupSettings {
    fn default() -> Self {
        Self {
            directory: None,
            interval_hours: 0,
            keep: default_backup_keep(),
        }
    }
}

impl BackupSettings {
    /// 校验数值范围。
    pub fn validate(&self) -> Result<(), String> {
        if self.interval_hours > 8760 {
            return Err("backup interval must be at most 8760 hours (one year)".into());
        }
        if !(1..=100).contains(&self.keep) {
            return Err("backup keep must be between 1 and 100".into());
        }
        Ok(())
    }
}
/// 默认日志保留天数。
pub const DEFAULT_LOG_RETENTION_DAYS: u32 = 30;
/// 日志保留范围上限，防止配置溢出 SQLite 日期计算。
pub const MAX_LOG_RETENTION_DAYS: u32 = 3650;

/// 一条模型计价规则。
///
/// `pattern` 支持两种形态：精确模型名，或以 `*` 结尾的前缀通配
/// （如 `gpt-4o*`）。价格单位是「每百万 token」，币种由使用者自定 ——
/// 网关只做乘法，不做汇率。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ModelPrice {
    /// 模型名或前缀通配。
    pub pattern: String,
    /// 每百万输入 token 的价格。
    pub input_per_m: f64,
    /// 每百万输出 token 的价格。
    pub output_per_m: f64,
    /// 每百万缓存命中 token 的价格。缺省按输入价（即不打折）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_per_m: Option<f64>,
    /// 每百万缓存写入 token 的价格（Anthropic 实价约 1.25 倍输入价）。
    /// 缺省按输入价。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_per_m: Option<f64>,
}

impl ModelPrice {
    /// 校验规则的合法性。
    pub fn validate(&self) -> Result<(), String> {
        if self.pattern.trim().is_empty() {
            return Err("pricing pattern must not be empty".into());
        }
        for (label, value) in [
            ("input_per_m", Some(self.input_per_m)),
            ("output_per_m", Some(self.output_per_m)),
            ("cached_input_per_m", self.cached_input_per_m),
            ("cache_write_per_m", self.cache_write_per_m),
        ] {
            if let Some(value) = value
                && (!value.is_finite() || value < 0.0)
            {
                return Err(format!("pricing {label} must be a non-negative number"));
            }
        }
        Ok(())
    }

    /// 该规则是否匹配一个模型名。
    fn matches(&self, model: &str) -> bool {
        match self.pattern.strip_suffix('*') {
            Some(prefix) => model.starts_with(prefix),
            None => self.pattern == model,
        }
    }

    /// 按本条规则计算一次请求的成本。
    ///
    /// `input_tokens` 是**计费口径**的总输入（含缓存读写，见
    /// `Usage::billing_normalized`）；全价部分 = 总输入 − 缓存读 − 缓存写。
    pub fn cost(
        &self,
        input_tokens: u64,
        output_tokens: u64,
        cached_tokens: u64,
        cache_write_tokens: u64,
    ) -> f64 {
        let cached_price = self.cached_input_per_m.unwrap_or(self.input_per_m);
        let write_price = self.cache_write_per_m.unwrap_or(self.input_per_m);
        let billable_input = input_tokens.saturating_sub(cached_tokens + cache_write_tokens);
        (billable_input as f64 * self.input_per_m
            + cached_tokens as f64 * cached_price
            + cache_write_tokens as f64 * write_price
            + output_tokens as f64 * self.output_per_m)
            / 1_000_000.0
    }
}

/// 在价表中为模型选择计价规则：精确名优先，其后取最长前缀通配。
pub fn price_for<'a>(prices: &'a [ModelPrice], model: &str) -> Option<&'a ModelPrice> {
    let mut best: Option<&ModelPrice> = None;
    for price in prices.iter().filter(|p| p.matches(model)) {
        if !price.pattern.ends_with('*') {
            return Some(price);
        }
        if best.is_none_or(|b| price.pattern.len() > b.pattern.len()) {
            best = Some(price);
        }
    }
    best
}

/// 一条设置变更：写入已序列化的 JSON，或删除该键。
#[derive(Debug, Clone)]
pub enum SettingWrite<'a> {
    /// 写入 JSON 字符串。
    Set {
        /// 设置键。
        key: &'a str,
        /// 已序列化的 JSON 值。
        value: String,
    },
    /// 删除该键。
    Remove {
        /// 设置键。
        key: &'a str,
    },
}

/// 设置仓储。
#[derive(Debug, Clone)]
pub struct SettingsRepo {
    db: Database,
}

impl SettingsRepo {
    /// 绑定到一个数据库。
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// 读取一个设置项。不存在时返回 `None`。
    pub async fn get<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>, StoreError> {
        let row = sqlx::query("SELECT value FROM settings WHERE key = ?")
            .bind(key)
            .fetch_optional(self.db.pool())
            .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let raw: String = row.get("value");
        serde_json::from_str(&raw)
            .map(Some)
            .map_err(StoreError::json("settings.value"))
    }

    /// 读取一个设置项，不存在或解析失败时返回默认值。
    ///
    /// 解析失败也回落到默认值并告警：一个坏掉的设置行不应该让网关起不来。
    pub async fn get_or_default<T: DeserializeOwned + Default>(&self, key: &str) -> T {
        match self.get::<T>(key).await {
            Ok(Some(v)) => v,
            Ok(None) => T::default(),
            Err(err) => {
                tracing::warn!(key, %err, "unreadable setting, falling back to default");
                T::default()
            }
        }
    }

    /// 写入一个设置项。
    pub async fn set<T: Serialize>(&self, key: &str, value: &T) -> Result<(), StoreError> {
        let raw = serde_json::to_string(value).map_err(StoreError::json("settings.value"))?;
        sqlx::query(
            "INSERT INTO settings (key, value, updated_at) VALUES (?, ?, datetime('now')) \
             ON CONFLICT (key) DO UPDATE SET value = excluded.value, \
             updated_at = excluded.updated_at",
        )
        .bind(key)
        .bind(raw)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// 在同一事务里应用多条设置变更。空列表是 no-op。
    pub async fn apply(&self, writes: &[SettingWrite<'_>]) -> Result<(), StoreError> {
        if writes.is_empty() {
            return Ok(());
        }
        let mut tx = self.db.pool().begin().await?;
        for write in writes {
            match write {
                SettingWrite::Set { key, value } => {
                    sqlx::query(
                        "INSERT INTO settings (key, value, updated_at) VALUES (?, ?, datetime('now')) \
                         ON CONFLICT (key) DO UPDATE SET value = excluded.value, \
                         updated_at = excluded.updated_at",
                    )
                    .bind(*key)
                    .bind(value)
                    .execute(&mut *tx)
                    .await?;
                }
                SettingWrite::Remove { key } => {
                    sqlx::query("DELETE FROM settings WHERE key = ?")
                        .bind(*key)
                        .execute(&mut *tx)
                        .await?;
                }
            }
        }
        tx.commit().await?;
        Ok(())
    }

    fn encode<T: Serialize>(value: &T) -> Result<String, StoreError> {
        serde_json::to_string(value).map_err(StoreError::json("settings.value"))
    }

    /// 同时写入 webhook 与重测间隔。任一校验失败则两条都不落库。
    pub async fn set_notify(&self, url: Option<&str>, minutes: u32) -> Result<(), StoreError> {
        if minutes > 1440 {
            return Err(StoreError::Invalid(
                "retest interval must be at most 1440 minutes".into(),
            ));
        }
        let url_write = match url.map(str::trim).filter(|u| !u.is_empty()) {
            Some(url) => {
                if !url.starts_with("http://") && !url.starts_with("https://") {
                    return Err(StoreError::Invalid(
                        "webhook url must start with http:// or https://".into(),
                    ));
                }
                SettingWrite::Set {
                    key: KEY_WEBHOOK_URL,
                    value: Self::encode(&url)?,
                }
            }
            None => SettingWrite::Remove {
                key: KEY_WEBHOOK_URL,
            },
        };
        self.apply(&[
            url_write,
            SettingWrite::Set {
                key: KEY_RETEST_MINUTES,
                value: Self::encode(&minutes)?,
            },
        ])
        .await
    }

    /// 写入或清除管理令牌。写入时与 `auth.initialized` 同事务提交。
    pub async fn set_admin_token(&self, hash: Option<&str>) -> Result<(), StoreError> {
        match hash.filter(|value| !value.is_empty()) {
            Some(hash) => {
                self.apply(&[
                    SettingWrite::Set {
                        key: KEY_ADMIN_TOKEN_HASH,
                        value: Self::encode(&hash)?,
                    },
                    SettingWrite::Set {
                        key: KEY_AUTH_INITIALIZED,
                        value: Self::encode(&true)?,
                    },
                ])
                .await
            }
            None => self.remove(KEY_ADMIN_TOKEN_HASH).await,
        }
    }

    /// 首次引导：哈希、用户名、initialized 同事务写入。
    pub async fn bootstrap_admin(&self, hash: &str, username: &str) -> Result<(), StoreError> {
        self.apply(&[
            SettingWrite::Set {
                key: KEY_ADMIN_TOKEN_HASH,
                value: Self::encode(&hash)?,
            },
            SettingWrite::Set {
                key: KEY_ADMIN_USERNAME,
                value: Self::encode(&username)?,
            },
            SettingWrite::Set {
                key: KEY_AUTH_INITIALIZED,
                value: Self::encode(&true)?,
            },
        ])
        .await
    }

    /// 导入备份里的可导出设置。先全部校验，再同事务写入。
    pub async fn import_settings(
        &self,
        routing: &RoutingPolicy,
        log_retention_days: u32,
        breaker: &BreakerPolicy,
        pricing: &[ModelPrice],
        empty_retry: &EmptyResponseRetryPolicy,
    ) -> Result<(), StoreError> {
        routing.validate().map_err(StoreError::Invalid)?;
        if !(1..=MAX_LOG_RETENTION_DAYS).contains(&log_retention_days) {
            return Err(StoreError::Invalid(format!(
                "log retention days must be between 1 and {MAX_LOG_RETENTION_DAYS}"
            )));
        }
        breaker.validate().map_err(StoreError::Invalid)?;
        for price in pricing {
            price.validate().map_err(StoreError::Invalid)?;
        }
        empty_retry
            .validate()
            .map_err(|message| StoreError::Invalid(message.into()))?;
        self.apply(&[
            SettingWrite::Set {
                key: KEY_ROUTING_POLICY,
                value: Self::encode(routing)?,
            },
            SettingWrite::Set {
                key: KEY_LOG_RETENTION_DAYS,
                value: Self::encode(&log_retention_days)?,
            },
            SettingWrite::Set {
                key: KEY_BREAKER_POLICY,
                value: Self::encode(breaker)?,
            },
            SettingWrite::Set {
                key: KEY_PRICING,
                value: Self::encode(&pricing)?,
            },
            SettingWrite::Set {
                key: KEY_EMPTY_RESPONSE_RETRY,
                value: Self::encode(empty_retry)?,
            },
        ])
        .await
    }

    /// 删除一个设置项。
    pub async fn remove(&self, key: &str) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM settings WHERE key = ?")
            .bind(key)
            .execute(self.db.pool())
            .await?;
        Ok(())
    }

    /// 列出全部设置项（原始 JSON 字符串）。
    pub async fn all(&self) -> Result<Vec<(String, String)>, StoreError> {
        let rows = sqlx::query("SELECT key, value FROM settings ORDER BY key")
            .fetch_all(self.db.pool())
            .await?;
        Ok(rows
            .iter()
            .map(|r| (r.get::<String, _>("key"), r.get::<String, _>("value")))
            .collect())
    }

    /// 读取路由策略。
    ///
    /// 单独给它一对类型化访问器（而不是让调用方自己拼 key + 泛型），是因为
    /// 它是每个请求都要用的核心配置：key 拼错的代价是「策略静默回落到默认值」，
    /// 这种错误不会报任何错，只会让路由行为莫名其妙。
    pub async fn routing_policy(&self) -> Result<RoutingPolicy, StoreError> {
        Ok(self.get_or_default(KEY_ROUTING_POLICY).await)
    }

    /// 写入路由策略。
    pub async fn set_routing_policy(&self, policy: &RoutingPolicy) -> Result<(), StoreError> {
        policy.validate().map_err(StoreError::Invalid)?;
        self.set(KEY_ROUTING_POLICY, policy).await
    }

    /// 读取熔断策略；缺失或损坏时使用默认值。
    pub async fn breaker_policy(&self) -> Result<BreakerPolicy, StoreError> {
        Ok(self.get_or_default(KEY_BREAKER_POLICY).await)
    }

    /// 校验并保存熔断策略。
    pub async fn set_breaker_policy(&self, policy: &BreakerPolicy) -> Result<(), StoreError> {
        policy.validate().map_err(StoreError::Invalid)?;
        self.set(KEY_BREAKER_POLICY, policy).await
    }

    /// 读取模型价表。缺失时为空（所有请求成本记 0）。
    pub async fn pricing(&self) -> Result<Vec<ModelPrice>, StoreError> {
        Ok(self
            .get::<Vec<ModelPrice>>(KEY_PRICING)
            .await?
            .unwrap_or_default())
    }

    /// 校验并保存模型价表。
    pub async fn set_pricing(&self, prices: &[ModelPrice]) -> Result<(), StoreError> {
        for price in prices {
            price.validate().map_err(StoreError::Invalid)?;
        }
        self.set(KEY_PRICING, &prices).await
    }

    /// 是否记录请求/响应正文快照。缺省开启 —— 个人网关的排障价值优先；
    /// 对正文敏感的部署可在设置页关掉。
    pub async fn capture_bodies(&self) -> Result<bool, StoreError> {
        Ok(self.get::<bool>(KEY_LOG_BODIES).await?.unwrap_or(true))
    }

    /// 设置是否记录正文快照。
    pub async fn set_capture_bodies(&self, enabled: bool) -> Result<(), StoreError> {
        self.set(KEY_LOG_BODIES, &enabled).await
    }

    /// 是否暴露 per-user Prometheus 指标。缺省关闭，防止用户 ID label 高基数。
    pub async fn per_user_metrics(&self) -> Result<bool, StoreError> {
        Ok(self
            .get::<bool>(KEY_METRICS_PER_USER)
            .await?
            .unwrap_or(false))
    }

    /// 设置 per-user Prometheus 指标开关。
    pub async fn set_per_user_metrics(&self, enabled: bool) -> Result<(), StoreError> {
        self.set(KEY_METRICS_PER_USER, &enabled).await
    }

    /// 告警 webhook 地址。空或缺失表示不通知。
    pub async fn webhook_url(&self) -> Result<Option<String>, StoreError> {
        Ok(self
            .get::<String>(KEY_WEBHOOK_URL)
            .await?
            .map(|url| url.trim().to_owned())
            .filter(|url| !url.is_empty()))
    }

    /// 设置告警 webhook 地址。传空清除。
    pub async fn set_webhook_url(&self, url: Option<&str>) -> Result<(), StoreError> {
        match url.map(str::trim).filter(|u| !u.is_empty()) {
            Some(url) => {
                if !url.starts_with("http://") && !url.starts_with("https://") {
                    return Err(StoreError::Invalid(
                        "webhook url must start with http:// or https://".into(),
                    ));
                }
                self.set(KEY_WEBHOOK_URL, &url).await
            }
            None => self.remove(KEY_WEBHOOK_URL).await,
        }
    }

    /// 自动禁用渠道的重测间隔（分钟）。0 表示关闭自愈。
    pub async fn retest_minutes(&self) -> u32 {
        match self.get::<u32>(KEY_RETEST_MINUTES).await {
            Ok(Some(minutes @ 0..=1440)) => minutes,
            Ok(Some(_)) | Ok(None) => DEFAULT_RETEST_MINUTES,
            Err(error) => {
                tracing::warn!(%error, "unreadable retest interval, using default");
                DEFAULT_RETEST_MINUTES
            }
        }
    }

    /// 设置重测间隔。
    pub async fn set_retest_minutes(&self, minutes: u32) -> Result<(), StoreError> {
        if minutes > 1440 {
            return Err(StoreError::Invalid(
                "retest interval must be at most 1440 minutes".into(),
            ));
        }
        self.set(KEY_RETEST_MINUTES, &minutes).await
    }

    /// 全局限制。缺省全 0（不限）。
    pub async fn global_limits(&self) -> Result<GlobalLimits, StoreError> {
        Ok(self.get_or_default(KEY_GLOBAL_LIMITS).await)
    }

    /// 校验并保存全局限制。
    pub async fn set_global_limits(&self, limits: &GlobalLimits) -> Result<(), StoreError> {
        limits.validate().map_err(StoreError::Invalid)?;
        self.set(KEY_GLOBAL_LIMITS, limits).await
    }

    /// 单 IP 限速。缺省 0(不限)。
    pub async fn ip_limits(&self) -> Result<IpLimits, StoreError> {
        Ok(self.get_or_default(KEY_IP_LIMITS).await)
    }

    /// 校验并保存单 IP 限速。
    pub async fn set_ip_limits(&self, limits: &IpLimits) -> Result<(), StoreError> {
        limits.validate().map_err(StoreError::Invalid)?;
        self.set(KEY_IP_LIMITS, limits).await
    }

    /// 告警 webhook 签名密钥。未配置返回 `None`;读取方永远不应回显明文。
    pub async fn webhook_secret(&self) -> Result<Option<String>, StoreError> {
        self.get::<String>(KEY_WEBHOOK_SECRET).await
    }

    /// 设置或清除 webhook 签名密钥(`None` = 清除)。
    pub async fn set_webhook_secret(&self, secret: Option<&str>) -> Result<(), StoreError> {
        match secret {
            Some(value) if !value.is_empty() => {
                self.set(KEY_WEBHOOK_SECRET, &value.to_owned()).await
            }
            _ => self.remove(KEY_WEBHOOK_SECRET).await,
        }
    }

    /// 自动备份设置。缺省关闭(interval_hours = 0)。
    pub async fn backup_settings(&self) -> Result<BackupSettings, StoreError> {
        Ok(self.get_or_default(KEY_BACKUP).await)
    }

    /// 校验并保存自动备份设置。
    pub async fn set_backup_settings(&self, settings: &BackupSettings) -> Result<(), StoreError> {
        settings.validate().map_err(StoreError::Invalid)?;
        self.set(KEY_BACKUP, settings).await
    }

    /// 凭据静态加密主密钥(base64)。未配置返回 `None`。
    pub async fn master_key(&self) -> Result<Option<String>, StoreError> {
        self.get::<String>(KEY_MASTER_KEY).await
    }

    /// 默认管理员账号邮箱。未配置返回 `None`。
    pub async fn admin_username(&self) -> Result<Option<String>, StoreError> {
        self.get::<String>(KEY_ADMIN_USERNAME).await
    }

    /// SMTP 外发地址。未配置返回 `None`（验证码走 log）。
    pub async fn smtp_url(&self) -> Result<Option<String>, StoreError> {
        self.get::<String>(KEY_SMTP_URL).await
    }

    /// 邮件发件人地址。未配置返回 `None`。
    pub async fn mail_from(&self) -> Result<Option<String>, StoreError> {
        self.get::<String>(KEY_MAIL_FROM).await
    }

    /// 保存 SMTP 配置（`None` = 清除）。
    pub async fn set_mail(
        &self,
        smtp_url: Option<&str>,
        from: Option<&str>,
    ) -> Result<(), StoreError> {
        match smtp_url {
            Some(url) if !url.is_empty() => self.set(KEY_SMTP_URL, &url.to_owned()).await?,
            _ => self.remove(KEY_SMTP_URL).await?,
        }
        match from {
            Some(addr) if !addr.is_empty() => self.set(KEY_MAIL_FROM, &addr.to_owned()).await?,
            _ => self.remove(KEY_MAIL_FROM).await?,
        }
        Ok(())
    }

    /// 设置或清除主密钥(`None` = 清除)。值必须是 32 字节的 base64。
    pub async fn set_master_key(&self, key: Option<&str>) -> Result<(), StoreError> {
        match key {
            Some(value) if !value.is_empty() => {
                crate::crypto::parse_master_key(value)
                    .map_err(|e| StoreError::Invalid(e.to_string()))?;
                self.set(KEY_MASTER_KEY, &value.to_owned()).await
            }
            _ => self.remove(KEY_MASTER_KEY).await,
        }
    }

    /// HTTP 200 响应策略。缺省为空回复 3 秒内最多重试 5 次，严格校验关闭。
    pub async fn empty_response_retry(&self) -> Result<EmptyResponseRetryPolicy, StoreError> {
        Ok(self.get_or_default(KEY_EMPTY_RESPONSE_RETRY).await)
    }

    /// 校验并保存 HTTP 200 空回复重试策略。
    pub async fn set_empty_response_retry(
        &self,
        policy: EmptyResponseRetryPolicy,
    ) -> Result<(), StoreError> {
        policy
            .validate()
            .map_err(|message| StoreError::Invalid(message.into()))?;
        self.set(KEY_EMPTY_RESPONSE_RETRY, &policy).await
    }

    /// 读取渠道亲和性设置。缺失时为全默认（功能关闭）。
    pub async fn affinity(&self) -> Result<AffinitySettings, StoreError> {
        Ok(self.get_or_default(KEY_AFFINITY).await)
    }

    /// 校验并保存渠道亲和性设置。
    pub async fn set_affinity(&self, settings: &AffinitySettings) -> Result<(), StoreError> {
        settings
            .validate()
            .map_err(|message| StoreError::Invalid(message.to_string()))?;
        self.set(KEY_AFFINITY, settings).await
    }

    /// 读取日志保留天数；缺失或损坏时使用 30 天。
    pub async fn log_retention_days(&self) -> u32 {
        match self.get::<u32>(KEY_LOG_RETENTION_DAYS).await {
            Ok(Some(days @ 1..=MAX_LOG_RETENTION_DAYS)) => days,
            Ok(Some(days)) => {
                tracing::warn!(days, "invalid log retention setting, using default");
                DEFAULT_LOG_RETENTION_DAYS
            }
            Ok(None) => DEFAULT_LOG_RETENTION_DAYS,
            Err(error) => {
                tracing::warn!(%error, "unreadable log retention setting, using default");
                DEFAULT_LOG_RETENTION_DAYS
            }
        }
    }

    /// 校验并保存日志保留天数。
    pub async fn set_log_retention_days(&self, days: u32) -> Result<(), StoreError> {
        if !(1..=MAX_LOG_RETENTION_DAYS).contains(&days) {
            return Err(StoreError::Invalid(format!(
                "log retention days must be between 1 and {MAX_LOG_RETENTION_DAYS}"
            )));
        }
        self.set(KEY_LOG_RETENTION_DAYS, &days).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    async fn repo() -> SettingsRepo {
        SettingsRepo::new(Database::open_in_memory().await.unwrap())
    }

    #[tokio::test]
    async fn set_then_get_roundtrips_a_bool() {
        let repo = repo().await;
        assert_eq!(repo.get::<bool>(KEY_LOG_BODIES).await.unwrap(), None);

        repo.set(KEY_LOG_BODIES, &true).await.unwrap();
        assert_eq!(repo.get::<bool>(KEY_LOG_BODIES).await.unwrap(), Some(true));

        repo.set(KEY_LOG_BODIES, &false).await.unwrap();
        assert_eq!(repo.get::<bool>(KEY_LOG_BODIES).await.unwrap(), Some(false));
    }

    #[tokio::test]
    async fn empty_response_retry_defaults_roundtrips_and_validates() {
        let repo = repo().await;
        assert_eq!(
            repo.empty_response_retry().await.unwrap(),
            EmptyResponseRetryPolicy::default()
        );

        let policy = EmptyResponseRetryPolicy {
            window_secs: 9,
            max_retries: 4,
            reject_nonstandard_200: true,
        };
        repo.set_empty_response_retry(policy).await.unwrap();
        assert_eq!(repo.empty_response_retry().await.unwrap(), policy);

        repo.set(
            KEY_EMPTY_RESPONSE_RETRY,
            &serde_json::json!({"window_secs": 7, "max_retries": 2}),
        )
        .await
        .unwrap();
        let legacy = repo.empty_response_retry().await.unwrap();
        assert_eq!(legacy.window_secs, 7);
        assert_eq!(legacy.max_retries, 2);
        assert!(!legacy.reject_nonstandard_200);

        assert!(
            repo.set_empty_response_retry(EmptyResponseRetryPolicy {
                window_secs: 3,
                max_retries: 101,
                reject_nonstandard_200: false,
            })
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn upsert_does_not_duplicate_rows() {
        let repo = repo().await;
        repo.set(KEY_LOG_RETENTION_DAYS, &30_u32).await.unwrap();
        repo.set(KEY_LOG_RETENTION_DAYS, &7_u32).await.unwrap();

        let all = repo.all().await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(
            repo.get::<u32>(KEY_LOG_RETENTION_DAYS).await.unwrap(),
            Some(7)
        );
    }

    #[tokio::test]
    async fn log_retention_is_typed_bounded_and_defaults_safely() {
        let repo = repo().await;
        assert_eq!(repo.log_retention_days().await, DEFAULT_LOG_RETENTION_DAYS);
        repo.set_log_retention_days(90).await.unwrap();
        assert_eq!(repo.log_retention_days().await, 90);
        assert!(repo.set_log_retention_days(0).await.is_err());
        assert!(
            repo.set_log_retention_days(MAX_LOG_RETENTION_DAYS + 1)
                .await
                .is_err()
        );
        assert_eq!(repo.log_retention_days().await, 90);
    }

    #[tokio::test]
    async fn routing_policy_roundtrips_as_a_struct() {
        let repo = repo().await;
        let policy = RoutingPolicy {
            native_first: false,
            max_attempts: 5,
            ..Default::default()
        };
        repo.set(KEY_ROUTING_POLICY, &policy).await.unwrap();

        let back: RoutingPolicy = repo.get(KEY_ROUTING_POLICY).await.unwrap().unwrap();
        assert_eq!(back, policy);
        assert!(!back.native_first);
        assert_eq!(back.max_attempts, 5);
    }

    #[tokio::test]
    async fn missing_key_yields_type_default() {
        let repo = repo().await;
        let policy: RoutingPolicy = repo.get_or_default(KEY_ROUTING_POLICY).await;
        // RoutingPolicy 的默认值是原生优先开启。
        assert!(policy.native_first);
    }

    #[tokio::test]
    async fn corrupt_value_falls_back_instead_of_failing() {
        // 一行坏设置不能让网关起不来。
        let repo = repo().await;
        sqlx::query("INSERT INTO settings (key, value) VALUES (?, 'not json')")
            .bind(KEY_ROUTING_POLICY)
            .execute(repo.db.pool())
            .await
            .unwrap();

        assert!(repo.get::<RoutingPolicy>(KEY_ROUTING_POLICY).await.is_err());
        let fallback: RoutingPolicy = repo.get_or_default(KEY_ROUTING_POLICY).await;
        assert_eq!(fallback, RoutingPolicy::default());
    }

    #[tokio::test]
    async fn breaker_policy_roundtrips_and_validates() {
        let repo = repo().await;
        // 缺省回落默认值。
        assert_eq!(
            repo.breaker_policy().await.unwrap(),
            BreakerPolicy::default()
        );

        let custom = BreakerPolicy {
            failure_threshold: 3,
            base_cooldown_secs: 10,
            max_cooldown_secs: 120,
        };
        repo.set_breaker_policy(&custom).await.unwrap();
        assert_eq!(repo.breaker_policy().await.unwrap(), custom);

        // 非法组合在写入前被拒。
        let bad = BreakerPolicy {
            failure_threshold: 3,
            base_cooldown_secs: 600,
            max_cooldown_secs: 300,
        };
        assert!(repo.set_breaker_policy(&bad).await.is_err());
        assert_eq!(repo.breaker_policy().await.unwrap(), custom);
    }

    #[tokio::test]
    async fn pricing_roundtrips_and_rejects_bad_rules() {
        let repo = repo().await;
        assert!(repo.pricing().await.unwrap().is_empty());

        let prices = vec![
            ModelPrice {
                pattern: "gpt-4o".into(),
                input_per_m: 2.5,
                output_per_m: 10.0,
                cached_input_per_m: None,
                cache_write_per_m: None,
            },
            ModelPrice {
                pattern: "gpt-4o*".into(),
                input_per_m: 1.0,
                output_per_m: 4.0,
                cached_input_per_m: None,
                cache_write_per_m: None,
            },
        ];
        repo.set_pricing(&prices).await.unwrap();
        assert_eq!(repo.pricing().await.unwrap(), prices);

        let bad = vec![ModelPrice {
            pattern: "".into(),
            input_per_m: 1.0,
            output_per_m: 1.0,
            cached_input_per_m: None,
            cache_write_per_m: None,
        }];
        assert!(repo.set_pricing(&bad).await.is_err());
        let nan = vec![ModelPrice {
            pattern: "x".into(),
            input_per_m: f64::NAN,
            output_per_m: 1.0,
            cached_input_per_m: None,
            cache_write_per_m: None,
        }];
        assert!(repo.set_pricing(&nan).await.is_err());
    }

    #[test]
    fn price_matching_prefers_exact_then_longest_prefix() {
        let prices = vec![
            ModelPrice {
                pattern: "gpt-*".into(),
                input_per_m: 9.0,
                output_per_m: 9.0,
                cached_input_per_m: None,
                cache_write_per_m: None,
            },
            ModelPrice {
                pattern: "gpt-4o*".into(),
                input_per_m: 5.0,
                output_per_m: 5.0,
                cached_input_per_m: None,
                cache_write_per_m: None,
            },
            ModelPrice {
                pattern: "gpt-4o-mini".into(),
                input_per_m: 0.15,
                output_per_m: 0.6,
                cached_input_per_m: None,
                cache_write_per_m: None,
            },
        ];
        // 精确名优先。
        assert_eq!(price_for(&prices, "gpt-4o-mini").unwrap().input_per_m, 0.15);
        // 其后最长前缀。
        assert_eq!(price_for(&prices, "gpt-4o-2024").unwrap().input_per_m, 5.0);
        assert_eq!(price_for(&prices, "gpt-3.5").unwrap().input_per_m, 9.0);
        assert!(price_for(&prices, "claude-sonnet").is_none());

        // 成本 = tokens × 每百万单价。
        let rule = price_for(&prices, "gpt-4o-mini").unwrap();
        let cost = rule.cost(1_000_000, 2_000_000, 0, 0);
        assert!((cost - (0.15 + 1.2)).abs() < 1e-9);
    }

    #[test]
    fn cache_aware_cost_splits_input_into_three_tiers() {
        let rule = ModelPrice {
            pattern: "claude-*".into(),
            input_per_m: 3.0,
            output_per_m: 15.0,
            cached_input_per_m: Some(0.3),
            cache_write_per_m: Some(3.75),
        };
        // 总输入 1M = 全价 400k + 缓存读 500k + 缓存写 100k；输出 200k。
        let cost = rule.cost(1_000_000, 200_000, 500_000, 100_000);
        let expected = 0.4 * 3.0 + 0.5 * 0.3 + 0.1 * 3.75 + 0.2 * 15.0;
        assert!((cost - expected).abs() < 1e-9, "cost = {cost}");

        // 未配缓存价时回落输入价（等于不打折），账单只会偏保守。
        let flat = ModelPrice {
            pattern: "gpt-4o".into(),
            input_per_m: 2.0,
            output_per_m: 8.0,
            cached_input_per_m: None,
            cache_write_per_m: None,
        };
        let flat_cost = flat.cost(1_000_000, 0, 600_000, 0);
        assert!((flat_cost - 2.0).abs() < 1e-9);
    }

    #[tokio::test]
    async fn remove_deletes_the_key() {
        let repo = repo().await;
        repo.set(KEY_ADMIN_TOKEN_HASH, &"abc").await.unwrap();
        repo.remove(KEY_ADMIN_TOKEN_HASH).await.unwrap();
        assert_eq!(
            repo.get::<String>(KEY_ADMIN_TOKEN_HASH).await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn all_returns_sorted_keys() {
        let repo = repo().await;
        repo.set(KEY_LOG_BODIES, &true).await.unwrap();
        repo.set(KEY_LOG_RETENTION_DAYS, &30_u32).await.unwrap();

        let all = repo.all().await.unwrap();
        let keys: Vec<&str> = all.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            vec![KEY_LOG_BODIES, KEY_LOG_RETENTION_DAYS]
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>(),
        );
    }

    #[tokio::test]
    async fn ip_limits_defaults_roundtrips_and_validates() {
        let repo = repo().await;
        assert_eq!(repo.ip_limits().await.unwrap(), IpLimits::default());

        let limits = IpLimits { rpm: 120 };
        repo.set_ip_limits(&limits).await.unwrap();
        assert_eq!(repo.ip_limits().await.unwrap(), limits);

        let too_big = IpLimits { rpm: 1_000_001 };
        assert!(repo.set_ip_limits(&too_big).await.is_err());
    }

    #[tokio::test]
    async fn global_limits_defaults_roundtrips_and_validates() {
        let repo = repo().await;
        assert_eq!(repo.global_limits().await.unwrap(), GlobalLimits::default());

        let limits = GlobalLimits {
            rpm: 600,
            tpm: 2_000_000,
            max_concurrency: 32,
        };
        repo.set_global_limits(&limits).await.unwrap();
        assert_eq!(repo.global_limits().await.unwrap(), limits);

        for bad in [
            GlobalLimits {
                rpm: 1_000_001,
                ..Default::default()
            },
            GlobalLimits {
                tpm: 1_000_000_001,
                ..Default::default()
            },
            GlobalLimits {
                max_concurrency: 100_001,
                ..Default::default()
            },
        ] {
            assert!(repo.set_global_limits(&bad).await.is_err(), "{bad:?}");
        }
        // 越界写入被拒后，库里仍是上一次的合法值。
        assert_eq!(repo.global_limits().await.unwrap(), limits);
    }

    /// 旧库里只存了 rpm/max_concurrency 的 JSON 必须能反序列化，tpm 缺省为 0（不限）。
    #[tokio::test]
    async fn global_limits_deserializes_legacy_json_without_tpm() {
        let repo = repo().await;
        repo.set(
            KEY_GLOBAL_LIMITS,
            &serde_json::json!({ "rpm": 60, "max_concurrency": 8 }),
        )
        .await
        .unwrap();
        assert_eq!(
            repo.global_limits().await.unwrap(),
            GlobalLimits {
                rpm: 60,
                tpm: 0,
                max_concurrency: 8,
            }
        );
    }

    #[tokio::test]
    async fn webhook_secret_set_clear_roundtrips() {
        let repo = repo().await;
        assert_eq!(repo.webhook_secret().await.unwrap(), None);

        repo.set_webhook_secret(Some("s3cret")).await.unwrap();
        assert_eq!(repo.webhook_secret().await.unwrap(), Some("s3cret".into()));

        // 空串与 None 都视为清除。
        repo.set_webhook_secret(Some("")).await.unwrap();
        assert_eq!(repo.webhook_secret().await.unwrap(), None);
    }

    #[tokio::test]
    async fn backup_settings_defaults_roundtrips_and_validates() {
        let repo = repo().await;
        assert_eq!(
            repo.backup_settings().await.unwrap(),
            BackupSettings::default()
        );

        let settings = BackupSettings {
            directory: Some("/data/backups".into()),
            interval_hours: 6,
            keep: 12,
        };
        repo.set_backup_settings(&settings).await.unwrap();
        assert_eq!(repo.backup_settings().await.unwrap(), settings);

        let bad_keep = BackupSettings {
            keep: 0,
            ..BackupSettings::default()
        };
        assert!(repo.set_backup_settings(&bad_keep).await.is_err());

        let bad_interval = BackupSettings {
            interval_hours: 8761,
            ..BackupSettings::default()
        };
        assert!(repo.set_backup_settings(&bad_interval).await.is_err());
    }

    #[tokio::test]
    async fn master_key_requires_32_bytes_base64() {
        let repo = repo().await;
        assert_eq!(repo.master_key().await.unwrap(), None);

        let key = base64::engine::general_purpose::STANDARD.encode([7_u8; 32]);
        repo.set_master_key(Some(&key)).await.unwrap();
        assert_eq!(repo.master_key().await.unwrap(), Some(key.clone()));

        // 长度不对的密钥必须被拒绝。
        let short = base64::engine::general_purpose::STANDARD.encode([7_u8; 16]);
        assert!(repo.set_master_key(Some(&short)).await.is_err());
        assert!(repo.set_master_key(Some("not-base64!!!")).await.is_err());

        repo.set_master_key(None).await.unwrap();
        assert_eq!(repo.master_key().await.unwrap(), None);
    }

    #[tokio::test]
    async fn routing_policy_validate_rejects_absurd_max_attempts() {
        let repo = repo().await;
        let policy = RoutingPolicy {
            max_attempts: 33,
            ..Default::default()
        };
        assert!(repo.set_routing_policy(&policy).await.is_err());

        let ok = RoutingPolicy {
            max_attempts: 5,
            max_upstream_calls: 12,
            ..Default::default()
        };
        repo.set_routing_policy(&ok).await.unwrap();
        assert_eq!(repo.routing_policy().await.unwrap(), ok);
    }

    #[tokio::test]
    async fn set_notify_rejects_bad_minutes_without_writing_url() {
        let repo = repo().await;
        let err = repo
            .set_notify(Some("https://example.com/hook"), 1441)
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Invalid(_)));
        assert_eq!(repo.webhook_url().await.unwrap(), None);
        assert_eq!(repo.retest_minutes().await, DEFAULT_RETEST_MINUTES);
    }

    #[tokio::test]
    async fn set_notify_writes_url_and_minutes_together() {
        let repo = repo().await;
        repo.set_notify(Some("https://example.com/hook"), 15)
            .await
            .unwrap();
        assert_eq!(
            repo.webhook_url().await.unwrap().as_deref(),
            Some("https://example.com/hook")
        );
        assert_eq!(repo.retest_minutes().await, 15);

        repo.set_notify(None, 0).await.unwrap();
        assert_eq!(repo.webhook_url().await.unwrap(), None);
        assert_eq!(repo.retest_minutes().await, 0);
    }

    #[tokio::test]
    async fn set_admin_token_writes_hash_and_initialized_together() {
        let repo = repo().await;
        repo.set_admin_token(Some("deadbeef")).await.unwrap();
        assert_eq!(
            repo.get::<String>(KEY_ADMIN_TOKEN_HASH)
                .await
                .unwrap()
                .as_deref(),
            Some("deadbeef")
        );
        assert_eq!(
            repo.get::<bool>(KEY_AUTH_INITIALIZED).await.unwrap(),
            Some(true)
        );

        repo.set_admin_token(None).await.unwrap();
        assert_eq!(
            repo.get::<String>(KEY_ADMIN_TOKEN_HASH).await.unwrap(),
            None
        );
        assert_eq!(
            repo.get::<bool>(KEY_AUTH_INITIALIZED).await.unwrap(),
            Some(true)
        );
    }
}
