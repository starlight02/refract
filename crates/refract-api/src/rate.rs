//! 速率限制（RPM / TPM）：每密钥维度与网关全局维度。
//!
//! 固定窗口（自然分钟）计数，纯内存维护：单实例部署下不需要分布式协调，
//! 数据库只存策略（每把密钥的上限、网关级上限），重启即清零 —— 限流是
//! 保护措施而非计费，短暂放空可以接受。
//!
//! TPM 采用「后账前查」：token 用量在请求完成后才可知，当前窗口已计入的
//! 用量达到上限时拒绝新请求。这与 OpenAI 的可观察行为一致 —— 一个恰好
//! 跨过上限的大请求会被放行，下一个请求才被挡。
//!
//! 全局窗口复用同一张表的保留 key（[`GLOBAL_WINDOW_KEY`]）：免鉴权模式下
//! 没有密钥可挂账，全局窗口是唯一挡住 token 洪水的地方。

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

/// 全局窗口借用的保留 key。真实密钥 ID 由 SQLite AUTOINCREMENT 生成，从 1 起，
/// 0 永远不会被占用 —— 全局 RPM/TPM 与每密钥限速因此共用一张表、共用一把锁。
pub const GLOBAL_WINDOW_KEY: i64 = 0;

/// 进程内窗口表上限。超出后丢掉非当前分钟的条目，避免删掉的密钥 ID 永驻。
const MAX_WINDOWS: usize = 10_000;

/// 每密钥的分钟窗口计数器。
///
/// 锁而非无锁结构：限流检查在鉴权之后、路由之前，每请求一次，个人网关的
/// 并发量级下一把 `Mutex` 的争用可以忽略；换 sharded map 只会增加依赖。
#[derive(Debug, Default)]
pub struct RateLimiter {
    windows: Mutex<HashMap<i64, Window>>,
}

#[derive(Debug, Clone, Copy)]
struct Window {
    minute: u64,
    requests: u64,
    tokens: u64,
}

/// 限流拒绝：附带触发维度与建议等待时长。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateExceeded {
    /// 触发的维度（诊断信息用）。
    pub dimension: RateDimension,
    /// 距窗口重置的秒数，回写 `Retry-After`。
    pub retry_after_secs: u64,
}

/// 触发限流的维度。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateDimension {
    /// 每分钟请求数。
    Requests,
    /// 每分钟 token 数。
    Tokens,
}

impl RateDimension {
    /// 错误信息里的人类可读描述。
    pub const fn describe(self) -> &'static str {
        match self {
            RateDimension::Requests => "requests per minute (RPM)",
            RateDimension::Tokens => "tokens per minute (TPM)",
        }
    }
}

impl RateLimiter {
    /// 创建空限流器。
    pub fn new() -> Self {
        Self::default()
    }

    /// 请求准入检查；通过则把本次请求计入窗口。
    ///
    /// `rpm`/`tpm` 任一 `<= 0` 表示该维度不限。
    pub fn admit(&self, key_id: i64, rpm: i64, tpm: i64) -> Result<(), RateExceeded> {
        if rpm <= 0 && tpm <= 0 {
            return Ok(());
        }
        let now = now_secs();
        self.admit_at(key_id, rpm, tpm, now)
    }

    fn lock_windows(&self) -> MutexGuard<'_, HashMap<i64, Window>> {
        self.windows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn admit_at(&self, key_id: i64, rpm: i64, tpm: i64, now: u64) -> Result<(), RateExceeded> {
        let minute = now / 60;
        let retry_after_secs = 60 - (now % 60);
        let mut windows = self.lock_windows();
        prune_stale_windows(&mut windows, minute);
        let window = windows.entry(key_id).or_insert(Window {
            minute,
            requests: 0,
            tokens: 0,
        });
        if window.minute != minute {
            *window = Window {
                minute,
                requests: 0,
                tokens: 0,
            };
        }
        if rpm > 0 && window.requests >= rpm as u64 {
            return Err(RateExceeded {
                dimension: RateDimension::Requests,
                retry_after_secs,
            });
        }
        if tpm > 0 && window.tokens >= tpm as u64 {
            return Err(RateExceeded {
                dimension: RateDimension::Tokens,
                retry_after_secs,
            });
        }
        window.requests += 1;
        Ok(())
    }

    #[cfg(test)]
    fn window_count(&self) -> usize {
        self.lock_windows().len()
    }

    /// 请求完成后把 token 用量计入当前窗口。
    pub fn add_tokens(&self, key_id: i64, tokens: u64) {
        if tokens == 0 {
            return;
        }
        self.add_tokens_at(key_id, tokens, now_secs());
    }

    fn add_tokens_at(&self, key_id: i64, tokens: u64, now: u64) {
        let minute = now / 60;
        let mut windows = self.lock_windows();
        prune_stale_windows(&mut windows, minute);
        let window = windows.entry(key_id).or_insert(Window {
            minute,
            requests: 0,
            tokens: 0,
        });
        if window.minute != minute {
            *window = Window {
                minute,
                requests: 0,
                tokens: 0,
            };
        }
        window.tokens = window.tokens.saturating_add(tokens);
    }
}

/// 按客户端 IP 的速率限制器（RPM）。
#[derive(Debug, Default)]
pub struct IpRateLimiter {
    windows: Mutex<HashMap<std::net::IpAddr, IpWindow>>,
}

#[derive(Debug, Clone, Copy)]
struct IpWindow {
    minute: u64,
    requests: u64,
}

impl IpRateLimiter {
    /// 创建空 IP 限流器。
    pub fn new() -> Self {
        Self::default()
    }

    /// IP 请求准入检查；通过则计入窗口。`rpm == 0` 表示不限。
    pub fn admit(&self, ip: std::net::IpAddr, rpm: u32) -> Result<(), RateExceeded> {
        if rpm == 0 {
            return Ok(());
        }
        let now = now_secs();
        let minute = now / 60;
        let retry_after_secs = 60 - (now % 60);
        let mut windows = self
            .windows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if windows.len() > 10_000 {
            windows.retain(|_, w| w.minute == minute);
        }
        let window = windows.entry(ip).or_insert(IpWindow {
            minute,
            requests: 0,
        });
        if window.minute != minute {
            *window = IpWindow {
                minute,
                requests: 0,
            };
        }
        if window.requests >= rpm as u64 {
            return Err(RateExceeded {
                dimension: RateDimension::Requests,
                retry_after_secs,
            });
        }
        window.requests += 1;
        Ok(())
    }
}

fn prune_stale_windows(windows: &mut HashMap<i64, Window>, minute: u64) {
    if windows.len() > MAX_WINDOWS {
        windows.retain(|_, window| window.minute == minute);
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn unlimited_keys_are_never_throttled() {
        let limiter = RateLimiter::new();
        for _ in 0..10_000 {
            limiter.admit_at(1, 0, 0, 60).unwrap();
        }
    }

    #[test]
    fn rpm_blocks_after_the_limit_and_resets_next_minute() {
        let limiter = RateLimiter::new();
        limiter.admit_at(1, 2, 0, 60).unwrap();
        limiter.admit_at(1, 2, 0, 61).unwrap();

        let denied = limiter.admit_at(1, 2, 0, 62).unwrap_err();
        assert_eq!(denied.dimension, RateDimension::Requests);
        // 62s → 距下一分钟还有 58 秒。
        assert_eq!(denied.retry_after_secs, 58);

        // 下一自然分钟重新放行。
        limiter.admit_at(1, 2, 0, 120).unwrap();
    }

    #[test]
    fn tpm_uses_posted_usage_from_the_same_window() {
        let limiter = RateLimiter::new();
        limiter.admit_at(1, 0, 100, 60).unwrap();
        limiter.add_tokens_at(1, 100, 61);

        let denied = limiter.admit_at(1, 0, 100, 62).unwrap_err();
        assert_eq!(denied.dimension, RateDimension::Tokens);

        // 窗口翻转后 token 计数清零。
        limiter.admit_at(1, 0, 100, 120).unwrap();
    }

    #[test]
    fn windows_are_isolated_per_key() {
        let limiter = RateLimiter::new();
        limiter.admit_at(1, 1, 0, 60).unwrap();
        limiter.admit_at(1, 1, 0, 61).unwrap_err();
        // 另一把密钥不受影响。
        limiter.admit_at(2, 1, 0, 61).unwrap();
    }

    #[test]
    fn token_posting_after_window_flip_starts_fresh() {
        let limiter = RateLimiter::new();
        limiter.add_tokens_at(1, 50, 60);
        limiter.add_tokens_at(1, 7, 120);
        // 120s 窗口里只有 7 个 token，tpm=10 仍应放行。
        limiter.admit_at(1, 0, 10, 121).unwrap();
    }

    #[test]
    fn stale_key_windows_are_pruned_when_the_table_grows() {
        let limiter = RateLimiter::new();
        for key in 1..=MAX_WINDOWS as i64 + 1 {
            limiter.admit_at(key, 1, 0, 60).unwrap();
        }
        assert!(limiter.window_count() > MAX_WINDOWS);

        limiter.admit_at(i64::MAX, 1, 0, 120).unwrap();
        assert!(limiter.window_count() <= MAX_WINDOWS);
    }

    #[test]
    fn ttl_cache_expires_and_invalidates() {
        let cache = TtlCache::new(std::time::Duration::from_millis(50));
        cache.insert(1, 10);
        assert_eq!(cache.get(&1), Some(10));
        cache.invalidate(&1);
        assert_eq!(cache.get(&1), None);
        cache.insert(2, 20);
        std::thread::sleep(std::time::Duration::from_millis(60));
        assert_eq!(cache.get(&2), None);
    }

    #[test]
    fn auth_rate_limiter_enforces_hourly_attempts_and_daily_successes() {
        let limiter = AuthRateLimiter::new();
        let ip = std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);
        for _ in 0..5 {
            assert!(limiter.check_register(ip).is_none());
            limiter.record_register_attempt(ip);
        }
        // 第 6 次尝试撞每小时窗口。
        assert!(limiter.check_register(ip).is_some());

        let ip2 = std::net::IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 2));
        for _ in 0..3 {
            limiter.record_register_attempt(ip2);
            limiter.record_register_success(ip2);
        }
        assert!(limiter.check_register(ip2).is_some());
    }
}

/// 极简 TTL 缓存：读时惰性过期，写时全量覆盖。
///
/// 用于用户状态（30 秒）与余额（60 秒）这类「允许秒级延迟、但每次请求打库太贵」
/// 的读多写少数据。主动失效由写路径负责（见 `AppState`）。
#[derive(Debug)]
pub struct TtlCache<K, V> {
    entries: std::sync::Arc<Mutex<HashMap<K, (V, std::time::Instant)>>>,
    ttl: std::time::Duration,
}

impl<K, V> Clone for TtlCache<K, V> {
    fn clone(&self) -> Self {
        Self {
            entries: std::sync::Arc::clone(&self.entries),
            ttl: self.ttl,
        }
    }
}

impl<K, V> TtlCache<K, V>
where
    K: std::hash::Hash + Eq,
    V: Clone,
{
    /// 创建缓存。
    pub fn new(ttl: std::time::Duration) -> Self {
        Self {
            entries: std::sync::Arc::new(Mutex::new(HashMap::new())),
            ttl,
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<K, (V, std::time::Instant)>> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 读。过期即视为未命中并顺手清掉。
    pub fn get(&self, key: &K) -> Option<V> {
        let mut guard = self.lock();
        let (value, at) = guard.get(key)?;
        if at.elapsed() >= self.ttl {
            guard.remove(key);
            return None;
        }
        Some(value.clone())
    }

    /// 写。
    pub fn insert(&self, key: K, value: V) {
        self.lock().insert(key, (value, std::time::Instant::now()));
    }

    /// 主动失效。
    pub fn invalidate(&self, key: &K) {
        self.lock().remove(key);
    }
}

/// 注册/认证限流：每 IP 每小时最多 5 次注册尝试 + 每 IP 每天最多 3 个成功账号。
///
/// 纯内存实现，重启清零 —— 防刷是保护措施而非精确计费，与 `RateLimiter` 同哲学。
#[derive(Debug, Default)]
pub struct AuthRateLimiter {
    /// 每小时注册尝试窗口。
    hourly: Mutex<HashMap<std::net::IpAddr, (u32, std::time::Instant)>>,
    /// 每天注册成功窗口。
    daily: Mutex<HashMap<std::net::IpAddr, (u32, std::time::Instant)>>,
}

impl AuthRateLimiter {
    /// 创建限流器。
    pub fn new() -> Self {
        Self::default()
    }

    /// 该 IP 当前是否被注册限流。返回剩余等待秒数。
    pub fn check_register(&self, ip: std::net::IpAddr) -> Option<u64> {
        let now = std::time::Instant::now();
        {
            let mut hourly = self
                .hourly
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some((count, at)) = hourly.get(&ip).copied() {
                if now.duration_since(at) >= std::time::Duration::from_secs(3600) {
                    hourly.remove(&ip);
                } else if count >= 5 {
                    return Some(
                        3600_u64
                            .saturating_sub(now.duration_since(at).as_secs())
                            .max(1),
                    );
                }
            }
        }
        let mut daily = self
            .daily
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((count, at)) = daily.get(&ip).copied() {
            if now.duration_since(at) >= std::time::Duration::from_secs(86_400) {
                daily.remove(&ip);
            } else if count >= 3 {
                return Some(
                    86_400_u64
                        .saturating_sub(now.duration_since(at).as_secs())
                        .max(1),
                );
            }
        }
        None
    }

    /// 记录一次注册尝试。
    pub fn record_register_attempt(&self, ip: std::net::IpAddr) {
        let mut hourly = self
            .hourly
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = hourly.entry(ip).or_insert((0, std::time::Instant::now()));
        if entry.1.elapsed() >= std::time::Duration::from_secs(3600) {
            *entry = (0, std::time::Instant::now());
        }
        entry.0 = entry.0.saturating_add(1);
    }

    /// 记录一次注册成功（占当天名额）。
    pub fn record_register_success(&self, ip: std::net::IpAddr) {
        let mut daily = self
            .daily
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = daily.entry(ip).or_insert((0, std::time::Instant::now()));
        if entry.1.elapsed() >= std::time::Duration::from_secs(86_400) {
            *entry = (0, std::time::Instant::now());
        }
        entry.0 = entry.0.saturating_add(1);
    }
}
