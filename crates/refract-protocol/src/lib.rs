//! 协议转换层。
//!
//! 四个协议（OpenAI Chat / OpenAI Responses / Anthropic Messages / Gemini）
//! 与统一中间表示（IR）之间的编解码，以及流式事件的互相转换。
//!
//! 本 crate 是纯函数层：不依赖存储、不依赖网络、不依赖渠道配置。
//! 这是协议转换正确性的保证 —— 每个 codec 都能脱离一切 IO 做单元测试。

// lint 配置统一在 workspace `Cargo.toml` 的 [workspace.lints] 里维护。

pub mod codec;
pub mod ir;
pub mod stream;

pub use codec::{CodecSet, ProtocolCodec, RequestCodec, ResponseCodec, StreamCodec};
pub use ir::{
    ContentPart, Extensions, MediaSource, Message, ReasoningConfig, ResponseFormat, Role, Sampling,
    StopReason, ToolChoice, ToolDef, UnifiedRequest, UnifiedResponse, Usage,
};
pub use stream::{
    PartKind, SseFrame, SseParser, StreamAggregator, StreamDecoder, StreamEncoder, StreamEvent,
};
