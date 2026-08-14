//! 上游 HTTP 客户端。
//!
//! 职责边界：**只负责把一个已经决定好的请求发出去，并把响应原样带回来**。
//! 它不做协议转换（那是 `refract-protocol` 的事），不做渠道选择与重试
//! （那是 `refract-router` 的事）。这条边界让上游客户端可以被完整地
//! 单元测试，也让重试逻辑不必关心 HTTP 细节。
//!
//! 两个关键设计：
//!
//! 1. **流式响应绝不缓冲**。SSE 响应以 `Stream` 形式逐帧透传，首字节延迟
//!    等于上游的首字节延迟。若在这里 `collect()` 成完整 body，流式就退化成
//!    了「等上游说完再一次性吐给客户端」，那是最糟糕的用户体验。
//! 2. **连接池全局共享**。`reqwest::Client` 内部就是连接池，克隆是浅拷贝。
//!    每次请求新建 client 会丢掉 keep-alive 与 TLS 会话复用，对同一上游的
//!    连续请求会白付一次 TLS 握手。

// lint 配置统一在 workspace `Cargo.toml` 的 [workspace.lints] 里维护。

pub mod client;
pub mod probe;
pub mod sse;

pub use client::{
    UpstreamClient, UpstreamClientConfig, UpstreamRawResponse, UpstreamRawStream, UpstreamRequest,
    UpstreamResponse, UpstreamSseStream,
};
pub use probe::{ModelProbe, probe_balance, probe_models};
pub use sse::{ByteStream, SseStream, sse_stream};
