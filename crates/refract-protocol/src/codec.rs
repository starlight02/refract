//! 协议编解码器接口。
//!
//! 新增一个协议 = 实现这三个 trait（加上流式编解码器），即 O(1) 的工作量 ——
//! 不需要碰任何既有 codec。这是「中枢辐射」模型的核心收益。

use refract_core::{GatewayError, Protocol};
use serde_json::Value;

use crate::ir::{UnifiedRequest, UnifiedResponse};
use crate::stream::{StreamDecoder, StreamEncoder};

pub mod chat;
pub mod gemini;
pub mod messages;
pub mod responses;

/// 请求编解码器：上游请求 JSON ⇄ [`UnifiedRequest`]。
pub trait RequestCodec {
    /// 解析上游请求 JSON 为 IR。
    ///
    /// 实现要求：
    /// - 宽松解析。上游可能带未知字段，未知字段进 [`crate::ir::Extensions`]
    ///   （带 `"<protocol>."` 前缀），不要报错。
    /// - 缺失必填字段要报 [`GatewayError::invalid_request`]，给出能直接
    ///   展示给客户端看的消息。
    fn decode_request(&self, raw: &Value) -> Result<UnifiedRequest, GatewayError>;

    /// 把 IR 编码为上游请求 JSON。
    ///
    /// 实现要求：
    /// - [`crate::ir::Extensions`] 中本协议的专属字段必须还原回去。
    /// - 尽力而为：IR 里存在但目标协议无法表达的字段，静默丢弃并
    ///   `tracing::debug!`，不要失败。
    fn encode_request(&self, ir: &UnifiedRequest) -> Result<Value, GatewayError>;
}

/// 响应编解码器：上游响应 JSON ⇄ [`UnifiedResponse`]。
pub trait ResponseCodec {
    /// 解析上游响应 JSON 为 IR。
    fn decode_response(&self, raw: &Value) -> Result<UnifiedResponse, GatewayError>;

    /// 把 IR 编码为上游响应 JSON。
    fn encode_response(&self, ir: &UnifiedResponse) -> Result<Value, GatewayError>;
}

/// 流式编解码器：上游 SSE ⇄ [`crate::stream::StreamEvent`]。
///
/// 解码器和编码器都是**有状态**的 —— 调用方必须为每个上游连接/客户端连接
/// 各持有一个实例，不能跨请求复用。
pub trait StreamCodec {
    /// 创建流式解码器。
    fn stream_decoder(&self) -> Box<dyn StreamDecoder>;

    /// 创建流式编码器。
    fn stream_encoder(&self) -> Box<dyn StreamEncoder>;
}

/// 一个协议的完整编解码实现。
///
/// 用方法而非关联常量暴露协议，否则 trait 不是对象安全的，
/// [`CodecSet`] 就没法用 `&dyn ProtocolCodec` 存放。
pub trait ProtocolCodec:
    RequestCodec + ResponseCodec + StreamCodec + Send + Sync + 'static
{
    /// 该 codec 对应的协议。
    fn protocol(&self) -> Protocol;
}

/// 四个协议的 codec 注册表。
///
/// 网关启动时构造一次，路由层用它按协议查 codec。
#[derive(Clone, Copy)]
pub struct CodecSet {
    chat: &'static dyn ProtocolCodec,
    responses: &'static dyn ProtocolCodec,
    messages: &'static dyn ProtocolCodec,
    gemini: &'static dyn ProtocolCodec,
}

impl CodecSet {
    /// 注册四个协议。
    ///
    /// # Panics
    ///
    /// 若任一 codec 报告的协议与其位置不符则 panic —— 这是装配期的编程错误，
    /// 让它尽早炸掉比在运行时把请求发错协议好。
    pub fn new(
        chat: &'static dyn ProtocolCodec,
        responses: &'static dyn ProtocolCodec,
        messages: &'static dyn ProtocolCodec,
        gemini: &'static dyn ProtocolCodec,
    ) -> Self {
        assert_eq!(chat.protocol(), Protocol::Chat);
        assert_eq!(responses.protocol(), Protocol::Responses);
        assert_eq!(messages.protocol(), Protocol::Messages);
        assert_eq!(gemini.protocol(), Protocol::Gemini);
        Self {
            chat,
            responses,
            messages,
            gemini,
        }
    }

    /// 按协议取 codec。
    pub fn for_protocol(&self, protocol: Protocol) -> &'static dyn ProtocolCodec {
        match protocol {
            Protocol::Chat => self.chat,
            Protocol::Responses => self.responses,
            Protocol::Messages => self.messages,
            Protocol::Gemini => self.gemini,
        }
    }

    /// 装配内置的四个 codec。
    ///
    /// 这是唯一把具体实现与协议绑定的地方 —— 换掉某个协议的实现只需要改这里
    /// 一行，不影响任何调用方。
    pub fn builtin() -> Self {
        Self::new(
            &chat::CHAT,
            &responses::RESPONSES,
            &messages::MESSAGES,
            &gemini::GEMINI,
        )
    }
}

impl Default for CodecSet {
    fn default() -> Self {
        Self::builtin()
    }
}

impl std::fmt::Debug for CodecSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodecSet").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_set_maps_every_protocol_to_its_own_codec() {
        let set = CodecSet::builtin();
        for protocol in Protocol::ALL {
            assert_eq!(set.for_protocol(protocol).protocol(), protocol);
        }
    }
}
