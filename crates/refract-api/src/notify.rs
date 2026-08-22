//! 路由事件的策略层：终态错误自动禁用 + webhook 告警 + 定时重测自愈。
//!
//! 网关的职责是无人值守。这个模块把三件事接成闭环：
//! 1. **发现**：executor 发出的失败/恢复事件在这里汇聚；
//! 2. **处置**：401/403 这类不会自愈的终态错误连续出现后自动禁用渠道
//!    （与手动禁用分开标记，保留自愈资格）；
//! 3. **告知**：熔断、恢复、自动禁用都推送到用户配置的 webhook——
//!    出事不吭声等于失职。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use refract_core::{ChannelId, ErrorKind, Protocol};
use refract_router::RouterEvent;

use crate::state::AppState;

/// 连续多少次终态错误后自动禁用渠道。
///
/// 3 次是「偶发 401（上游抖动/网关误判）」与「key 真的废了」的分界；
/// new-api/one-api 的等价功能同样不做成可配置项。
pub const AUTH_DISABLE_THRESHOLD: u32 = 3;

/// 同一事件的去重窗口：熔断的指数退避会反复触发 suspend，
/// 每次都推送只会把通知渠道变成噪音源。
const DEDUP_WINDOW: Duration = Duration::from_secs(300);

/// webhook 请求超时。
const WEBHOOK_TIMEOUT: Duration = Duration::from_secs(5);

/// 拉起路由事件消费者。随进程存活；通道关闭（AppState 全部销毁）时退出。
pub fn spawn_event_worker(
    state: AppState,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<RouterEvent>,
) {
    tokio::spawn(async move {
        let mut worker = EventWorker::default();
        while let Some(event) = rx.recv().await {
            worker.handle(&state, event).await;
        }
    });
}

#[derive(Default)]
struct EventWorker {
    /// 每个端点的连续终态错误计数。
    auth_streaks: HashMap<(ChannelId, Protocol), u32>,
    /// 通知去重表。
    dedup: HashMap<String, Instant>,
}

impl EventWorker {
    async fn handle(&mut self, state: &AppState, event: RouterEvent) {
        match event {
            RouterEvent::Failure {
                channel_id,
                channel_name,
                protocol,
                kind,
                message,
                suspended,
                ..
            } => {
                // 终态错误：凭据废了不会自己好，计数触顶后停用渠道。
                if matches!(
                    kind,
                    ErrorKind::Unauthenticated | ErrorKind::PermissionDenied
                ) {
                    let streak = self.auth_streaks.entry((channel_id, protocol)).or_insert(0);
                    *streak += 1;
                    if *streak >= AUTH_DISABLE_THRESHOLD {
                        *streak = 0;
                        self.auto_disable(state, channel_id, &channel_name, protocol, &message)
                            .await;
                        return;
                    }
                }
                if suspended {
                    self.notify(
                        state,
                        "endpoint.suspended",
                        &channel_name,
                        Some(protocol),
                        &format!("端点被熔断暂停：{message}"),
                    )
                    .await;
                }
            }
            RouterEvent::Success {
                channel_id,
                channel_name,
                protocol,
                recovered,
            } => {
                self.auth_streaks.remove(&(channel_id, protocol));
                if recovered {
                    self.notify(
                        state,
                        "endpoint.recovered",
                        &channel_name,
                        Some(protocol),
                        "端点已从熔断中恢复",
                    )
                    .await;
                }
            }
        }
    }

    async fn auto_disable(
        &mut self,
        state: &AppState,
        channel_id: ChannelId,
        channel_name: &str,
        protocol: Protocol,
        reason: &str,
    ) {
        // 渠道可能已被手动禁用/删除；失败时只记日志，不重试 ——
        // 下一轮失败事件会再次走到这里。
        match state
            .channel_repo()
            .set_auto_disabled(refract_core::DEFAULT_OWNER_ID, channel_id)
            .await
        {
            Ok(()) => {
                if let Err(error) = state.reload_channels().await {
                    tracing::warn!(%error, "failed to reload channels after auto-disable");
                }
                tracing::warn!(
                    channel = channel_name,
                    %protocol,
                    reason,
                    "channel auto-disabled after repeated auth failures"
                );
                self.notify(
                    state,
                    "channel.auto_disabled",
                    channel_name,
                    Some(protocol),
                    &format!(
                        "连续 {AUTH_DISABLE_THRESHOLD} 次凭据/权限错误，渠道已自动停用：{reason}"
                    ),
                )
                .await;
            }
            Err(error) => {
                tracing::warn!(%error, channel = channel_name, "failed to auto-disable channel");
            }
        }
    }

    /// 推送一条事件到 webhook。未配置地址或命中去重窗口时静默跳过。
    async fn notify(
        &mut self,
        state: &AppState,
        event: &str,
        channel: &str,
        protocol: Option<Protocol>,
        detail: &str,
    ) {
        let Some(url) = state.webhook_url() else {
            return;
        };
        let key = format!("{event}:{channel}:{protocol:?}");
        let now = Instant::now();
        if let Some(last) = self.dedup.get(&key)
            && now.duration_since(*last) < DEDUP_WINDOW
        {
            return;
        }
        self.dedup.insert(key, now);
        // 顺手清一下过期条目，防止长期运行下的无界增长。
        self.dedup
            .retain(|_, at| now.duration_since(*at) < DEDUP_WINDOW * 2);

        // 投递放到独立任务里：事件循环是串行单消费者，通知端点挂 5 秒
        // 就会把熔断/恢复事件的处置整体卡住。丢一条通知可以接受，
        // 卡住事件流不行。
        let url = url.to_owned();
        let event = event.to_owned();
        let channel = channel.to_owned();
        let detail = detail.to_owned();
        let secret = state.webhook_secret();
        tokio::spawn(async move {
            send_webhook(&url, &event, &channel, protocol, &detail, secret.as_deref()).await;
        });
    }
}

/// 发送单条 webhook。负载是通用 JSON —— 一个「自定义 HTTP」出口足以对接
/// Telegram bot 网桥、Server 酱、飞书自定义机器人等，不内建 N 个平台适配器。
pub async fn send_webhook(
    url: &str,
    event: &str,
    channel: &str,
    protocol: Option<Protocol>,
    detail: &str,
    secret: Option<&str>,
) {
    let payload = serde_json::json!({
        "source": "refract",
        "event": event,
        "channel": channel,
        "protocol": protocol.map(|p| p.as_str()),
        "detail": detail,
        "timestamp": chrono::Utc::now().to_rfc3339(),
    });
    let client = match reqwest::Client::builder().timeout(WEBHOOK_TIMEOUT).build() {
        Ok(client) => client,
        Err(error) => {
            tracing::warn!(%error, "failed to build webhook client");
            return;
        }
    };
    let body_bytes = match serde_json::to_vec(&payload) {
        Ok(b) => b,
        Err(error) => {
            tracing::warn!(%error, "failed to serialize webhook payload");
            return;
        }
    };
    let mut req = client
        .post(url)
        .header("content-type", "application/json")
        .body(body_bytes.clone());
    if let Some(s) = secret.filter(|s| !s.is_empty()) {
        let sig_bytes = hmac_sha256(s.as_bytes(), &body_bytes);
        let signature = hex::encode(sig_bytes);
        req = req.header("x-refract-signature", format!("sha256={signature}"));
    }

    match req.send().await {
        Ok(response) if !response.status().is_success() => {
            tracing::warn!(status = %response.status(), event, "webhook endpoint returned non-2xx");
        }
        Ok(_) => {}
        Err(error) => {
            tracing::warn!(%error, event, "failed to deliver webhook");
        }
    }
}

/// HMAC-SHA256 实现（基于 `sha2` crate，避免跨版本 digest 冲突）。
pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut k = [0_u8; 64];
    if key.len() > 64 {
        let hash = Sha256::digest(key);
        k[..32].copy_from_slice(&hash);
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36_u8; 64];
    let mut opad = [0x5c_u8; 64];
    for i in 0..64 {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(msg);
    let inner_hash = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner_hash);
    outer.finalize().into()
}

/// 自动禁用渠道的定时重测循环。间隔从设置读取（0 = 关闭），改动无需重启。
///
/// 只重测 `auto_disabled` 的渠道：手动禁用是用户的显式决定，不碰。
/// 重测走与管理页「测试」按钮相同的最小真实请求；成功即恢复渠道并通知。
pub async fn auto_retest_loop(state: AppState) {
    loop {
        let minutes = state.settings_repo().retest_minutes().await;
        if minutes == 0 {
            // 关闭时低频轮询设置本身，等它被重新打开。
            tokio::time::sleep(Duration::from_secs(300)).await;
            continue;
        }
        tokio::time::sleep(Duration::from_secs(u64::from(minutes) * 60)).await;

        let disabled: Vec<_> = match state
            .channel_repo()
            .list(refract_core::DEFAULT_OWNER_ID)
            .await
        {
            Ok(channels) => channels.into_iter().filter(|c| c.auto_disabled).collect(),
            Err(error) => {
                tracing::warn!(%error, "auto-retest: failed to list channels");
                continue;
            }
        };

        for channel in disabled {
            let result = crate::admin::run_channel_test(&state, &channel, Default::default()).await;
            let success = result["success"].as_bool().unwrap_or(false);
            if !success {
                tracing::info!(channel = %channel.name, "auto-retest: still failing");
                continue;
            }
            match state
                .channel_repo()
                .restore_auto_disabled(refract_core::DEFAULT_OWNER_ID, channel.id)
                .await
            {
                Ok(true) => {
                    if let Err(error) = state.reload_channels().await {
                        tracing::warn!(%error, "failed to reload channels after auto-recover");
                    }
                    tracing::info!(channel = %channel.name, "auto-retest: channel recovered");
                    if let Some(url) = state.webhook_url() {
                        let secret = state.webhook_secret();
                        send_webhook(
                            &url,
                            "channel.auto_recovered",
                            &channel.name,
                            None,
                            "自动重测成功，渠道已恢复启用",
                            secret.as_deref(),
                        )
                        .await;
                    }
                }
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(%error, channel = %channel.name, "failed to restore channel");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::hmac_sha256;

    #[test]
    fn hmac_sha256_matches_rfc4231_case_1() {
        let mac = hmac_sha256(&[0x0b; 20], b"Hi There");
        assert_eq!(
            hex::encode(mac),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    #[test]
    fn hmac_sha256_hashes_keys_longer_than_a_block() {
        let key = [0xaa; 131];
        let mac = hmac_sha256(
            &key,
            b"Test Using Larger Than Block-Size Key - Hash Key First",
        );
        assert_eq!(
            hex::encode(mac),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }
}
