//! 流式事件模型与 SSE 编解码。
//!
//! # 为什么需要统一事件模型
//!
//! 四种协议的流式格式差异远大于非流式：
//!
//! - **OpenAI Chat**：无事件名，`data: {choices:[{delta:{...}}]}`，以 `data: [DONE]` 收尾。
//!   工具调用靠 `tool_calls[].index` 累积，首帧带 `id`/`name`，后续只带 `arguments` 片段。
//! - **OpenAI Responses**：具名事件 + 全局递增 `sequence_number`，仪式性事件多
//!   （`response.created` → `output_item.added` → `content_part.added` → deltas → ...）。
//! - **Anthropic**：严格的 `message_start` → `content_block_start` → deltas →
//!   `content_block_stop` → `message_delta` → `message_stop` 序列，块有 index。
//! - **Gemini**：每个 chunk 是完整的 `{candidates:[...]}`，无增量语义标记。
//!
//! 所以编码器必须是**状态机**：它要记住自己已经发过哪些仪式性事件、当前开着
//! 哪个内容块，才能补齐目标协议要求的事件序列。

use refract_core::{ErrorKind, GatewayError};

use crate::ir::{ContentPart, StopReason, Usage};

/// 内容块的种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartKind {
    /// 文本。
    Text,
    /// 推理内容。
    Thinking,
    /// 工具调用。
    ToolUse,
    /// 拒答。
    Refusal,
}

/// 统一流式事件。
///
/// 解码器把上游 SSE 翻译成这些事件；编码器把它们翻译成客户端要的 SSE。
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// 流开始。
    Start {
        /// 响应 ID。
        id: String,
        /// 模型名。
        model: String,
        /// 首批 usage（Anthropic 在 `message_start` 就给 input_tokens）。
        usage: Option<Usage>,
    },
    /// 一个内容块开始。
    ContentStart {
        /// 块下标。
        index: u32,
        /// 块种类。
        kind: PartKind,
    },
    /// 文本增量。
    TextDelta {
        /// 块下标。
        index: u32,
        /// 增量文本。
        text: String,
    },
    /// 推理增量。
    ThinkingDelta {
        /// 块下标。
        index: u32,
        /// 增量文本。
        text: String,
    },
    /// 推理签名（Anthropic 在推理块结束前发）。
    ThinkingSignature {
        /// 块下标。
        index: u32,
        /// 签名。
        signature: String,
    },
    /// 拒答增量。
    RefusalDelta {
        /// 块下标。
        index: u32,
        /// 增量文本。
        text: String,
    },
    /// 工具调用开始。
    ToolCallStart {
        /// 块下标。
        index: u32,
        /// 调用 ID。
        id: String,
        /// 工具名。
        name: String,
        /// Gemini 的 `thoughtSignature`（挂在 functionCall part 上）。
        ///
        /// Gemini 3 要求多轮回传的工具调用带签名，流式直通时必须过链路。
        signature: Option<String>,
    },
    /// 工具调用入参增量（JSON 片段，不保证是合法 JSON）。
    ToolCallArgsDelta {
        /// 块下标。
        index: u32,
        /// JSON 片段。
        fragment: String,
    },
    /// 一个内容块结束。
    ContentStop {
        /// 块下标。
        index: u32,
    },
    /// 用量更新。
    Usage(Usage),
    /// 流结束原因。
    Stop {
        /// 停止原因。
        reason: StopReason,
        /// 命中的停止序列。
        stop_sequence: Option<String>,
    },
    /// 流正常终结（对应 `data: [DONE]` / `message_stop`）。
    Done,
    /// 流中的错误事件。
    Error {
        /// 错误消息。
        message: String,
        /// 错误类型。
        kind: String,
    },
    /// 心跳，无语义。编码器可选择转发或丢弃。
    Ping,
}

impl StreamEvent {
    /// 该事件是否代表流的终结。
    pub const fn is_terminal(&self) -> bool {
        matches!(self, StreamEvent::Done | StreamEvent::Error { .. })
    }

    /// 该事件涉及的块下标。
    pub const fn index(&self) -> Option<u32> {
        match self {
            StreamEvent::ContentStart { index, .. }
            | StreamEvent::TextDelta { index, .. }
            | StreamEvent::ThinkingDelta { index, .. }
            | StreamEvent::ThinkingSignature { index, .. }
            | StreamEvent::RefusalDelta { index, .. }
            | StreamEvent::ToolCallStart { index, .. }
            | StreamEvent::ToolCallArgsDelta { index, .. }
            | StreamEvent::ContentStop { index } => Some(*index),
            _ => None,
        }
    }
}

/// 一条 SSE 帧。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseFrame {
    /// `event:` 字段。OpenAI Chat 不用事件名，此处为 `None`。
    pub event: Option<String>,
    /// `data:` 字段。
    pub data: String,
}

impl SseFrame {
    /// 无事件名的数据帧。
    pub fn data(data: impl Into<String>) -> Self {
        Self {
            event: None,
            data: data.into(),
        }
    }

    /// 具名事件帧。
    pub fn named(event: impl Into<String>, data: impl Into<String>) -> Self {
        Self {
            event: Some(event.into()),
            data: data.into(),
        }
    }

    /// 渲染成 SSE 线格式（含结尾空行）。
    pub fn render(&self) -> String {
        let mut out = String::with_capacity(self.data.len() + 24);
        if let Some(ev) = &self.event {
            out.push_str("event: ");
            out.push_str(ev);
            out.push('\n');
        }
        // data 可能含换行，SSE 规范要求逐行加 `data: ` 前缀。
        for line in self.data.split('\n') {
            out.push_str("data: ");
            out.push_str(line);
            out.push('\n');
        }
        out.push('\n');
        out
    }
}

/// 增量式 SSE 解析器。
///
/// 处理上游返回的字节流：按行切分、组装 `event:`/`data:` 字段、在空行处产出帧。
/// 必须能处理**任意分块边界** —— TCP 不保证一个 read 恰好是一帧。
#[derive(Debug, Default)]
pub struct SseParser {
    buffer: Vec<u8>,
    event: Option<String>,
    data: Vec<String>,
}

impl SseParser {
    /// 新建解析器。
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂入一段字节，产出其中完整的帧。
    pub fn feed(&mut self, chunk: &str) -> Vec<SseFrame> {
        self.feed_bytes(chunk.as_bytes())
            .expect("a UTF-8 str cannot create an invalid SSE line")
    }

    /// 喂入任意 TCP 字节块；UTF-8 字符可以跨 chunk，完整行后才校验。
    pub fn feed_bytes(&mut self, chunk: &[u8]) -> Result<Vec<SseFrame>, GatewayError> {
        self.buffer.extend_from_slice(chunk);
        let mut frames = Vec::new();

        // 逐行消费缓冲区；不完整的末行留在缓冲里等下一次 feed。
        while let Some(pos) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let mut line = self.buffer[..pos].to_vec();
            self.buffer.drain(..=pos);
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let line = std::str::from_utf8(&line).map_err(|error| {
                GatewayError::new(
                    ErrorKind::UpstreamError,
                    format!("upstream SSE contains invalid UTF-8: {error}"),
                )
            })?;

            if line.is_empty() {
                if let Some(frame) = self.flush() {
                    frames.push(frame);
                }
                continue;
            }
            // 注释行（`:` 开头），SSE 规范里用于保活。
            if line.starts_with(':') {
                continue;
            }
            let (field, value) = match line.split_once(':') {
                Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
                None => (line, ""),
            };
            match field {
                "event" => self.event = Some(value.to_owned()),
                "data" => self.data.push(value.to_owned()),
                // id / retry 对我们没有语义。
                _ => {}
            }
        }
        Ok(frames)
    }

    /// 流结束时把残留内容作为最后一帧产出。
    pub fn finish(&mut self) -> Option<SseFrame> {
        self.finish_bytes()
            .expect("SSE assembled from UTF-8 str chunks must remain UTF-8")
    }

    /// 结束字节流并刷出最后一个残留帧。
    pub fn finish_bytes(&mut self) -> Result<Option<SseFrame>, GatewayError> {
        if !self.buffer.is_empty() {
            let rest = std::mem::take(&mut self.buffer);
            let rest = std::str::from_utf8(&rest).map_err(|error| {
                GatewayError::new(
                    ErrorKind::UpstreamError,
                    format!("upstream SSE contains invalid UTF-8: {error}"),
                )
            })?;
            let trimmed = rest.trim();
            if let Some(value) = trimmed.strip_prefix("data:") {
                self.data.push(value.trim_start().to_owned());
            }
        }
        Ok(self.flush())
    }

    fn flush(&mut self) -> Option<SseFrame> {
        if self.data.is_empty() && self.event.is_none() {
            return None;
        }
        let data = self.data.join("\n");
        self.data.clear();
        Some(SseFrame {
            event: self.event.take(),
            data,
        })
    }
}

/// 把上游 SSE 帧解码成统一事件。
///
/// 有状态：某些协议（如 OpenAI Chat 的 tool_calls）需要记住块下标与已见的
/// 工具调用 ID 才能正确产出 [`StreamEvent::ToolCallStart`]。
pub trait StreamDecoder: Send {
    /// 解码一帧，可能产出零个或多个事件。
    fn decode(&mut self, frame: &SseFrame) -> Result<Vec<StreamEvent>, GatewayError>;

    /// 流结束时补齐尾部事件。
    fn finish(&mut self) -> Result<Vec<StreamEvent>, GatewayError> {
        Ok(Vec::new())
    }
}

/// 把统一事件编码成客户端要的 SSE 帧。
///
/// 有状态：负责补齐目标协议要求的仪式性事件（如 Anthropic 的
/// `message_start`、Responses 的 `sequence_number`）。
pub trait StreamEncoder: Send {
    /// 编码一个事件，可能产出零个或多个帧。
    fn encode(&mut self, event: &StreamEvent) -> Result<Vec<SseFrame>, GatewayError>;

    /// 流结束时补齐尾部帧。
    fn finish(&mut self) -> Result<Vec<SseFrame>, GatewayError> {
        Ok(Vec::new())
    }
}

/// 累积流式事件，重建出完整的 [`crate::ir::UnifiedResponse`] 内容。
///
/// 用途：非流式客户端打到只支持流式的上游时（或反过来），需要把流聚合成整体。
/// 也用于计费 —— 流式响应的 usage 往往只在最后一帧出现。
/// 内容块按 `(index, kind)` 而非仅 `index` 归档。
///
/// 原因：不同协议对块下标的用法不一致，同一个下标上可能先后出现不同种类的
/// 增量（典型场景是 DeepSeek 系中转站把 `reasoning_content` 与 `content`
/// 都放在 `choices[0]`，两者天然共享下标 0）。只按 index 归档会导致后到的
/// 那一类被静默丢弃 —— 正文消失是最糟糕的失败模式。
#[derive(Debug, Default)]
pub struct StreamAggregator {
    /// 响应 ID。
    pub id: String,
    /// 模型名。
    pub model: String,
    /// 停止原因。
    pub stop_reason: Option<StopReason>,
    /// 命中的停止序列。
    pub stop_sequence: Option<String>,
    /// 累积用量。
    pub usage: Usage,
    /// 已见的内容块，按首次出现顺序排列。
    blocks: Vec<(u32, BlockAccum)>,
}

#[derive(Debug)]
enum BlockAccum {
    Text(String),
    Thinking {
        text: String,
        signature: Option<String>,
    },
    ToolUse {
        id: String,
        name: String,
        args: String,
        signature: Option<String>,
    },
    Refusal(String),
}

impl StreamAggregator {
    /// 新建聚合器。
    pub fn new() -> Self {
        Self::default()
    }

    /// 吸收一个事件。
    pub fn absorb(&mut self, event: &StreamEvent) {
        match event {
            StreamEvent::Start { id, model, usage } => {
                self.id = id.clone();
                self.model = model.clone();
                if let Some(u) = usage {
                    self.merge_usage(*u);
                }
            }
            StreamEvent::ContentStart { index, kind } => self.set_block(*index, *kind),
            StreamEvent::TextDelta { index, text } => {
                if let BlockAccum::Text(buf) = self.ensure(*index, PartKind::Text) {
                    buf.push_str(text);
                }
            }
            StreamEvent::ThinkingDelta { index, text } => {
                if let BlockAccum::Thinking { text: buf, .. } =
                    self.ensure(*index, PartKind::Thinking)
                {
                    buf.push_str(text);
                }
            }
            StreamEvent::ThinkingSignature { index, signature } => {
                if let BlockAccum::Thinking { signature: sig, .. } =
                    self.ensure(*index, PartKind::Thinking)
                {
                    *sig = Some(signature.clone());
                }
            }
            StreamEvent::RefusalDelta { index, text } => {
                if let BlockAccum::Refusal(buf) = self.ensure(*index, PartKind::Refusal) {
                    buf.push_str(text);
                }
            }
            StreamEvent::ToolCallStart {
                index,
                id,
                name,
                signature,
            } => {
                if let BlockAccum::ToolUse {
                    id: slot_id,
                    name: slot_name,
                    signature: slot_sig,
                    ..
                } = self.ensure(*index, PartKind::ToolUse)
                {
                    if !id.is_empty() {
                        *slot_id = id.clone();
                    }
                    if !name.is_empty() {
                        *slot_name = name.clone();
                    }
                    if signature.is_some() {
                        slot_sig.clone_from(signature);
                    }
                }
            }
            StreamEvent::ToolCallArgsDelta { index, fragment } => {
                if let BlockAccum::ToolUse { args, .. } = self.ensure(*index, PartKind::ToolUse) {
                    args.push_str(fragment);
                }
            }
            StreamEvent::Usage(u) => self.merge_usage(*u),
            StreamEvent::Stop {
                reason,
                stop_sequence,
            } => {
                self.stop_reason = Some(*reason);
                if stop_sequence.is_some() {
                    self.stop_sequence = stop_sequence.clone();
                }
            }
            StreamEvent::ContentStop { .. }
            | StreamEvent::Done
            | StreamEvent::Ping
            | StreamEvent::Error { .. } => {}
        }
    }

    /// 累积用量时取每个字段的最大值。
    ///
    /// 各协议的流式 usage 语义不一：Anthropic 在 `message_start` 给 input、
    /// 在 `message_delta` 给累积的 output；OpenAI 只在最后一帧给完整 usage。
    /// 取最大值对两种语义都正确，而累加会在 Anthropic 上重复计数。
    fn merge_usage(&mut self, u: Usage) {
        self.usage.input_tokens = self.usage.input_tokens.max(u.input_tokens);
        self.usage.output_tokens = self.usage.output_tokens.max(u.output_tokens);
        self.usage.cached_input_tokens = self.usage.cached_input_tokens.max(u.cached_input_tokens);
        self.usage.cache_write_tokens = self.usage.cache_write_tokens.max(u.cache_write_tokens);
        self.usage.reasoning_tokens = self.usage.reasoning_tokens.max(u.reasoning_tokens);
    }

    /// 为空块构造一个指定种类的累加器。
    fn fresh(kind: PartKind) -> BlockAccum {
        match kind {
            PartKind::Text => BlockAccum::Text(String::new()),
            PartKind::Thinking => BlockAccum::Thinking {
                text: String::new(),
                signature: None,
            },
            PartKind::ToolUse => BlockAccum::ToolUse {
                id: String::new(),
                name: String::new(),
                args: String::new(),
                signature: None,
            },
            PartKind::Refusal => BlockAccum::Refusal(String::new()),
        }
    }

    /// 该累加器是否属于给定种类。
    fn matches_kind(block: &BlockAccum, kind: PartKind) -> bool {
        matches!(
            (block, kind),
            (BlockAccum::Text(_), PartKind::Text)
                | (BlockAccum::Thinking { .. }, PartKind::Thinking)
                | (BlockAccum::ToolUse { .. }, PartKind::ToolUse)
                | (BlockAccum::Refusal(_), PartKind::Refusal)
        )
    }

    /// 显式开启一个内容块。
    ///
    /// 若 `(index, kind)` 已存在则复用（重复的 `ContentStart` 不该清空已收到的
    /// 增量），否则追加一个新块。
    fn set_block(&mut self, index: u32, kind: PartKind) {
        if self.find(index, kind).is_none() {
            self.blocks.push((index, Self::fresh(kind)));
        }
    }

    fn find(&mut self, index: u32, kind: PartKind) -> Option<usize> {
        self.blocks
            .iter()
            .position(|(i, b)| *i == index && Self::matches_kind(b, kind))
    }

    /// 取出 `(index, kind)` 对应的累加器，不存在则就地创建。
    ///
    /// 宽容策略：上游没发 `ContentStart` 就直接发 delta 是常态（中转站经常
    /// 省略仪式性事件），此时按 delta 的种类自动开块。
    fn ensure(&mut self, index: u32, kind: PartKind) -> &mut BlockAccum {
        let pos = match self.find(index, kind) {
            Some(pos) => pos,
            None => {
                self.blocks.push((index, Self::fresh(kind)));
                self.blocks.len() - 1
            }
        };
        &mut self.blocks[pos].1
    }

    /// 产出聚合后的内容片段。
    ///
    /// 输出顺序按块下标升序，同下标内按首次出现顺序 —— 这样推理块会排在
    /// 同下标的正文块之前，与各协议的惯例一致。
    pub fn into_content(self) -> Vec<ContentPart> {
        let mut blocks = self.blocks;
        // 稳定排序，保证同下标内的相对顺序不被打乱。
        blocks.sort_by_key(|(index, _)| *index);

        let mut out = Vec::with_capacity(blocks.len());
        for (_, block) in blocks {
            match block {
                BlockAccum::Text(text) if text.is_empty() => {}
                BlockAccum::Text(text) => out.push(ContentPart::Text { text }),
                BlockAccum::Thinking { text, signature }
                    if text.is_empty() && signature.is_none() => {}
                BlockAccum::Thinking { text, signature } => {
                    out.push(ContentPart::Thinking { text, signature });
                }
                BlockAccum::ToolUse {
                    id,
                    name,
                    args,
                    signature,
                } => {
                    // 入参是拼接出来的 JSON 字符串，可能不完整（流被截断）。
                    // 解析失败时保留原始字符串，让上层能看到到底收到了什么。
                    let input = if args.trim().is_empty() {
                        serde_json::Value::Object(serde_json::Map::new())
                    } else {
                        serde_json::from_str(&args)
                            .unwrap_or_else(|_| serde_json::Value::String(args.clone()))
                    };
                    out.push(ContentPart::ToolUse {
                        id,
                        name,
                        input,
                        signature,
                    });
                }
                BlockAccum::Refusal(text) if text.is_empty() => {}
                BlockAccum::Refusal(text) => out.push(ContentPart::Refusal { text }),
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn sse_frame_renders_event_and_data() {
        let frame = SseFrame::named("message_start", r#"{"type":"message_start"}"#);
        assert_eq!(
            frame.render(),
            "event: message_start\ndata: {\"type\":\"message_start\"}\n\n"
        );
    }

    #[test]
    fn sse_frame_prefixes_every_line_of_multiline_data() {
        let frame = SseFrame::data("line1\nline2");
        assert_eq!(frame.render(), "data: line1\ndata: line2\n\n");
    }

    #[test]
    fn parser_handles_frame_split_across_chunks() {
        let mut p = SseParser::new();
        // 帧被切成三段，包括切在 JSON 中间。
        assert!(p.feed("data: {\"a\":").is_empty());
        assert!(p.feed("1}").is_empty());
        let frames = p.feed("\n\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, r#"{"a":1}"#);
    }

    #[test]
    fn parser_accepts_utf8_codepoint_split_across_byte_chunks() {
        let mut parser = SseParser::new();
        let bytes = "data: 你好\n\n".as_bytes();
        let split = bytes
            .windows(2)
            .position(|window| window[0] & 0b1100_0000 == 0b1100_0000)
            .expect("multibyte character")
            + 1;

        assert!(parser.feed_bytes(&bytes[..split]).unwrap().is_empty());
        let frames = parser.feed_bytes(&bytes[split..]).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "你好");
    }

    #[test]
    fn parser_rejects_invalid_utf8_when_a_line_completes() {
        let mut parser = SseParser::new();
        let error = parser
            .feed_bytes(b"data: \xff\n\n")
            .expect_err("invalid UTF-8 must not be lossily forwarded");
        assert_eq!(error.kind, ErrorKind::UpstreamError);
        assert!(error.message.contains("invalid UTF-8"));
    }

    #[test]
    fn parser_reads_event_names() {
        let mut p = SseParser::new();
        let frames = p.feed("event: content_block_delta\ndata: {\"x\":1}\n\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event.as_deref(), Some("content_block_delta"));
    }

    #[test]
    fn parser_handles_crlf_and_comments() {
        let mut p = SseParser::new();
        let frames = p.feed(": keep-alive\r\ndata: {\"ok\":true}\r\n\r\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, r#"{"ok":true}"#);
    }

    #[test]
    fn parser_joins_multiple_data_lines() {
        let mut p = SseParser::new();
        let frames = p.feed("data: part1\ndata: part2\n\n");
        assert_eq!(frames[0].data, "part1\npart2");
    }

    #[test]
    fn parser_emits_multiple_frames_from_one_chunk() {
        let mut p = SseParser::new();
        let frames = p.feed("data: a\n\ndata: b\n\ndata: c\n\n");
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[2].data, "c");
    }

    #[test]
    fn parser_finish_flushes_trailing_frame_without_blank_line() {
        // 上游断流时最后一帧可能没有收尾空行，不能丢。
        let mut p = SseParser::new();
        assert!(p.feed("data: {\"last\":true}").is_empty());
        let frame = p.finish().expect("trailing frame");
        assert_eq!(frame.data, r#"{"last":true}"#);
    }

    #[test]
    fn aggregator_rebuilds_text_from_deltas() {
        let mut agg = StreamAggregator::new();
        agg.absorb(&StreamEvent::Start {
            id: "resp_1".into(),
            model: "gpt-4o".into(),
            usage: None,
        });
        agg.absorb(&StreamEvent::ContentStart {
            index: 0,
            kind: PartKind::Text,
        });
        agg.absorb(&StreamEvent::TextDelta {
            index: 0,
            text: "Hello".into(),
        });
        agg.absorb(&StreamEvent::TextDelta {
            index: 0,
            text: ", world".into(),
        });
        agg.absorb(&StreamEvent::ContentStop { index: 0 });
        agg.absorb(&StreamEvent::Stop {
            reason: StopReason::Stop,
            stop_sequence: None,
        });

        assert_eq!(agg.id, "resp_1");
        assert_eq!(agg.stop_reason, Some(StopReason::Stop));
        assert_eq!(agg.into_content(), vec![ContentPart::text("Hello, world")]);
    }

    #[test]
    fn aggregator_rebuilds_tool_call_from_fragments() {
        let mut agg = StreamAggregator::new();
        agg.absorb(&StreamEvent::ToolCallStart {
            index: 0,
            id: "call_1".into(),
            name: "get_weather".into(),
            signature: None,
        });
        for frag in [r#"{"ci"#, r#"ty":"To"#, r#"kyo"}"#] {
            agg.absorb(&StreamEvent::ToolCallArgsDelta {
                index: 0,
                fragment: frag.into(),
            });
        }
        let content = agg.into_content();
        assert_eq!(
            content,
            vec![ContentPart::ToolUse {
                id: "call_1".into(),
                name: "get_weather".into(),
                input: serde_json::json!({"city": "Tokyo"}),
                signature: None,
            }]
        );
    }

    #[test]
    fn aggregator_keeps_truncated_tool_args_as_string() {
        // 流被截断时不能丢数据，也不能 panic。
        let mut agg = StreamAggregator::new();
        agg.absorb(&StreamEvent::ToolCallStart {
            index: 0,
            id: "c".into(),
            name: "f".into(),
            signature: None,
        });
        agg.absorb(&StreamEvent::ToolCallArgsDelta {
            index: 0,
            fragment: r#"{"partial":"#.into(),
        });
        let content = agg.into_content();
        match &content[0] {
            ContentPart::ToolUse { input, .. } => {
                assert!(
                    input.is_string(),
                    "expected raw string fallback, got {input:?}"
                );
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn aggregator_takes_max_not_sum_of_usage() {
        // Anthropic 语义：message_start 给 input，message_delta 给累积 output。
        // 累加会导致 input 被重复计数。
        let mut agg = StreamAggregator::new();
        agg.absorb(&StreamEvent::Start {
            id: "m".into(),
            model: "claude".into(),
            usage: Some(Usage {
                input_tokens: 100,
                output_tokens: 1,
                ..Default::default()
            }),
        });
        agg.absorb(&StreamEvent::Usage(Usage {
            input_tokens: 100,
            output_tokens: 57,
            ..Default::default()
        }));
        assert_eq!(agg.usage.input_tokens, 100);
        assert_eq!(agg.usage.output_tokens, 57);
    }

    #[test]
    fn aggregator_tolerates_missing_content_start() {
        // 中转站常省略仪式性事件，直接发 delta。
        let mut agg = StreamAggregator::new();
        agg.absorb(&StreamEvent::TextDelta {
            index: 0,
            text: "bare".into(),
        });
        assert_eq!(agg.into_content(), vec![ContentPart::text("bare")]);
    }

    #[test]
    fn aggregator_preserves_thinking_signature() {
        let mut agg = StreamAggregator::new();
        agg.absorb(&StreamEvent::ContentStart {
            index: 0,
            kind: PartKind::Thinking,
        });
        agg.absorb(&StreamEvent::ThinkingDelta {
            index: 0,
            text: "hmm".into(),
        });
        agg.absorb(&StreamEvent::ThinkingSignature {
            index: 0,
            signature: "sig_xyz".into(),
        });
        assert_eq!(
            agg.into_content(),
            vec![ContentPart::Thinking {
                text: "hmm".into(),
                signature: Some("sig_xyz".into()),
            }]
        );
    }

    #[test]
    fn aggregator_handles_sparse_block_indices() {
        // 上游不保证块下标从 0 连续递增。
        let mut agg = StreamAggregator::new();
        agg.absorb(&StreamEvent::TextDelta {
            index: 2,
            text: "third".into(),
        });
        let content = agg.into_content();
        // 空块被丢弃，只留有内容的。
        assert_eq!(content, vec![ContentPart::text("third")]);
    }

    #[test]
    fn terminal_events_are_flagged() {
        assert!(StreamEvent::Done.is_terminal());
        assert!(
            StreamEvent::Error {
                message: "x".into(),
                kind: "overloaded_error".into()
            }
            .is_terminal()
        );
        assert!(!StreamEvent::Ping.is_terminal());
    }
}
