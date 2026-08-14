//! 路由执行过程中产生的、值得上层关注的事件。
//!
//! 执行器只负责**发**（非阻塞、丢弃式），不关心谁在听、怎么处理 ——
//! 自动禁用的决策与 webhook 通知都属于策略层（refract-api），把它们
//! 塞进执行器会让路由热路径背上 IO 依赖。

use refract_core::{ChannelId, ErrorKind, Protocol};

/// 一条路由事件。
#[derive(Debug, Clone)]
pub enum RouterEvent {
    /// 一次上游调用失败。
    Failure {
        /// 渠道 ID。
        channel_id: ChannelId,
        /// 渠道名快照。
        channel_name: String,
        /// 端点协议。
        protocol: Protocol,
        /// 错误分类。
        kind: ErrorKind,
        /// 错误消息摘要。
        message: String,
        /// 本次失败后端点是否处于熔断挂起中。
        suspended: bool,
        /// 连续失败数（含本次）。
        consecutive_fails: u32,
    },
    /// 一次上游调用成功。
    Success {
        /// 渠道 ID。
        channel_id: ChannelId,
        /// 渠道名快照。
        channel_name: String,
        /// 端点协议。
        protocol: Protocol,
        /// 本次成功是否解除了熔断挂起。
        recovered: bool,
    },
}

/// 事件发送端。`None` 时执行器静默运行（测试与嵌入场景）。
pub type EventSender = tokio::sync::mpsc::UnboundedSender<RouterEvent>;
