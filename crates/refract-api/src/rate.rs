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
use std::sync::Mutex;

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

    fn admit_at(&self, key_id: i64, rpm: i64, tpm: i64, now: u64) -> Result<(), RateExceeded> {
        let minute = now / 60;
        let retry_after_secs = 60 - (now % 60);
        let mut windows = self.windows.lock().expect("rate windows lock");
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
        self.windows.lock().expect("rate windows lock").len()
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
        let mut windows = self.windows.lock().expect("rate windows lock");
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
        let mut windows = self.windows.lock().expect("ip rate windows lock");
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
}
