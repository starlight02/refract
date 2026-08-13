//! Refract 核心领域模型。
//!
//! 本 crate 不做任何 IO，只定义领域概念与其不变量：协议、地址构造、渠道、
//! 路由策略、错误分类。所有其他 crate 依赖它，它不依赖任何其他 crate。

// lint 配置统一在 workspace `Cargo.toml` 的 [workspace.lints] 里维护。

pub mod address;
pub mod channel;
pub mod error;
pub mod protocol;
pub mod routing;

pub use address::{Action, AddressError, UpstreamAddress};
pub use channel::{
    Channel, ChannelEndpoint, ChannelError, ChannelId, ChannelKind, Credential, ModelEntry,
    TranscodePolicy,
};
pub use error::{ErrorKind, GatewayError};
pub use protocol::{AuthScheme, ParseProtocolError, Protocol, ProtocolSet};
pub use routing::{RankKey, RoutingPolicy, SelectionMode, weighted_pick};

/// 当前唯一所有者 ID。
///
/// 系统为单用户设计，但所有业务实体都带 `owner_id` 列，将来加多用户时
/// 只需放开这个常量的来源，不动业务逻辑。
pub const DEFAULT_OWNER_ID: i64 = 1;

pub(crate) const fn default_owner() -> i64 {
    DEFAULT_OWNER_ID
}

pub(crate) const fn default_true() -> bool {
    true
}

pub(crate) const fn default_weight() -> u32 {
    1
}
