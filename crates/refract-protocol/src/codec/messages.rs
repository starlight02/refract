//! Anthropic Messages API 协议编解码。
//!
//! 对应 `POST /v1/messages`。这个协议与其他三家最大的结构性差异有三处，
//! 编解码时必须正面处理，否则转换出来的请求会被上游直接拒绝：
//!
//! 1. **`max_tokens` 必填**。其他协议都可以省略，所以从它们转过来时必须补默认值。
//! 2. **没有 `tool` 角色**。工具结果是 `role:"user"` 消息里的 `tool_result` block，
//!    所以 IR 的 [`Role::Tool`] 消息要合并进 user 消息。
//! 3. **不允许连续同角色消息**。OpenAI 那边一次并行工具调用会产生 N 条
//!    `role:"tool"` 消息，直接映射过来就是 N 条连续 user 消息 —— 必须合并。
//!
//! 另外 [`ContentPart::Thinking`] 的 `signature` 是 Anthropic 独有的，且在多轮
//! 工具调用中**丢了就会被上游拒整个请求**，所以本 codec 全链路（非流式与流式）
//! 都保证它无损。

use refract_core::{GatewayError, Protocol};
use serde_json::{Map, Value, json};

use crate::codec::{ProtocolCodec, RequestCodec, ResponseCodec, StreamCodec};
use crate::ir::{
    ContentPart, Extensions, MediaSource, Message, ReasoningConfig, Role, Sampling, StopReason,
    ToolChoice, ToolDef, UnifiedRequest, UnifiedResponse, Usage,
};
use crate::stream::{PartKind, SseFrame, StreamDecoder, StreamEncoder, StreamEvent};

/// Anthropic Messages codec。
pub struct MessagesCodec;

/// 供 `CodecSet` 注册的单例。
pub static MESSAGES: MessagesCodec = MessagesCodec;

/// 扩展键前缀。本协议只认自己这一份，其他协议的键原样留在 IR 里。
const EXT: &str = "messages.";

/// `max_tokens` 缺失时的兜底值。
///
/// Anthropic 要求必填，而 OpenAI/Gemini 允许省略（含义是「模型上限」）。
/// 这里取一个既不会截断常规回答、又不会让上游因超模型窗口而报错的中间值。
const DEFAULT_MAX_TOKENS: u32 = 4_096;

/// Gemini「动态思考」（`thinkingBudget: -1`）折算到 Anthropic 的默认预算。
///
/// Anthropic 没有「让模型自己决定」的档位，只能给个定值。取值参考
/// Anthropic 文档对复杂任务的建议起点，再由 [`encode_thinking`] 按
/// `max_tokens` clamp。
const DYNAMIC_THINKING_BUDGET: u32 = 8_192;

/// 请求里被本 codec 显式消费掉的顶层字段，其余的进 extensions。
const KNOWN_REQUEST_FIELDS: &[&str] = &[
    "model",
    "max_tokens",
    "stream",
    "system",
    "messages",
    "tools",
    "tool_choice",
    "thinking",
    "temperature",
    "top_p",
    "top_k",
    "stop_sequences",
    "metadata",
];

/// 响应里被本 codec 显式消费掉的顶层字段。
const KNOWN_RESPONSE_FIELDS: &[&str] = &[
    "id",
    "type",
    "role",
    "model",
    "content",
    "stop_reason",
    "stop_sequence",
    "usage",
];

// ---------------------------------------------------------------------------
// 请求
// ---------------------------------------------------------------------------

impl RequestCodec for MessagesCodec {
    fn decode_request(&self, raw: &Value) -> Result<UnifiedRequest, GatewayError> {
        let obj = raw
            .as_object()
            .ok_or_else(|| GatewayError::invalid_request("request body must be a JSON object"))?;

        let model = obj
            .get("model")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| GatewayError::invalid_request("field `model` is required"))?;

        // Anthropic 的 max_tokens 是必填的，缺了要在网关就拦下来 ——
        // 放过去只会换成一条上游 400，那对客户端来说更难排查。
        let max_tokens = match obj.get("max_tokens") {
            Some(Value::Number(n)) => n.as_u64().map(|v| v as u32).ok_or_else(|| {
                GatewayError::invalid_request("field `max_tokens` must be a positive integer")
            })?,
            Some(_) => {
                return Err(GatewayError::invalid_request(
                    "field `max_tokens` must be a positive integer",
                ));
            }
            None => {
                return Err(GatewayError::invalid_request(
                    "field `max_tokens` is required by the Anthropic Messages API",
                ));
            }
        };

        let mut ir = UnifiedRequest::new(model, Vec::new());
        ir.max_output_tokens = Some(max_tokens);
        ir.stream = obj.get("stream").and_then(Value::as_bool).unwrap_or(false);
        // Anthropic 的流式 usage 是协议自带的（message_start / message_delta 都带），
        // 不像 OpenAI 需要 stream_options 显式开启。
        ir.stream_include_usage = ir.stream;

        if let Some(system) = obj.get("system") {
            ir.system = decode_system(system)?;
            // 带 cache_control 断点的 system 原文存档：IR 表达不了 block 级
            // 缓存标记，直通回 messages 时丢断点会让缓存全失效，成本差一个
            // 数量级。编码器碰到同协议时优先用原文还原。
            if has_cache_control(system) {
                ir.set_extension(format!("{EXT}system_raw"), system.clone());
            }
        }

        let messages = obj
            .get("messages")
            .and_then(Value::as_array)
            .ok_or_else(|| GatewayError::invalid_request("field `messages` must be an array"))?;
        for (i, raw_msg) in messages.iter().enumerate() {
            ir.messages.push(decode_message(raw_msg, i)?);
        }

        if let Some(tools) = obj.get("tools").and_then(Value::as_array) {
            let mut function_tools_raw = Vec::new();
            let mut tools_have_cache_control = false;
            for raw_tool in tools {
                // 服务端工具（web_search / computer 之类）只有 `type`，没有
                // input_schema，无法表达成 ToolDef —— 整条塞进 extensions 保真。
                if raw_tool.get("input_schema").is_none() {
                    let bucket = ir
                        .extensions
                        .entry(format!("{EXT}server_tools"))
                        .or_insert_with(|| Value::Array(Vec::new()));
                    if let Some(arr) = bucket.as_array_mut() {
                        arr.push(raw_tool.clone());
                    }
                    continue;
                }
                tools_have_cache_control |= has_cache_control(raw_tool);
                function_tools_raw.push(raw_tool.clone());
                ir.tools.push(decode_tool(raw_tool)?);
            }
            // 与 system 同理：tools 定义是缓存断点的常客（大工具集），
            // 带 cache_control 时存原文供直通还原。
            if tools_have_cache_control {
                ir.set_extension(format!("{EXT}tools_raw"), Value::Array(function_tools_raw));
            }
        }

        if let Some(choice) = obj.get("tool_choice") {
            let (tc, parallel) = decode_tool_choice(choice);
            ir.tool_choice = tc;
            ir.parallel_tool_calls = parallel;
        }

        ir.sampling = Sampling {
            temperature: obj.get("temperature").and_then(Value::as_f64),
            top_p: obj.get("top_p").and_then(Value::as_f64),
            top_k: obj.get("top_k").and_then(Value::as_u64).map(|v| v as u32),
            stop: obj
                .get("stop_sequences")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
            ..Sampling::default()
        };

        if let Some(thinking) = obj.get("thinking") {
            ir.reasoning = decode_thinking(thinking);
        }

        // Anthropic 把终端用户标识放在 metadata.user_id。
        ir.user = obj
            .get("metadata")
            .and_then(|m| m.get("user_id"))
            .and_then(Value::as_str)
            .map(str::to_owned);

        // metadata 里除 user_id 之外的字段（Anthropic 允许自定义）不能丢。
        if let Some(meta) = obj.get("metadata").and_then(Value::as_object) {
            let rest: Map<String, Value> = meta
                .iter()
                .filter(|(k, _)| k.as_str() != "user_id")
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            if !rest.is_empty() {
                ir.set_extension(format!("{EXT}metadata"), Value::Object(rest));
            }
        }

        collect_unknown(obj, KNOWN_REQUEST_FIELDS, &mut ir.extensions);
        Ok(ir)
    }

    fn encode_request(&self, ir: &UnifiedRequest) -> Result<Value, GatewayError> {
        let max_tokens = ir.max_output_tokens.unwrap_or_else(|| {
            tracing::debug!(
                default = DEFAULT_MAX_TOKENS,
                "inbound request omitted max_output_tokens; Anthropic requires max_tokens"
            );
            DEFAULT_MAX_TOKENS
        });

        let mut out = Map::new();
        out.insert("model".into(), json!(ir.model));
        out.insert("max_tokens".into(), json!(max_tokens));
        if ir.stream {
            out.insert("stream".into(), json!(true));
        }

        // messages→messages 直通时优先还原带 cache_control 的原文（IR 装不下
        // block 级缓存断点）。跨协议来的请求没有这些 extension，走正常构造。
        if let Some(system_raw) = ir.extension(&format!("{EXT}system_raw")) {
            out.insert("system".into(), system_raw.clone());
        } else if !ir.system.is_empty() {
            out.insert("system".into(), encode_system(&ir.system));
        }
        out.insert(
            "messages".into(),
            Value::Array(encode_messages(&ir.messages)),
        );

        if let Some(Value::Array(tools_raw)) = ir.extension(&format!("{EXT}tools_raw")) {
            out.insert("tools".into(), Value::Array(tools_raw.clone()));
        } else if !ir.tools.is_empty() {
            let tools: Vec<Value> = ir.tools.iter().map(encode_tool).collect();
            out.insert("tools".into(), Value::Array(tools));
        }
        // 服务端工具原样追加回 tools 数组。
        if let Some(Value::Array(server)) = ir.extension(&format!("{EXT}server_tools")) {
            let slot = out
                .entry("tools".to_string())
                .or_insert_with(|| Value::Array(Vec::new()));
            if let Some(arr) = slot.as_array_mut() {
                arr.extend(server.iter().cloned());
            }
        }

        if let Some(choice) = encode_tool_choice(&ir.tool_choice, ir.parallel_tool_calls) {
            out.insert("tool_choice".into(), choice);
        }

        // thinking 是否最终启用 —— 决定采样参数能不能带。
        let thinking = ir
            .reasoning
            .as_ref()
            .map(|r| encode_thinking(r, Some(max_tokens)));
        let thinking_enabled = thinking
            .as_ref()
            .and_then(|t| t.get("type"))
            .and_then(Value::as_str)
            == Some("enabled");

        // Anthropic 硬性约束：extended thinking 开启时 temperature 只能是 1
        // （等于不设）、top_k 不允许、top_p 只允许 [0.95, 1]。跨协议转来的
        // 采样参数原样透传会让整个请求被 400，只能剥离。
        if let Some(t) = ir.sampling.temperature {
            if thinking_enabled && (t - 1.0).abs() > f64::EPSILON {
                tracing::debug!(temperature = t, "thinking 开启时不支持 temperature，已剥离");
            } else {
                out.insert("temperature".into(), json!(t));
            }
        }
        if let Some(p) = ir.sampling.top_p {
            if thinking_enabled && p < 0.95 {
                tracing::debug!(top_p = p, "thinking 开启时 top_p 须在 [0.95,1]，已剥离");
            } else {
                out.insert("top_p".into(), json!(p));
            }
        }
        if let Some(k) = ir.sampling.top_k {
            if thinking_enabled {
                tracing::debug!(top_k = k, "thinking 开启时不支持 top_k，已剥离");
            } else {
                out.insert("top_k".into(), json!(k));
            }
        }
        if !ir.sampling.stop.is_empty() {
            out.insert("stop_sequences".into(), json!(ir.sampling.stop));
        }
        if ir.sampling.frequency_penalty.is_some()
            || ir.sampling.presence_penalty.is_some()
            || ir.sampling.seed.is_some()
            || ir.sampling.candidate_count.is_some()
        {
            tracing::debug!(
                "Anthropic Messages has no frequency_penalty/presence_penalty/seed/candidate_count; dropped"
            );
        }

        if let Some(thinking) = thinking {
            out.insert("thinking".into(), thinking);
        }

        // metadata：先还原扩展里的自定义字段，再叠加 user。
        let mut metadata = ir
            .extension(&format!("{EXT}metadata"))
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        if let Some(user) = &ir.user {
            metadata.insert("user_id".into(), json!(user));
        }
        if !metadata.is_empty() {
            out.insert("metadata".into(), Value::Object(metadata));
        }

        if ir.response_format.is_some() {
            // Anthropic 没有 response_format —— 结构化输出靠工具或提示词实现。
            tracing::debug!("Anthropic Messages has no response_format field; dropped");
        }

        restore_extensions(
            &ir.extensions,
            &mut out,
            &["server_tools", "metadata", "system_raw", "tools_raw"],
        );
        Ok(Value::Object(out))
    }
}

/// 该 JSON 值（对象或对象数组）是否携带 `cache_control` 标记。
///
/// 用于决定要不要为直通保真留存原文 —— 只在真的有缓存断点时才多存一份，
/// 避免所有请求都背上双倍内存。
fn has_cache_control(value: &Value) -> bool {
    match value {
        Value::Object(obj) => obj.contains_key("cache_control"),
        Value::Array(items) => items.iter().any(has_cache_control),
        _ => false,
    }
}

/// 解码 `system`：字符串或 content block 数组。
fn decode_system(raw: &Value) -> Result<Vec<ContentPart>, GatewayError> {
    match raw {
        Value::String(s) if s.is_empty() => Ok(Vec::new()),
        Value::String(s) => Ok(vec![ContentPart::text(s)]),
        Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.extend(decode_block(item)?);
            }
            Ok(out)
        }
        Value::Null => Ok(Vec::new()),
        _ => Err(GatewayError::invalid_request(
            "field `system` must be a string or an array of content blocks",
        )),
    }
}

/// 编码 `system`：纯文本时压成字符串，否则用 block 数组。
///
/// 压成字符串不只是为了紧凑 —— 不少中转站只实现了字符串形式的 system。
fn encode_system(parts: &[ContentPart]) -> Value {
    let all_text = parts.iter().all(|p| matches!(p, ContentPart::Text { .. }));
    if all_text {
        let mut buf = String::new();
        for part in parts {
            if let ContentPart::Text { text } = part {
                if !buf.is_empty() {
                    buf.push('\n');
                }
                buf.push_str(text);
            }
        }
        return Value::String(buf);
    }
    Value::Array(parts.iter().filter_map(encode_block).collect())
}

fn decode_message(raw: &Value, position: usize) -> Result<Message, GatewayError> {
    let obj = raw.as_object().ok_or_else(|| {
        GatewayError::invalid_request(format!("messages[{position}] must be an object"))
    })?;
    let role = match obj.get("role").and_then(Value::as_str) {
        Some("user") => Role::User,
        Some("assistant") => Role::Assistant,
        // 宽容：有些客户端会误发 system 角色消息，收下来当 system 内容更实用。
        Some("system") => Role::System,
        Some(other) => {
            return Err(GatewayError::invalid_request(format!(
                "messages[{position}].role must be `user` or `assistant`, got `{other}`"
            )));
        }
        None => {
            return Err(GatewayError::invalid_request(format!(
                "messages[{position}].role is required"
            )));
        }
    };

    let content = match obj.get("content") {
        Some(Value::String(s)) => vec![ContentPart::text(s)],
        Some(Value::Array(items)) => {
            let mut parts = Vec::with_capacity(items.len());
            for item in items {
                parts.extend(decode_block(item)?);
            }
            parts
        }
        // 空 content 是合法的（比如被截断的 assistant 回合）。
        Some(Value::Null) | None => Vec::new(),
        Some(_) => {
            return Err(GatewayError::invalid_request(format!(
                "messages[{position}].content must be a string or an array of content blocks"
            )));
        }
    };

    // 只含 tool_result 的 user 消息在 IR 里归为 Role::Tool，这样转到 OpenAI
    // Chat 时能直接映射成 role:"tool" 消息。
    let role = if role == Role::User
        && !content.is_empty()
        && content
            .iter()
            .all(|p| matches!(p, ContentPart::ToolResult { .. }))
    {
        Role::Tool
    } else {
        role
    };

    Ok(Message::new(role, content))
}

/// 解码单个 content block。
///
/// 返回 `Vec` 而不是单个 part：便于把某些形态展开或吞掉；未知 block
/// 类型会包成 [`ContentPart::Opaque`] 原样保留。
fn decode_block(raw: &Value) -> Result<Vec<ContentPart>, GatewayError> {
    // 宽容：有些客户端在 block 数组里塞裸字符串。
    if let Value::String(s) = raw {
        return Ok(vec![ContentPart::text(s)]);
    }
    let obj = raw
        .as_object()
        .ok_or_else(|| GatewayError::invalid_request("content block must be an object"))?;

    let kind = obj.get("type").and_then(Value::as_str).unwrap_or_default();
    let part = match kind {
        "text" => ContentPart::text(obj.get("text").and_then(Value::as_str).unwrap_or_default()),
        "image" => {
            let (source, mime) = decode_source(obj.get("source"))?;
            ContentPart::Image {
                source,
                mime,
                detail: None,
            }
        }
        "document" => {
            let (source, mime) = decode_source(obj.get("source"))?;
            ContentPart::File {
                source,
                mime,
                name: obj.get("title").and_then(Value::as_str).map(str::to_owned),
            }
        }
        "thinking" => ContentPart::Thinking {
            text: obj
                .get("thinking")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            // signature 必须原样保留：多轮工具调用时丢了它上游会拒整个请求。
            signature: obj
                .get("signature")
                .and_then(Value::as_str)
                .map(str::to_owned),
        },
        "redacted_thinking" => ContentPart::RedactedThinking {
            data: obj
                .get("data")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        },
        "tool_use" => ContentPart::ToolUse {
            id: obj
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| GatewayError::invalid_request("tool_use block requires `id`"))?
                .to_owned(),
            name: obj
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| GatewayError::invalid_request("tool_use block requires `name`"))?
                .to_owned(),
            input: obj.get("input").cloned().unwrap_or_else(|| json!({})),
            signature: None,
        },
        "tool_result" => ContentPart::ToolResult {
            id: obj
                .get("tool_use_id")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    GatewayError::invalid_request("tool_result block requires `tool_use_id`")
                })?
                .to_owned(),
            // Anthropic 的 tool_result 不带函数名；转 Gemini 时由编码侧
            // 从对话历史反查。
            name: None,
            content: decode_tool_result_content(obj.get("content"))?,
            is_error: obj
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        },
        "" => {
            return Err(GatewayError::invalid_request(
                "content block requires a `type` field",
            ));
        }
        other => {
            // 未知 block（server_tool_use / web_search_tool_result / 新模态）
            // 不丢弃：Anthropic 要求多轮对话原样回传服务端工具块，丢了会
            // 损坏对话历史。包成 Opaque，messages→messages 直通无损，
            // 跨协议时由目标编码器丢弃。
            tracing::debug!(block_type = other, "opaque Anthropic content block");
            return Ok(vec![ContentPart::Opaque {
                protocol: "messages".to_owned(),
                value: raw.clone(),
            }]);
        }
    };
    Ok(vec![part])
}

/// `tool_result.content` 可以是字符串，也可以是 block 数组（含图片）。
fn decode_tool_result_content(raw: Option<&Value>) -> Result<Vec<ContentPart>, GatewayError> {
    match raw {
        Some(Value::String(s)) => Ok(vec![ContentPart::text(s)]),
        Some(Value::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.extend(decode_block(item)?);
            }
            Ok(out)
        }
        // 工具可以什么都不返回。
        Some(Value::Null) | None => Ok(Vec::new()),
        Some(other) => Ok(vec![ContentPart::text(other.to_string())]),
    }
}

/// 解码 `source` 对象为 [`MediaSource`] 与 MIME。
fn decode_source(raw: Option<&Value>) -> Result<(MediaSource, Option<String>), GatewayError> {
    let obj = raw
        .and_then(Value::as_object)
        .ok_or_else(|| GatewayError::invalid_request("media block requires a `source` object"))?;
    let mime = obj
        .get("media_type")
        .and_then(Value::as_str)
        .map(str::to_owned);
    match obj.get("type").and_then(Value::as_str) {
        Some("base64") => {
            let data = obj.get("data").and_then(Value::as_str).ok_or_else(|| {
                GatewayError::invalid_request("base64 source requires a `data` field")
            })?;
            Ok((MediaSource::Base64(data.to_owned()), mime))
        }
        Some("url") => {
            let url = obj.get("url").and_then(Value::as_str).ok_or_else(|| {
                GatewayError::invalid_request("url source requires a `url` field")
            })?;
            Ok((MediaSource::Url(url.to_owned()), mime))
        }
        Some("file") => {
            let id = obj.get("file_id").and_then(Value::as_str).ok_or_else(|| {
                GatewayError::invalid_request("file source requires a `file_id` field")
            })?;
            Ok((MediaSource::FileId(id.to_owned()), mime))
        }
        // `text` 源用于纯文本文档，内容直接在 data 里。
        Some("text") => {
            let data = obj.get("data").and_then(Value::as_str).unwrap_or_default();
            Ok((MediaSource::Base64(data.to_owned()), mime))
        }
        Some(other) => Err(GatewayError::invalid_request(format!(
            "unsupported source type `{other}`"
        ))),
        None => Err(GatewayError::invalid_request(
            "media `source` requires a `type` field",
        )),
    }
}

/// 编码 [`MediaSource`] 为 Anthropic 的 `source` 对象。
fn encode_source(source: &MediaSource, mime: Option<&str>) -> Value {
    match source {
        MediaSource::Base64(data) => json!({
            "type": "base64",
            // base64 图片的 media_type 是必填的；缺失时给个不会让上游炸的兜底。
            "media_type": mime.unwrap_or("application/octet-stream"),
            "data": data,
        }),
        MediaSource::Url(url) => {
            // 从其他协议转过来时 URL 可能本身就是 data URI，要还原成 base64 源。
            let (parsed, parsed_mime) = MediaSource::parse_data_uri(url);
            match parsed {
                MediaSource::Base64(data) => json!({
                    "type": "base64",
                    "media_type": parsed_mime
                        .as_deref()
                        .or(mime)
                        .unwrap_or("application/octet-stream"),
                    "data": data,
                }),
                _ => json!({ "type": "url", "url": url }),
            }
        }
        MediaSource::FileId(id) => json!({ "type": "file", "file_id": id }),
    }
}

/// 编码单个 [`ContentPart`] 为 Anthropic content block。
///
/// 返回 `None` 表示该片段 Anthropic 无法表达（跨协议的 Opaque 块），
/// 调用方直接跳过。
fn encode_block(part: &ContentPart) -> Option<Value> {
    Some(match part {
        ContentPart::Text { text } => json!({ "type": "text", "text": text }),
        ContentPart::Image {
            source,
            mime,
            detail: _,
        } => json!({
            "type": "image",
            "source": encode_source(source, mime.as_deref()),
        }),
        ContentPart::File { source, mime, name } => {
            let mut block = json!({
                "type": "document",
                "source": encode_source(source, mime.as_deref()),
            });
            if let (Some(obj), Some(name)) = (block.as_object_mut(), name) {
                obj.insert("title".into(), json!(name));
            }
            block
        }
        ContentPart::Audio { source, format } => {
            // Anthropic 目前不收音频，降级成 document 让上游至少能看到数据，
            // 而不是静默丢掉用户的输入。
            tracing::debug!("Anthropic Messages has no audio block; encoded as document");
            json!({
                "type": "document",
                "source": encode_source(source, format.as_deref()),
            })
        }
        ContentPart::Thinking { text, signature } => {
            let mut block = json!({ "type": "thinking", "thinking": text });
            if let (Some(obj), Some(sig)) = (block.as_object_mut(), signature) {
                obj.insert("signature".into(), json!(sig));
            }
            block
        }
        ContentPart::RedactedThinking { data } => {
            json!({ "type": "redacted_thinking", "data": data })
        }
        ContentPart::ToolUse {
            id, name, input, ..
        } => json!({
            "type": "tool_use",
            "id": id,
            "name": name,
            "input": input,
        }),
        ContentPart::ToolResult {
            id,
            content,
            is_error,
            ..
        } => {
            let mut block = json!({
                "type": "tool_result",
                "tool_use_id": id,
                "content": encode_tool_result_content(content),
            });
            if *is_error && let Some(obj) = block.as_object_mut() {
                obj.insert("is_error".into(), json!(true));
            }
            block
        }
        // Anthropic 没有 refusal block，退化成文本 —— 拒答理由对用户有意义，
        // 丢掉会让对话看起来凭空断了一轮。
        ContentPart::Refusal { text } => json!({ "type": "text", "text": text }),
        ContentPart::Opaque { protocol, value } => {
            // 同协议直通原样还原（server_tool_use 等必须回传的块）；
            // 跨协议的私有块塞给 Anthropic 必被拒，丢弃。
            if protocol == "messages" {
                value.clone()
            } else {
                tracing::debug!(%protocol, "非 messages 的 Opaque 块无法表达，已丢弃");
                return None;
            }
        }
    })
}

/// 工具结果内容：纯文本压成字符串，含图片时用 block 数组。
fn encode_tool_result_content(content: &[ContentPart]) -> Value {
    if content.is_empty() {
        return Value::String(String::new());
    }
    if content
        .iter()
        .all(|p| matches!(p, ContentPart::Text { .. }))
    {
        let mut buf = String::new();
        for part in content {
            if let ContentPart::Text { text } = part {
                buf.push_str(text);
            }
        }
        return Value::String(buf);
    }
    Value::Array(content.iter().filter_map(encode_block).collect())
}

/// 编码消息数组，处理 Anthropic 的两条硬性结构约束。
///
/// 1. [`Role::Tool`] 消息没有对应角色 → 变成 user 消息里的 `tool_result` block。
/// 2. 不允许连续同角色消息 → 相邻同角色合并成一条。
///
/// 第 2 条在实践中一定会撞上：OpenAI 一次并行工具调用会回 N 条 `role:"tool"`
/// 消息，逐条映射就是 N 条连续 user 消息，上游直接 400。
fn encode_messages(messages: &[Message]) -> Vec<Value> {
    // (role, blocks)，role 只会是 "user" 或 "assistant"。
    let mut merged: Vec<(&'static str, Vec<Value>)> = Vec::with_capacity(messages.len());

    for msg in messages {
        let role = match msg.role {
            // System 消息本该在 UnifiedRequest.system 里；万一混进 messages，
            // 当 user 处理总比丢掉好。
            Role::User | Role::System => "user",
            Role::Assistant => "assistant",
            Role::Tool => "user",
        };
        let blocks: Vec<Value> = msg.content.iter().filter_map(encode_block).collect();
        if blocks.is_empty() {
            // 空 content 的消息在 Anthropic 侧是非法的，跳过。
            continue;
        }
        match merged.last_mut() {
            Some((prev_role, prev_blocks)) if *prev_role == role => prev_blocks.extend(blocks),
            _ => merged.push((role, blocks)),
        }
    }

    // Anthropic 要求第一条消息必须是 user。别的协议没有这个约束（OpenAI
    // 允许对话以 assistant 开头），转换过来时垫一条最小占位，否则整个
    // 请求被上游 400。
    if merged.first().is_some_and(|(role, _)| *role == "assistant") {
        merged.insert(0, ("user", vec![json!({ "type": "text", "text": "." })]));
        tracing::debug!("messages: 对话以 assistant 开头，已垫入占位 user 消息");
    }

    merged
        .into_iter()
        .map(|(role, blocks)| {
            // 单个纯文本块压成字符串，兼容实现不完整的中转站。
            let content = match blocks.as_slice() {
                [only] if only.get("type").and_then(Value::as_str) == Some("text") => only
                    .get("text")
                    .cloned()
                    .unwrap_or_else(|| Value::String(String::new())),
                _ => Value::Array(blocks),
            };
            json!({ "role": role, "content": content })
        })
        .collect()
}

fn decode_tool(raw: &Value) -> Result<ToolDef, GatewayError> {
    let name = raw
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| GatewayError::invalid_request("each tool requires a `name`"))?;
    Ok(ToolDef {
        name: name.to_owned(),
        description: raw
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_owned),
        // 注意字段名：Anthropic 叫 input_schema，OpenAI 叫 parameters。
        parameters: raw
            .get("input_schema")
            .cloned()
            .unwrap_or_else(|| json!({ "type": "object", "properties": {} })),
        strict: None,
    })
}

fn encode_tool(tool: &ToolDef) -> Value {
    let mut out = json!({
        "name": tool.name,
        "input_schema": tool.parameters,
    });
    if let (Some(obj), Some(desc)) = (out.as_object_mut(), &tool.description) {
        obj.insert("description".into(), json!(desc));
    }
    if tool.strict.is_some() {
        tracing::debug!("Anthropic tools have no `strict` flag; dropped");
    }
    out
}

/// 解码 `tool_choice`，同时抽出 `disable_parallel_tool_use`。
fn decode_tool_choice(raw: &Value) -> (ToolChoice, Option<bool>) {
    // Anthropic 的语义是「禁用并行」，IR 是「允许并行」，取反。
    let parallel = raw
        .get("disable_parallel_tool_use")
        .and_then(Value::as_bool)
        .map(|disabled| !disabled);
    let choice = match raw.get("type").and_then(Value::as_str) {
        Some("auto") => ToolChoice::Auto,
        // `any` = 必须用工具，但不指定哪个。
        Some("any") => ToolChoice::Required,
        Some("none") => ToolChoice::None,
        Some("tool") => match raw.get("name").and_then(Value::as_str) {
            Some(name) => ToolChoice::Tool(name.to_owned()),
            // type:"tool" 缺 name 是客户端 bug，退化成 Required 比报错友好。
            None => ToolChoice::Required,
        },
        _ => ToolChoice::Unspecified,
    };
    (choice, parallel)
}

fn encode_tool_choice(choice: &ToolChoice, parallel: Option<bool>) -> Option<Value> {
    let mut out = match choice {
        ToolChoice::Unspecified => {
            // 没指定策略但显式要求禁用并行时，仍要发出 tool_choice 才能带上该标志。
            if parallel == Some(false) {
                json!({ "type": "auto" })
            } else {
                return None;
            }
        }
        ToolChoice::Auto => json!({ "type": "auto" }),
        ToolChoice::Required => json!({ "type": "any" }),
        ToolChoice::None => json!({ "type": "none" }),
        ToolChoice::Tool(name) => json!({ "type": "tool", "name": name }),
    };
    if let (Some(obj), Some(allowed)) = (out.as_object_mut(), parallel)
        && !allowed
    {
        obj.insert("disable_parallel_tool_use".into(), json!(true));
    }
    Some(out)
}

fn decode_thinking(raw: &Value) -> Option<ReasoningConfig> {
    match raw.get("type").and_then(Value::as_str) {
        Some("enabled") => Some(ReasoningConfig {
            effort: None,
            budget_tokens: raw
                .get("budget_tokens")
                .and_then(Value::as_u64)
                .map(|v| v as u32),
            include_thoughts: Some(true),
        }),
        Some("disabled") => Some(ReasoningConfig {
            effort: Some("none".into()),
            budget_tokens: None,
            include_thoughts: Some(false),
        }),
        _ => None,
    }
}

fn encode_thinking(cfg: &ReasoningConfig, max_output: Option<u32>) -> Value {
    if cfg.include_thoughts == Some(false) {
        return json!({ "type": "disabled" });
    }
    // 从 OpenAI 转过来时只有 effort，必须折算成预算，否则思考会被静默关掉。
    // Gemini 的「动态思考」（thinkingBudget:-1）解码后 budget 为 None 但
    // include_thoughts 为 true —— 这时给一个中档默认预算，禁用是错误语义。
    let budget = cfg
        .budget_or_from_effort(max_output)
        .or_else(|| (cfg.include_thoughts == Some(true)).then_some(DYNAMIC_THINKING_BUDGET));
    let Some(budget) = budget else {
        return json!({ "type": "disabled" });
    };
    // Anthropic 的硬性约束：1024 <= budget_tokens < max_tokens。
    // 跨协议来的预算可能越界（Gemini 上限 32768 且与 max 无关），直接
    // 透传会被上游 400，必须 clamp。max_tokens 太小放不下最小预算时，
    // 只能禁用思考 —— 这是两个约束共同决定的，没有第三种选择。
    let Some(ceiling) = max_output.map(|m| m.saturating_sub(1)) else {
        return json!({ "type": "enabled", "budget_tokens": budget.max(1024) });
    };
    if ceiling < 1024 {
        tracing::debug!(
            max_output,
            "max_tokens 容不下最小思考预算（1024），thinking 已禁用"
        );
        return json!({ "type": "disabled" });
    }
    let clamped = budget.clamp(1024, ceiling);
    if clamped != budget {
        tracing::debug!(budget, clamped, "thinking 预算越界，已收敛到合法区间");
    }
    json!({ "type": "enabled", "budget_tokens": clamped })
}

// ---------------------------------------------------------------------------
// 响应
// ---------------------------------------------------------------------------

impl ResponseCodec for MessagesCodec {
    fn decode_response(&self, raw: &Value) -> Result<UnifiedResponse, GatewayError> {
        let obj = raw
            .as_object()
            .ok_or_else(|| GatewayError::invalid_request("response body must be a JSON object"))?;

        // 上游可能返回错误体而不是消息体，要翻译成带正确 kind 的 GatewayError。
        if obj.get("type").and_then(Value::as_str) == Some("error") {
            return Err(decode_error_body(obj));
        }

        let id = obj.get("id").and_then(Value::as_str).unwrap_or_default();
        let model = obj.get("model").and_then(Value::as_str).unwrap_or_default();
        let mut out = UnifiedResponse::new(id, model);

        if let Some(items) = obj.get("content").and_then(Value::as_array) {
            for item in items {
                out.content.extend(decode_block(item)?);
            }
        }

        out.stop_reason = obj
            .get("stop_reason")
            .and_then(Value::as_str)
            .map(decode_stop_reason);
        out.stop_sequence = obj
            .get("stop_sequence")
            .and_then(Value::as_str)
            .map(str::to_owned);
        out.usage = obj.get("usage").map(decode_usage).unwrap_or_default();

        collect_unknown(obj, KNOWN_RESPONSE_FIELDS, &mut out.extensions);
        Ok(out)
    }

    fn encode_response(&self, ir: &UnifiedResponse) -> Result<Value, GatewayError> {
        let mut out = Map::new();
        out.insert("id".into(), json!(ir.id));
        out.insert("type".into(), json!("message"));
        out.insert("role".into(), json!("assistant"));
        out.insert("model".into(), json!(ir.model));
        out.insert(
            "content".into(),
            Value::Array(ir.content.iter().filter_map(encode_block).collect()),
        );
        out.insert(
            "stop_reason".into(),
            match ir.stop_reason {
                Some(reason) => json!(encode_stop_reason(reason)),
                None => Value::Null,
            },
        );
        out.insert(
            "stop_sequence".into(),
            match &ir.stop_sequence {
                Some(seq) => json!(seq),
                None => Value::Null,
            },
        );
        out.insert("usage".into(), encode_usage(&ir.usage));

        restore_extensions(&ir.extensions, &mut out, &[]);
        Ok(Value::Object(out))
    }
}

fn decode_stop_reason(raw: &str) -> StopReason {
    match raw {
        "end_turn" => StopReason::Stop,
        "max_tokens" => StopReason::MaxTokens,
        "stop_sequence" => StopReason::StopSequence,
        "tool_use" => StopReason::ToolUse,
        "refusal" => StopReason::Refusal,
        "pause_turn" => StopReason::PauseTurn,
        other => {
            tracing::debug!(stop_reason = other, "unknown Anthropic stop_reason");
            StopReason::Other
        }
    }
}

fn encode_stop_reason(reason: StopReason) -> &'static str {
    match reason {
        StopReason::Stop => "end_turn",
        StopReason::MaxTokens => "max_tokens",
        StopReason::StopSequence => "stop_sequence",
        StopReason::ToolUse => "tool_use",
        StopReason::Refusal => "refusal",
        StopReason::PauseTurn => "pause_turn",
        // Anthropic 没有内容过滤这个停止原因；它用 refusal 表达安全拦截。
        StopReason::ContentFilter => "refusal",
        StopReason::Other => "end_turn",
    }
}

fn decode_usage(raw: &Value) -> Usage {
    let n = |key: &str| raw.get(key).and_then(Value::as_u64).unwrap_or(0);
    Usage {
        input_tokens: n("input_tokens"),
        output_tokens: n("output_tokens"),
        cached_input_tokens: n("cache_read_input_tokens"),
        cache_write_tokens: n("cache_creation_input_tokens"),
        // Anthropic 不单独报推理 token，它已包含在 output_tokens 里。
        reasoning_tokens: 0,
    }
}

fn encode_usage(usage: &Usage) -> Value {
    let mut out = json!({
        "input_tokens": usage.input_tokens,
        "output_tokens": usage.output_tokens,
    });
    if let Some(obj) = out.as_object_mut() {
        if usage.cache_write_tokens > 0 {
            obj.insert(
                "cache_creation_input_tokens".into(),
                json!(usage.cache_write_tokens),
            );
        }
        if usage.cached_input_tokens > 0 {
            obj.insert(
                "cache_read_input_tokens".into(),
                json!(usage.cached_input_tokens),
            );
        }
    }
    out
}

/// 把 `{type:"error", error:{type, message}}` 翻译成 [`GatewayError`]。
fn decode_error_body(obj: &Map<String, Value>) -> GatewayError {
    let err = obj.get("error");
    let message = err
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("upstream returned an error without a message");
    let kind = err
        .and_then(|e| e.get("type"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    error_from_anthropic_type(kind, message)
}

/// Anthropic `error.type` → [`GatewayError`]。
fn error_from_anthropic_type(kind: &str, message: &str) -> GatewayError {
    use refract_core::ErrorKind;
    let kind = match kind {
        "invalid_request_error" => ErrorKind::InvalidRequest,
        "authentication_error" => ErrorKind::Unauthenticated,
        "permission_error" => ErrorKind::PermissionDenied,
        "not_found_error" => ErrorKind::NotFound,
        "request_too_large" => ErrorKind::PayloadTooLarge,
        "rate_limit_error" => ErrorKind::RateLimited,
        // overloaded 是上游过载，换个渠道有救，归到 UpstreamError（可重试）。
        "api_error" | "overloaded_error" => ErrorKind::UpstreamError,
        "timeout_error" => ErrorKind::Timeout,
        _ => ErrorKind::UpstreamError,
    };
    GatewayError::new(kind, message).with_protocol(Protocol::Messages)
}

// ---------------------------------------------------------------------------
// 扩展字段
// ---------------------------------------------------------------------------

/// 把未识别的顶层字段收进 extensions，键名带 `"messages."` 前缀。
fn collect_unknown(obj: &Map<String, Value>, known: &[&str], ext: &mut Extensions) {
    for (key, value) in obj {
        if known.contains(&key.as_str()) {
            continue;
        }
        ext.insert(format!("{EXT}{key}"), value.clone());
    }
}

/// 把 extensions 中本协议的字段还原回输出。
///
/// `skip` 里的键是已经被显式消费过的（如 `metadata`、`server_tools`），
/// 再写一遍会覆盖掉正确的合并结果。
fn restore_extensions(ext: &Extensions, out: &mut Map<String, Value>, skip: &[&str]) {
    for (key, value) in ext {
        let Some(field) = key.strip_prefix(EXT) else {
            continue;
        };
        if skip.contains(&field) {
            continue;
        }
        // 不覆盖本 codec 已经算好的字段。
        out.entry(field.to_string())
            .or_insert_with(|| value.clone());
    }
}

// ---------------------------------------------------------------------------
// 流式
// ---------------------------------------------------------------------------

impl StreamCodec for MessagesCodec {
    fn stream_decoder(&self) -> Box<dyn StreamDecoder> {
        Box::new(MessagesStreamDecoder::default())
    }

    fn stream_encoder(&self) -> Box<dyn StreamEncoder> {
        Box::new(MessagesStreamEncoder::default())
    }
}

/// 流式解码器。
///
/// 按 `frame.event` 分发。刻意宽容：中转站经常省略 `content_block_start`
/// 直接发 delta，此时按 delta 的类型推断块种类并补一个 [`StreamEvent::ContentStart`]，
/// 这样下游（聚合器与其他协议的编码器）拿到的事件流始终是完整的。
#[derive(Default)]
struct MessagesStreamDecoder {
    /// 已经产出过 ContentStart 的块下标。
    started: Vec<u32>,
    /// 是否已经产出过 Done。
    done: bool,
}

impl MessagesStreamDecoder {
    /// 补齐缺失的 ContentStart。返回需要前置产出的事件。
    fn ensure_started(&mut self, index: u32, kind: PartKind) -> Option<StreamEvent> {
        if self.started.contains(&index) {
            return None;
        }
        self.started.push(index);
        Some(StreamEvent::ContentStart { index, kind })
    }
}

impl StreamDecoder for MessagesStreamDecoder {
    fn decode(&mut self, frame: &SseFrame) -> Result<Vec<StreamEvent>, GatewayError> {
        let data = frame.data.trim();
        if data.is_empty() {
            return Ok(Vec::new());
        }
        // Anthropic 不会发 `[DONE]`，但经过某些中转站会被加上。
        if data == "[DONE]" {
            return Ok(if self.done {
                Vec::new()
            } else {
                self.done = true;
                vec![StreamEvent::Done]
            });
        }

        // 非 JSON 帧忽略而不是终止流：中转站会插入裸文本心跳，为一个心跳
        // 丢掉整个回答是不可接受的失败模式。
        let Ok(value) = serde_json::from_str::<Value>(data) else {
            tracing::debug!(bytes = data.len(), "messages: ignoring non-JSON SSE frame");
            return Ok(Vec::new());
        };

        // 事件名缺失时（中转站爱省）退回到载荷里的 `type` 字段。
        let event = frame
            .event
            .as_deref()
            .or_else(|| value.get("type").and_then(Value::as_str))
            .unwrap_or_default();

        let mut out = Vec::new();
        match event {
            "message_start" => {
                let msg = value.get("message");
                out.push(StreamEvent::Start {
                    id: msg
                        .and_then(|m| m.get("id"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    model: msg
                        .and_then(|m| m.get("model"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    usage: msg.and_then(|m| m.get("usage")).map(decode_usage),
                });
            }
            "content_block_start" => {
                let index = block_index(&value);
                let block = value.get("content_block");
                let kind = match block.and_then(|b| b.get("type")).and_then(Value::as_str) {
                    Some("thinking") | Some("redacted_thinking") => PartKind::Thinking,
                    Some("tool_use") | Some("server_tool_use") => PartKind::ToolUse,
                    _ => PartKind::Text,
                };
                if !self.started.contains(&index) {
                    self.started.push(index);
                }
                out.push(StreamEvent::ContentStart { index, kind });

                if kind == PartKind::ToolUse {
                    out.push(StreamEvent::ToolCallStart {
                        index,
                        id: block
                            .and_then(|b| b.get("id"))
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        name: block
                            .and_then(|b| b.get("name"))
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        signature: None,
                    });
                }
                // content_block_start 可以带非空初始内容（少见但合法）。
                if let Some(text) = block
                    .and_then(|b| b.get("text"))
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    out.push(StreamEvent::TextDelta {
                        index,
                        text: text.to_owned(),
                    });
                }
                if let Some(text) = block
                    .and_then(|b| b.get("thinking"))
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    out.push(StreamEvent::ThinkingDelta {
                        index,
                        text: text.to_owned(),
                    });
                }
            }
            "content_block_delta" => {
                let index = block_index(&value);
                let delta = value.get("delta");
                let dtype = delta
                    .and_then(|d| d.get("type"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                match dtype {
                    "text_delta" => {
                        let text = delta
                            .and_then(|d| d.get("text"))
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        out.extend(self.ensure_started(index, PartKind::Text));
                        out.push(StreamEvent::TextDelta {
                            index,
                            text: text.to_owned(),
                        });
                    }
                    "thinking_delta" => {
                        let text = delta
                            .and_then(|d| d.get("thinking"))
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        out.extend(self.ensure_started(index, PartKind::Thinking));
                        out.push(StreamEvent::ThinkingDelta {
                            index,
                            text: text.to_owned(),
                        });
                    }
                    // 关键：signature 走独立事件，丢了会导致多轮工具调用被上游拒。
                    "signature_delta" => {
                        let sig = delta
                            .and_then(|d| d.get("signature"))
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        out.extend(self.ensure_started(index, PartKind::Thinking));
                        out.push(StreamEvent::ThinkingSignature {
                            index,
                            signature: sig.to_owned(),
                        });
                    }
                    "input_json_delta" => {
                        let frag = delta
                            .and_then(|d| d.get("partial_json"))
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        out.extend(self.ensure_started(index, PartKind::ToolUse));
                        out.push(StreamEvent::ToolCallArgsDelta {
                            index,
                            fragment: frag.to_owned(),
                        });
                    }
                    other => {
                        tracing::debug!(delta_type = other, "unknown Anthropic delta type");
                    }
                }
            }
            "content_block_stop" => {
                out.push(StreamEvent::ContentStop {
                    index: block_index(&value),
                });
            }
            "message_delta" => {
                if let Some(usage) = value.get("usage") {
                    out.push(StreamEvent::Usage(decode_usage(usage)));
                }
                let delta = value.get("delta");
                if let Some(reason) = delta
                    .and_then(|d| d.get("stop_reason"))
                    .and_then(Value::as_str)
                {
                    out.push(StreamEvent::Stop {
                        reason: decode_stop_reason(reason),
                        stop_sequence: delta
                            .and_then(|d| d.get("stop_sequence"))
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                    });
                }
            }
            "message_stop" => {
                self.done = true;
                out.push(StreamEvent::Done);
            }
            "ping" => out.push(StreamEvent::Ping),
            "error" => {
                let err = value.get("error");
                out.push(StreamEvent::Error {
                    message: err
                        .and_then(|e| e.get("message"))
                        .and_then(Value::as_str)
                        .unwrap_or("upstream stream error")
                        .to_owned(),
                    kind: err
                        .and_then(|e| e.get("type"))
                        .and_then(Value::as_str)
                        .unwrap_or("api_error")
                        .to_owned(),
                });
            }
            other => {
                tracing::debug!(event = other, "unknown Anthropic stream event; ignored");
            }
        }
        Ok(out)
    }

    fn finish(&mut self) -> Result<Vec<StreamEvent>, GatewayError> {
        // 上游断流时没发 message_stop，补一个 Done，否则下游会一直等。
        if self.done {
            return Ok(Vec::new());
        }
        self.done = true;
        Ok(vec![StreamEvent::Done])
    }
}

fn block_index(value: &Value) -> u32 {
    value
        .get("index")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(u64::from(u32::MAX)) as u32
}

/// 流式编码器。
///
/// 负责把任意来源的事件流整形成 Anthropic 要求的严格序列：
///
/// ```text
/// message_start → (content_block_start → delta* → content_block_stop)*
///               → message_delta → message_stop
/// ```
///
/// 上游若是 OpenAI Chat，只会发裸 delta（没有 Start、没有 ContentStart、
/// 没有 ContentStop），这些仪式性事件全部由本编码器补齐。另外 IR 的块下标可能
/// 稀疏（比如 Responses 的 output_index 会跳号），而 Anthropic 要求块下标从 0
/// 连续递增，所以这里做一次重编号。
#[derive(Default)]
struct MessagesStreamEncoder {
    /// 是否已发出 message_start。
    started: bool,
    /// 已发出的响应 ID / 模型名，补 message_start 时要用。
    id: String,
    model: String,
    /// 首帧 usage（input_tokens）。
    input_usage: Usage,
    /// IR 块下标 → Anthropic 块下标的映射，按首次出现顺序重编号。
    remap: Vec<(u32, u32)>,
    /// 当前打开着的块（Anthropic 下标 + 种类）。
    open: Option<(u32, PartKind)>,
    /// 下一个可用的 Anthropic 块下标。
    next_index: u32,
    /// 累积的输出 usage，message_delta 要带。
    usage: Usage,
    /// 停止原因，等到收尾时才写进 message_delta。
    stop_reason: Option<StopReason>,
    stop_sequence: Option<String>,
    /// 是否已发出 message_stop。
    finished: bool,
}

impl MessagesStreamEncoder {
    /// 补出 message_start。上游没发 Start 时也必须有，否则客户端 SDK 会直接报错。
    fn ensure_started(&mut self, frames: &mut Vec<SseFrame>) {
        if self.started {
            return;
        }
        self.started = true;
        if self.id.is_empty() {
            self.id = format!("msg_{}", uuid::Uuid::new_v4().simple());
        }
        let payload = json!({
            "type": "message_start",
            "message": {
                "id": self.id,
                "type": "message",
                "role": "assistant",
                "model": self.model,
                "content": [],
                "stop_reason": Value::Null,
                "stop_sequence": Value::Null,
                "usage": encode_usage(&self.input_usage),
            },
        });
        frames.push(SseFrame::named("message_start", payload.to_string()));
    }

    /// 取（或分配）某个 IR 下标对应的 Anthropic 下标。
    fn anth_index(&mut self, ir_index: u32) -> u32 {
        if let Some((_, mapped)) = self.remap.iter().find(|(ir, _)| *ir == ir_index) {
            return *mapped;
        }
        let mapped = self.next_index;
        self.next_index += 1;
        self.remap.push((ir_index, mapped));
        mapped
    }

    /// 关闭当前打开的块。
    fn close_open(&mut self, frames: &mut Vec<SseFrame>) {
        if let Some((index, _)) = self.open.take() {
            frames.push(SseFrame::named(
                "content_block_stop",
                json!({ "type": "content_block_stop", "index": index }).to_string(),
            ));
        }
    }

    /// 保证指定种类的块处于打开状态；种类不符或下标不同就先关掉旧块。
    fn open_block(&mut self, ir_index: u32, kind: PartKind, frames: &mut Vec<SseFrame>) -> u32 {
        self.ensure_started(frames);
        let index = self.anth_index(ir_index);
        match self.open {
            Some((open_idx, open_kind)) if open_idx == index && open_kind == kind => return index,
            Some(_) => self.close_open(frames),
            None => {}
        }
        let content_block = match kind {
            PartKind::Text => json!({ "type": "text", "text": "" }),
            PartKind::Thinking => json!({ "type": "thinking", "thinking": "", "signature": "" }),
            // Refusal 在 Anthropic 侧没有对应块，退化成文本块。
            PartKind::Refusal => json!({ "type": "text", "text": "" }),
            // 工具块的 id/name 由 ToolCallStart 提供，走 open_tool_block。
            PartKind::ToolUse => json!({ "type": "tool_use", "id": "", "name": "", "input": {} }),
        };
        frames.push(SseFrame::named(
            "content_block_start",
            json!({
                "type": "content_block_start",
                "index": index,
                "content_block": content_block,
            })
            .to_string(),
        ));
        self.open = Some((index, kind));
        index
    }

    /// 打开一个带 id/name 的工具块。
    fn open_tool_block(
        &mut self,
        ir_index: u32,
        id: &str,
        name: &str,
        frames: &mut Vec<SseFrame>,
    ) -> u32 {
        self.ensure_started(frames);
        let index = self.anth_index(ir_index);
        // 同一个下标上若已开着块（可能是 ContentStart 先到），先关掉再用带
        // id/name 的正式块替换 —— 否则客户端拿不到工具名，整段调用都没法用。
        if self.open.is_some() {
            self.close_open(frames);
        }
        let id = if id.is_empty() {
            format!("toolu_{}", uuid::Uuid::new_v4().simple())
        } else {
            id.to_owned()
        };
        frames.push(SseFrame::named(
            "content_block_start",
            json!({
                "type": "content_block_start",
                "index": index,
                "content_block": { "type": "tool_use", "id": id, "name": name, "input": {} },
            })
            .to_string(),
        ));
        self.open = Some((index, PartKind::ToolUse));
        index
    }

    /// 收尾：关块 → message_delta → message_stop。
    fn wrap_up(&mut self, frames: &mut Vec<SseFrame>) {
        if self.finished {
            return;
        }
        self.finished = true;
        // 即使一个内容事件都没来过，也要给出完整骨架。
        self.ensure_started(frames);
        self.close_open(frames);

        let reason = self.stop_reason.unwrap_or(StopReason::Stop);
        frames.push(SseFrame::named(
            "message_delta",
            json!({
                "type": "message_delta",
                "delta": {
                    "stop_reason": encode_stop_reason(reason),
                    "stop_sequence": match &self.stop_sequence {
                        Some(s) => json!(s),
                        None => Value::Null,
                    },
                },
                // 官方 message_delta.usage 是全量累积（含 input_tokens）；
                // 只写 output 的话，上游没发 message_start usage 的场景
                // （如 chat 转来）客户端就永远看不到 input。
                "usage": encode_usage(&self.usage),
            })
            .to_string(),
        ));
        frames.push(SseFrame::named(
            "message_stop",
            json!({ "type": "message_stop" }).to_string(),
        ));
    }
}

impl StreamEncoder for MessagesStreamEncoder {
    fn encode(&mut self, event: &StreamEvent) -> Result<Vec<SseFrame>, GatewayError> {
        let mut frames = Vec::new();
        // message_stop 之后不再产出任何东西，避免客户端状态机错乱。
        if self.finished {
            return Ok(frames);
        }

        match event {
            StreamEvent::Start { id, model, usage } => {
                if !self.started {
                    self.id = id.clone();
                    self.model = model.clone();
                    if let Some(u) = usage {
                        self.input_usage = *u;
                        self.usage = *u;
                    }
                }
                self.ensure_started(&mut frames);
            }
            StreamEvent::ContentStart { index, kind } => {
                // 工具块要等 ToolCallStart 才有 id/name，这里不急着开。
                if *kind != PartKind::ToolUse {
                    self.open_block(*index, *kind, &mut frames);
                }
            }
            StreamEvent::TextDelta { index, text } => {
                let idx = self.open_block(*index, PartKind::Text, &mut frames);
                frames.push(SseFrame::named(
                    "content_block_delta",
                    json!({
                        "type": "content_block_delta",
                        "index": idx,
                        "delta": { "type": "text_delta", "text": text },
                    })
                    .to_string(),
                ));
            }
            // Anthropic 没有 refusal 块，当文本发出去 —— 拒答理由要让用户看到。
            StreamEvent::RefusalDelta { index, text } => {
                let idx = self.open_block(*index, PartKind::Refusal, &mut frames);
                frames.push(SseFrame::named(
                    "content_block_delta",
                    json!({
                        "type": "content_block_delta",
                        "index": idx,
                        "delta": { "type": "text_delta", "text": text },
                    })
                    .to_string(),
                ));
            }
            StreamEvent::ThinkingDelta { index, text } => {
                let idx = self.open_block(*index, PartKind::Thinking, &mut frames);
                frames.push(SseFrame::named(
                    "content_block_delta",
                    json!({
                        "type": "content_block_delta",
                        "index": idx,
                        "delta": { "type": "thinking_delta", "thinking": text },
                    })
                    .to_string(),
                ));
            }
            StreamEvent::ThinkingSignature { index, signature } => {
                let idx = self.open_block(*index, PartKind::Thinking, &mut frames);
                frames.push(SseFrame::named(
                    "content_block_delta",
                    json!({
                        "type": "content_block_delta",
                        "index": idx,
                        "delta": { "type": "signature_delta", "signature": signature },
                    })
                    .to_string(),
                ));
            }
            StreamEvent::ToolCallStart {
                index, id, name, ..
            } => {
                self.open_tool_block(*index, id, name, &mut frames);
            }
            StreamEvent::ToolCallArgsDelta { index, fragment } => {
                // 上游可能没发过 ToolCallStart（Chat 的后续帧只有 arguments），
                // 此时补一个空 name 的块总比丢掉入参好。
                // `anth_index` 要 &mut self，不能放进 match guard 里借用 self.open。
                let mapped = self.anth_index(*index);
                let idx = match self.open {
                    Some((open, PartKind::ToolUse)) if open == mapped => mapped,
                    _ => self.open_tool_block(*index, "", "", &mut frames),
                };
                frames.push(SseFrame::named(
                    "content_block_delta",
                    json!({
                        "type": "content_block_delta",
                        "index": idx,
                        "delta": { "type": "input_json_delta", "partial_json": fragment },
                    })
                    .to_string(),
                ));
            }
            StreamEvent::ContentStop { index } => {
                let idx = self.anth_index(*index);
                if matches!(self.open, Some((open, _)) if open == idx) {
                    self.close_open(&mut frames);
                }
            }
            StreamEvent::Usage(u) => {
                // 取最大值：Anthropic 的 message_delta 报的是累积量，
                // 而 OpenAI 只在末帧报一次，两种语义下取 max 都对。
                self.usage.input_tokens = self.usage.input_tokens.max(u.input_tokens);
                self.usage.output_tokens = self.usage.output_tokens.max(u.output_tokens);
                self.usage.cached_input_tokens =
                    self.usage.cached_input_tokens.max(u.cached_input_tokens);
                self.usage.cache_write_tokens =
                    self.usage.cache_write_tokens.max(u.cache_write_tokens);
            }
            StreamEvent::Stop {
                reason,
                stop_sequence,
            } => {
                self.stop_reason = Some(*reason);
                if stop_sequence.is_some() {
                    self.stop_sequence = stop_sequence.clone();
                }
            }
            StreamEvent::Done => self.wrap_up(&mut frames),
            StreamEvent::Ping => {
                frames.push(SseFrame::named(
                    "ping",
                    json!({ "type": "ping" }).to_string(),
                ));
            }
            StreamEvent::Error { message, kind } => {
                // 错误帧要能被客户端 SDK 识别，所以照 Anthropic 的错误体发。
                frames.push(SseFrame::named(
                    "error",
                    json!({
                        "type": "error",
                        "error": { "type": kind, "message": message },
                    })
                    .to_string(),
                ));
                self.finished = true;
            }
        }
        Ok(frames)
    }

    fn finish(&mut self) -> Result<Vec<SseFrame>, GatewayError> {
        let mut frames = Vec::new();
        self.wrap_up(&mut frames);
        Ok(frames)
    }
}

impl ProtocolCodec for MessagesCodec {
    fn protocol(&self) -> Protocol {
        Protocol::Messages
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    /// 从帧里取出事件名序列，验证仪式性事件是否齐备。
    fn event_names(frames: &[SseFrame]) -> Vec<String> {
        frames
            .iter()
            .map(|f| f.event.clone().unwrap_or_default())
            .collect()
    }

    fn parse(frame: &SseFrame) -> Value {
        serde_json::from_str(&frame.data).expect("frame data must be valid JSON")
    }

    /// 跑一遍编码器，返回所有帧（含 finish 补的）。
    fn encode_all(events: &[StreamEvent]) -> Vec<SseFrame> {
        let mut enc = MessagesStreamEncoder::default();
        let mut frames = Vec::new();
        for ev in events {
            frames.extend(enc.encode(ev).expect("encode"));
        }
        frames.extend(enc.finish().expect("finish"));
        frames
    }

    #[test]
    fn minimal_request_round_trips() {
        let raw = json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 1024,
            "messages": [{ "role": "user", "content": "hi" }],
        });
        let ir = MESSAGES.decode_request(&raw).expect("decode");
        assert_eq!(ir.model, "claude-sonnet-4-5");
        assert_eq!(ir.max_output_tokens, Some(1_024));
        assert!(!ir.stream);
        assert_eq!(ir.messages, vec![Message::text(Role::User, "hi")]);

        let back = MESSAGES.encode_request(&ir).expect("encode");
        assert_eq!(back, raw);
    }

    #[test]
    fn missing_max_tokens_is_rejected() {
        let raw = json!({
            "model": "claude-sonnet-4-5",
            "messages": [{ "role": "user", "content": "hi" }],
        });
        let err = MESSAGES.decode_request(&raw).expect_err("must fail");
        assert_eq!(err.kind, refract_core::ErrorKind::InvalidRequest);
        assert!(
            err.message.contains("max_tokens"),
            "message should name the missing field, got: {}",
            err.message
        );

        // model 缺失同理。
        let no_model = json!({ "max_tokens": 8, "messages": [] });
        let err = MESSAGES.decode_request(&no_model).expect_err("must fail");
        assert!(err.message.contains("model"), "got: {}", err.message);
    }

    #[test]
    fn max_tokens_defaults_when_ir_has_none() {
        let ir = UnifiedRequest::new("claude", vec![Message::text(Role::User, "hi")]);
        let out = MESSAGES.encode_request(&ir).expect("encode");
        assert_eq!(out["max_tokens"], json!(DEFAULT_MAX_TOKENS));
    }

    #[test]
    fn system_accepts_string_and_block_array() {
        let as_string = json!({
            "model": "m", "max_tokens": 8,
            "system": "be terse",
            "messages": [],
        });
        let ir = MESSAGES.decode_request(&as_string).expect("decode");
        assert_eq!(ir.system, vec![ContentPart::text("be terse")]);

        let as_blocks = json!({
            "model": "m", "max_tokens": 8,
            "system": [
                { "type": "text", "text": "be terse" },
                { "type": "text", "text": "be correct" },
            ],
            "messages": [],
        });
        let ir = MESSAGES.decode_request(&as_blocks).expect("decode");
        assert_eq!(ir.system_text(), "be terse\nbe correct");
        // 纯文本 system 编码回去要压成字符串（中转站兼容性）。
        let out = MESSAGES.encode_request(&ir).expect("encode");
        assert_eq!(out["system"], json!("be terse\nbe correct"));
    }

    #[test]
    fn multimodal_image_sources_round_trip() {
        let raw = json!({
            "model": "m", "max_tokens": 8,
            "messages": [{
                "role": "user",
                "content": [
                    { "type": "text", "text": "what is this" },
                    { "type": "image", "source": {
                        "type": "base64", "media_type": "image/png", "data": "iVBORw0KGgo="
                    }},
                    { "type": "image", "source": { "type": "url", "url": "https://x/y.jpg" }},
                    { "type": "document", "source": {
                        "type": "base64", "media_type": "application/pdf", "data": "JVBER"
                    }, "title": "spec.pdf" },
                ],
            }],
        });
        let ir = MESSAGES.decode_request(&raw).expect("decode");
        assert_eq!(
            ir.messages[0].content,
            vec![
                ContentPart::text("what is this"),
                ContentPart::Image {
                    source: MediaSource::Base64("iVBORw0KGgo=".into()),
                    mime: Some("image/png".into()),
                    detail: None,
                },
                ContentPart::Image {
                    source: MediaSource::Url("https://x/y.jpg".into()),
                    mime: None,
                    detail: None,
                },
                ContentPart::File {
                    source: MediaSource::Base64("JVBER".into()),
                    mime: Some("application/pdf".into()),
                    name: Some("spec.pdf".into()),
                },
            ]
        );
        let back = MESSAGES.encode_request(&ir).expect("encode");
        assert_eq!(back["messages"], raw["messages"]);
    }

    #[test]
    fn data_uri_image_from_other_protocol_becomes_base64_source() {
        // OpenAI 那边的图片是 data URI 形式的 URL，转到 Anthropic 必须拆成
        // base64 源，否则上游会拒收。
        let mut ir = UnifiedRequest::new("m", vec![]);
        ir.max_output_tokens = Some(8);
        ir.messages.push(Message::new(
            Role::User,
            vec![ContentPart::Image {
                source: MediaSource::Url("data:image/webp;base64,UklGR".into()),
                mime: None,
                detail: Some("high".into()),
            }],
        ));
        let out = MESSAGES.encode_request(&ir).expect("encode");
        assert_eq!(
            out["messages"][0]["content"][0]["source"],
            json!({ "type": "base64", "media_type": "image/webp", "data": "UklGR" })
        );
    }

    #[test]
    fn tool_lifecycle_round_trips() {
        // 三段式：声明 → 调用 → 回传。
        let raw = json!({
            "model": "m", "max_tokens": 64,
            "tools": [{
                "name": "get_weather",
                "description": "look up weather",
                "input_schema": {
                    "type": "object",
                    "properties": { "city": { "type": "string" } },
                    "required": ["city"],
                },
            }],
            "tool_choice": { "type": "any" },
            "messages": [
                { "role": "user", "content": "weather in Kyoto?" },
                { "role": "assistant", "content": [
                    { "type": "text", "text": "checking" },
                    { "type": "tool_use", "id": "toolu_1", "name": "get_weather",
                      "input": { "city": "Kyoto" } },
                ]},
                { "role": "user", "content": [
                    { "type": "tool_result", "tool_use_id": "toolu_1", "content": "22C sunny" },
                ]},
            ],
        });
        let ir = MESSAGES.decode_request(&raw).expect("decode");

        // input_schema 落到 ToolDef.parameters。
        assert_eq!(ir.tools.len(), 1);
        assert_eq!(ir.tools[0].name, "get_weather");
        assert_eq!(ir.tools[0].parameters, raw["tools"][0]["input_schema"]);
        // `any` → Required。
        assert_eq!(ir.tool_choice, ToolChoice::Required);

        assert_eq!(
            ir.messages[1].content[1],
            ContentPart::ToolUse {
                signature: None,
                id: "toolu_1".into(),
                name: "get_weather".into(),
                input: json!({ "city": "Kyoto" }),
            }
        );
        // 纯 tool_result 的 user 消息归为 Role::Tool，便于转到 OpenAI Chat。
        assert_eq!(ir.messages[2].role, Role::Tool);
        assert_eq!(
            ir.messages[2].content[0],
            ContentPart::ToolResult {
                name: None,
                id: "toolu_1".into(),
                content: vec![ContentPart::text("22C sunny")],
                is_error: false,
            }
        );

        let back = MESSAGES.encode_request(&ir).expect("encode");
        assert_eq!(back["tools"], raw["tools"]);
        assert_eq!(back["tool_choice"], json!({ "type": "any" }));
        // Role::Tool 回来时必须还是 role:"user"。
        assert_eq!(back["messages"][2]["role"], json!("user"));
        assert_eq!(back["messages"], raw["messages"]);
    }

    #[test]
    fn tool_result_can_carry_images_and_error_flag() {
        let raw = json!({
            "model": "m", "max_tokens": 8,
            "messages": [{ "role": "user", "content": [{
                "type": "tool_result",
                "tool_use_id": "toolu_9",
                "is_error": true,
                "content": [
                    { "type": "text", "text": "screenshot follows" },
                    { "type": "image", "source": {
                        "type": "base64", "media_type": "image/png", "data": "AAA"
                    }},
                ],
            }]}],
        });
        let ir = MESSAGES.decode_request(&raw).expect("decode");
        let ContentPart::ToolResult {
            content, is_error, ..
        } = &ir.messages[0].content[0]
        else {
            panic!(
                "expected a tool_result part, got {:?}",
                ir.messages[0].content[0]
            );
        };
        assert!(*is_error);
        assert_eq!(content.len(), 2);
        // 含图片时不能被压成字符串。
        let back = MESSAGES.encode_request(&ir).expect("encode");
        assert_eq!(back["messages"], raw["messages"]);
    }

    #[test]
    fn tool_choice_carries_parallel_flag_both_ways() {
        let raw = json!({
            "model": "m", "max_tokens": 8, "messages": [],
            "tool_choice": { "type": "tool", "name": "pick", "disable_parallel_tool_use": true },
        });
        let ir = MESSAGES.decode_request(&raw).expect("decode");
        assert_eq!(ir.tool_choice, ToolChoice::Tool("pick".into()));
        // Anthropic 说「禁用并行」，IR 说「允许并行」，语义取反。
        assert_eq!(ir.parallel_tool_calls, Some(false));

        let back = MESSAGES.encode_request(&ir).expect("encode");
        assert_eq!(back["tool_choice"], raw["tool_choice"]);

        // 允许并行时不该写出 disable_parallel_tool_use。
        let mut allowed = ir.clone();
        allowed.parallel_tool_calls = Some(true);
        let out = MESSAGES.encode_request(&allowed).expect("encode");
        assert_eq!(
            out["tool_choice"],
            json!({ "type": "tool", "name": "pick" })
        );
    }

    #[test]
    fn thinking_signature_survives_request_round_trip() {
        // 这是最关键的不变量：signature 丢了，Anthropic 会拒掉整个多轮请求。
        // max_tokens 必须大于 budget_tokens，否则 clamp 逻辑会禁用 thinking。
        let raw = json!({
            "model": "m", "max_tokens": 4096,
            "thinking": { "type": "enabled", "budget_tokens": 2048 },
            "messages": [{ "role": "assistant", "content": [
                { "type": "thinking", "thinking": "let me think", "signature": "SIG-ABC" },
                { "type": "redacted_thinking", "data": "OPAQUE" },
                { "type": "text", "text": "done" },
            ]}],
        });
        let ir = MESSAGES.decode_request(&raw).expect("decode");
        assert_eq!(
            ir.messages[0].content[0],
            ContentPart::Thinking {
                text: "let me think".into(),
                signature: Some("SIG-ABC".into()),
            }
        );
        assert_eq!(
            ir.messages[0].content[1],
            ContentPart::RedactedThinking {
                data: "OPAQUE".into()
            }
        );
        assert_eq!(
            ir.reasoning,
            Some(ReasoningConfig {
                effort: None,
                budget_tokens: Some(2_048),
                include_thoughts: Some(true),
            })
        );

        let back = MESSAGES.encode_request(&ir).expect("encode");
        // Anthropic 要求首条消息是 user：assistant 开头时插占位，原消息后移。
        assert_eq!(back["messages"][0]["role"], json!("user"));
        assert_eq!(back["messages"][1], raw["messages"][0]);
        assert_eq!(back["thinking"], raw["thinking"]);
    }

    #[test]
    fn effort_only_reasoning_is_converted_to_budget() {
        // 从 OpenAI 转过来只有 effort，不折算成 budget 的话思考会被静默关掉。
        let mut ir = UnifiedRequest::new("m", vec![]);
        ir.max_output_tokens = Some(10_000);
        ir.reasoning = Some(ReasoningConfig {
            effort: Some("high".into()),
            ..Default::default()
        });
        let out = MESSAGES.encode_request(&ir).expect("encode");
        assert_eq!(
            out["thinking"],
            json!({ "type": "enabled", "budget_tokens": 8_000 })
        );

        // 显式关闭时输出 disabled。
        let disabled = json!({
            "model": "m", "max_tokens": 8, "messages": [],
            "thinking": { "type": "disabled" },
        });
        let ir = MESSAGES.decode_request(&disabled).expect("decode");
        let out = MESSAGES.encode_request(&ir).expect("encode");
        assert_eq!(out["thinking"], json!({ "type": "disabled" }));
    }

    #[test]
    fn consecutive_same_role_messages_are_merged() {
        // Anthropic 拒绝连续两条同角色消息。OpenAI 一次并行工具调用会产生
        // 多条 role:"tool" 消息，直接映射就会撞上这个限制。
        let mut ir = UnifiedRequest::new("m", vec![]);
        ir.max_output_tokens = Some(8);
        ir.messages = vec![
            Message::text(Role::User, "a"),
            Message::text(Role::User, "b"),
            Message::text(Role::Assistant, "c"),
            Message::new(
                Role::Tool,
                vec![ContentPart::ToolResult {
                    name: None,
                    id: "t1".into(),
                    content: vec![ContentPart::text("r1")],
                    is_error: false,
                }],
            ),
            Message::new(
                Role::Tool,
                vec![ContentPart::ToolResult {
                    name: None,
                    id: "t2".into(),
                    content: vec![ContentPart::text("r2")],
                    is_error: false,
                }],
            ),
        ];
        let out = MESSAGES.encode_request(&ir).expect("encode");
        let messages = out["messages"].as_array().expect("array");

        // 5 条 IR 消息 → 3 条 Anthropic 消息：user(a+b), assistant(c), user(两个 tool_result)。
        assert_eq!(messages.len(), 3);
        assert_eq!(
            messages[0],
            json!({ "role": "user", "content": [
                { "type": "text", "text": "a" },
                { "type": "text", "text": "b" },
            ]})
        );
        assert_eq!(messages[1]["role"], json!("assistant"));
        assert_eq!(
            messages[2],
            json!({ "role": "user", "content": [
                { "type": "tool_result", "tool_use_id": "t1", "content": "r1" },
                { "type": "tool_result", "tool_use_id": "t2", "content": "r2" },
            ]})
        );
        // 合并后不能出现相邻同角色。
        let roles: Vec<&str> = messages
            .iter()
            .map(|m| m["role"].as_str().unwrap())
            .collect();
        assert!(
            roles.windows(2).all(|w| w[0] != w[1]),
            "found adjacent same-role messages: {roles:?}"
        );
    }

    #[test]
    fn empty_content_messages_are_dropped_when_encoding() {
        let mut ir = UnifiedRequest::new("m", vec![]);
        ir.max_output_tokens = Some(8);
        ir.messages = vec![
            Message::text(Role::User, "a"),
            // 空 content 的 assistant 消息在 Anthropic 侧非法，且它若被保留会
            // 把前后两条 user 隔开，破坏合并。
            Message::new(Role::Assistant, vec![]),
            Message::text(Role::User, "b"),
        ];
        let out = MESSAGES.encode_request(&ir).expect("encode");
        let messages = out["messages"].as_array().expect("array");
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0]["content"],
            json!([{ "type": "text", "text": "a" }, { "type": "text", "text": "b" }])
        );
    }

    #[test]
    fn sampling_and_user_metadata_round_trip() {
        let raw = json!({
            "model": "m", "max_tokens": 8, "messages": [],
            "temperature": 0.5,
            "top_p": 0.9,
            "top_k": 40,
            "stop_sequences": ["\n\nHuman:", "END"],
            "metadata": { "user_id": "u-42" },
        });
        let ir = MESSAGES.decode_request(&raw).expect("decode");
        assert_eq!(ir.sampling.temperature, Some(0.5));
        assert_eq!(ir.sampling.top_p, Some(0.9));
        assert_eq!(ir.sampling.top_k, Some(40));
        assert_eq!(ir.sampling.stop, vec!["\n\nHuman:", "END"]);
        assert_eq!(ir.user.as_deref(), Some("u-42"));

        let back = MESSAGES.encode_request(&ir).expect("encode");
        assert_eq!(back, raw);
    }

    #[test]
    fn unknown_request_fields_land_in_extensions_and_come_back() {
        let raw = json!({
            "model": "m", "max_tokens": 8, "messages": [],
            "service_tier": "priority",
            "mcp_servers": [{ "name": "fs" }],
        });
        let ir = MESSAGES.decode_request(&raw).expect("decode");
        assert_eq!(
            ir.extension("messages.service_tier"),
            Some(&json!("priority"))
        );
        assert_eq!(
            ir.extension("messages.mcp_servers"),
            Some(&json!([{ "name": "fs" }]))
        );
        // 未知字段不该冒充已知字段。
        assert!(ir.extension("messages.model").is_none());

        let back = MESSAGES.encode_request(&ir).expect("encode");
        assert_eq!(back["service_tier"], json!("priority"));
        assert_eq!(back["mcp_servers"], json!([{ "name": "fs" }]));
    }

    #[test]
    fn server_tools_without_input_schema_are_preserved() {
        // 服务端工具没有 input_schema，无法表达成 ToolDef，但不能丢。
        let raw = json!({
            "model": "m", "max_tokens": 8, "messages": [],
            "tools": [
                { "type": "web_search_20250305", "name": "web_search" },
                { "name": "local", "input_schema": { "type": "object" } },
            ],
        });
        let ir = MESSAGES.decode_request(&raw).expect("decode");
        assert_eq!(ir.tools.len(), 1);
        assert_eq!(ir.tools[0].name, "local");

        let back = MESSAGES.encode_request(&ir).expect("encode");
        let tools = back["tools"].as_array().expect("array");
        assert_eq!(tools.len(), 2);
        assert!(
            tools.contains(&json!({ "type": "web_search_20250305", "name": "web_search" })),
            "server tool must be restored verbatim, got {tools:?}"
        );
    }

    #[test]
    fn response_round_trips_with_usage_and_cache_fields() {
        let raw = json!({
            "id": "msg_01",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-4-5",
            "content": [
                { "type": "thinking", "thinking": "hmm", "signature": "SIG" },
                { "type": "text", "text": "hello" },
                { "type": "tool_use", "id": "toolu_1", "name": "f", "input": { "a": 1 } },
            ],
            "stop_reason": "tool_use",
            "stop_sequence": Value::Null,
            "usage": {
                "input_tokens": 10,
                "output_tokens": 20,
                "cache_creation_input_tokens": 5,
                "cache_read_input_tokens": 7,
            },
        });
        let ir = MESSAGES.decode_response(&raw).expect("decode");
        assert_eq!(ir.id, "msg_01");
        assert_eq!(ir.stop_reason, Some(StopReason::ToolUse));
        assert_eq!(
            ir.usage,
            Usage {
                input_tokens: 10,
                output_tokens: 20,
                // cache_read → cached_input，cache_creation → cache_write。
                cached_input_tokens: 7,
                cache_write_tokens: 5,
                reasoning_tokens: 0,
            }
        );
        assert_eq!(
            ir.content[0],
            ContentPart::Thinking {
                text: "hmm".into(),
                signature: Some("SIG".into()),
            }
        );

        let back = MESSAGES.encode_response(&ir).expect("encode");
        assert_eq!(back, raw);
    }

    #[test]
    fn stop_reason_mapping_covers_every_variant() {
        let pairs = [
            ("end_turn", StopReason::Stop),
            ("max_tokens", StopReason::MaxTokens),
            ("stop_sequence", StopReason::StopSequence),
            ("tool_use", StopReason::ToolUse),
            ("refusal", StopReason::Refusal),
            ("pause_turn", StopReason::PauseTurn),
        ];
        for (wire, ir) in pairs {
            assert_eq!(decode_stop_reason(wire), ir, "decoding `{wire}`");
            assert_eq!(encode_stop_reason(ir), wire, "encoding `{ir:?}`");
        }
        // 未知值退化成 Other，不能报错。
        assert_eq!(decode_stop_reason("something_new"), StopReason::Other);
        // Other/ContentFilter 在 Anthropic 侧没有专属值，映射到最接近的。
        assert_eq!(encode_stop_reason(StopReason::Other), "end_turn");
        assert_eq!(encode_stop_reason(StopReason::ContentFilter), "refusal");
    }

    #[test]
    fn error_body_maps_to_typed_gateway_error() {
        use refract_core::ErrorKind;
        let cases = [
            ("invalid_request_error", ErrorKind::InvalidRequest),
            ("authentication_error", ErrorKind::Unauthenticated),
            ("permission_error", ErrorKind::PermissionDenied),
            ("not_found_error", ErrorKind::NotFound),
            ("request_too_large", ErrorKind::PayloadTooLarge),
            ("rate_limit_error", ErrorKind::RateLimited),
            ("api_error", ErrorKind::UpstreamError),
            ("overloaded_error", ErrorKind::UpstreamError),
        ];
        for (wire, expected) in cases {
            let raw = json!({
                "type": "error",
                "error": { "type": wire, "message": "boom" },
            });
            let err = MESSAGES
                .decode_response(&raw)
                .expect_err("must be an error");
            assert_eq!(err.kind, expected, "for error.type `{wire}`");
            assert_eq!(err.message, "boom");
            assert_eq!(err.protocol, Some(Protocol::Messages));
        }
        // overloaded 应该值得换渠道重试。
        let overloaded = json!({
            "type": "error",
            "error": { "type": "overloaded_error", "message": "busy" },
        });
        let err = MESSAGES.decode_response(&overloaded).expect_err("error");
        assert!(err.is_retryable());
    }

    #[test]
    fn stream_decodes_full_anthropic_sequence() {
        let mut dec = MessagesStreamDecoder::default();
        let frames = [
            SseFrame::named(
                "message_start",
                json!({ "type": "message_start", "message": {
                    "id": "msg_1", "model": "claude-sonnet-4-5",
                    "usage": { "input_tokens": 11, "output_tokens": 0 },
                }})
                .to_string(),
            ),
            SseFrame::named(
                "content_block_start",
                json!({ "type": "content_block_start", "index": 0,
                        "content_block": { "type": "thinking", "thinking": "" }})
                .to_string(),
            ),
            SseFrame::named(
                "content_block_delta",
                json!({ "type": "content_block_delta", "index": 0,
                        "delta": { "type": "thinking_delta", "thinking": "why" }})
                .to_string(),
            ),
            SseFrame::named(
                "content_block_delta",
                json!({ "type": "content_block_delta", "index": 0,
                        "delta": { "type": "signature_delta", "signature": "SIG" }})
                .to_string(),
            ),
            SseFrame::named(
                "content_block_stop",
                json!({ "type": "content_block_stop", "index": 0 }).to_string(),
            ),
            SseFrame::named("ping", json!({ "type": "ping" }).to_string()),
            SseFrame::named(
                "content_block_start",
                json!({ "type": "content_block_start", "index": 1,
                        "content_block": { "type": "tool_use", "id": "toolu_1", "name": "f" }})
                .to_string(),
            ),
            SseFrame::named(
                "content_block_delta",
                json!({ "type": "content_block_delta", "index": 1,
                        "delta": { "type": "input_json_delta", "partial_json": "{\"a\":" }})
                .to_string(),
            ),
            SseFrame::named(
                "message_delta",
                json!({ "type": "message_delta",
                        "delta": { "stop_reason": "tool_use", "stop_sequence": Value::Null },
                        "usage": { "output_tokens": 33 }})
                .to_string(),
            ),
            SseFrame::named(
                "message_stop",
                json!({ "type": "message_stop" }).to_string(),
            ),
        ];

        let mut events = Vec::new();
        for frame in &frames {
            events.extend(dec.decode(frame).expect("decode"));
        }

        assert_eq!(
            events,
            vec![
                StreamEvent::Start {
                    id: "msg_1".into(),
                    model: "claude-sonnet-4-5".into(),
                    usage: Some(Usage {
                        input_tokens: 11,
                        ..Default::default()
                    }),
                },
                StreamEvent::ContentStart {
                    index: 0,
                    kind: PartKind::Thinking
                },
                StreamEvent::ThinkingDelta {
                    index: 0,
                    text: "why".into()
                },
                // signature_delta 必须成为独立事件，否则多轮工具调用会失败。
                StreamEvent::ThinkingSignature {
                    index: 0,
                    signature: "SIG".into()
                },
                StreamEvent::ContentStop { index: 0 },
                StreamEvent::Ping,
                StreamEvent::ContentStart {
                    index: 1,
                    kind: PartKind::ToolUse
                },
                StreamEvent::ToolCallStart {
                    signature: None,
                    index: 1,
                    id: "toolu_1".into(),
                    name: "f".into(),
                },
                StreamEvent::ToolCallArgsDelta {
                    index: 1,
                    fragment: "{\"a\":".into()
                },
                StreamEvent::Usage(Usage {
                    output_tokens: 33,
                    ..Default::default()
                }),
                StreamEvent::Stop {
                    reason: StopReason::ToolUse,
                    stop_sequence: None,
                },
                StreamEvent::Done,
            ]
        );
        // 已经收到 message_stop，finish 不该再补 Done。
        assert_eq!(dec.finish().expect("finish"), vec![]);
    }

    #[test]
    fn stream_decoder_tolerates_missing_ceremony() {
        // 中转站常见行为：不发 message_start / content_block_start，直接发 delta。
        let mut dec = MessagesStreamDecoder::default();
        let events = dec
            .decode(&SseFrame::named(
                "content_block_delta",
                json!({ "type": "content_block_delta", "index": 0,
                        "delta": { "type": "text_delta", "text": "hi" }})
                .to_string(),
            ))
            .expect("decode must not fail");
        assert_eq!(
            events,
            vec![
                // 缺失的 ContentStart 由解码器补上。
                StreamEvent::ContentStart {
                    index: 0,
                    kind: PartKind::Text
                },
                StreamEvent::TextDelta {
                    index: 0,
                    text: "hi".into()
                },
            ]
        );
        // 同一个块的第二个 delta 不该再补 ContentStart。
        let events = dec
            .decode(&SseFrame::named(
                "content_block_delta",
                json!({ "type": "content_block_delta", "index": 0,
                        "delta": { "type": "text_delta", "text": "!" }})
                .to_string(),
            ))
            .expect("decode");
        assert_eq!(
            events,
            vec![StreamEvent::TextDelta {
                index: 0,
                text: "!".into()
            }]
        );

        // 上游断流没发 message_stop 时，finish 补 Done。
        assert_eq!(dec.finish().expect("finish"), vec![StreamEvent::Done]);
    }

    #[test]
    fn stream_decoder_falls_back_to_payload_type_and_reports_errors() {
        // 事件名缺失时用载荷里的 type。
        let mut dec = MessagesStreamDecoder::default();
        let events = dec
            .decode(&SseFrame::data(
                json!({ "type": "message_stop" }).to_string(),
            ))
            .expect("decode");
        assert_eq!(events, vec![StreamEvent::Done]);

        // error 事件。
        let mut dec = MessagesStreamDecoder::default();
        let events = dec
            .decode(&SseFrame::named(
                "error",
                json!({ "type": "error",
                        "error": { "type": "overloaded_error", "message": "busy" }})
                .to_string(),
            ))
            .expect("decode");
        assert_eq!(
            events,
            vec![StreamEvent::Error {
                message: "busy".into(),
                kind: "overloaded_error".into(),
            }]
        );

        // 未知事件被忽略而不是报错。
        let mut dec = MessagesStreamDecoder::default();
        assert_eq!(
            dec.decode(&SseFrame::named("brand_new_event", "{}"))
                .expect("decode"),
            vec![]
        );

        // 坏 JSON 只跳过该帧：中转站的裸文本心跳不该让整个回答消失。
        let mut dec = MessagesStreamDecoder::default();
        assert_eq!(
            dec.decode(&SseFrame::named("message_delta", "{not json"))
                .expect("坏 JSON 不该终止流"),
            vec![]
        );
        // 且跳过之后仍能解析真实增量。
        let after = dec
            .decode(&SseFrame::named(
                "content_block_delta",
                json!({ "type": "content_block_delta", "index": 0,
                        "delta": { "type": "text_delta", "text": "hi" } })
                .to_string(),
            ))
            .expect("坏帧之后仍应能解码");
        assert!(
            after
                .iter()
                .any(|e| matches!(e, StreamEvent::TextDelta { text, .. } if text == "hi"))
        );
    }

    #[test]
    fn stream_encoder_emits_complete_sequence_from_bare_deltas() {
        // 上游是 OpenAI Chat：只有裸 TextDelta，没有任何仪式性事件。
        // 编码器必须自己造出完整的 Anthropic 序列。
        let frames = encode_all(&[
            StreamEvent::TextDelta {
                index: 0,
                text: "he".into(),
            },
            StreamEvent::TextDelta {
                index: 0,
                text: "llo".into(),
            },
            StreamEvent::Stop {
                reason: StopReason::Stop,
                stop_sequence: None,
            },
            StreamEvent::Done,
        ]);

        assert_eq!(
            event_names(&frames),
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );

        // message_start 必须是个结构完整的 message 对象。
        let start = parse(&frames[0]);
        assert_eq!(start["message"]["type"], json!("message"));
        assert_eq!(start["message"]["role"], json!("assistant"));
        assert!(
            start["message"]["id"]
                .as_str()
                .is_some_and(|s| !s.is_empty()),
            "message_start must carry an id even when upstream sent none"
        );
        assert_eq!(parse(&frames[1])["content_block"]["type"], json!("text"));
        assert_eq!(
            parse(&frames[2])["delta"],
            json!({ "type": "text_delta", "text": "he" })
        );
        assert_eq!(
            parse(&frames[5])["delta"],
            json!({ "stop_reason": "end_turn", "stop_sequence": Value::Null })
        );
    }

    #[test]
    fn stream_encoder_renumbers_sparse_indices_and_switches_blocks() {
        // IR 下标可能稀疏（Responses 的 output_index 会跳号），
        // Anthropic 要求从 0 连续递增。
        let frames = encode_all(&[
            StreamEvent::Start {
                id: "msg_x".into(),
                model: "claude".into(),
                usage: Some(Usage {
                    input_tokens: 9,
                    ..Default::default()
                }),
            },
            StreamEvent::ThinkingDelta {
                index: 5,
                text: "think".into(),
            },
            StreamEvent::ThinkingSignature {
                index: 5,
                signature: "SIG".into(),
            },
            StreamEvent::TextDelta {
                index: 9,
                text: "answer".into(),
            },
            StreamEvent::ToolCallStart {
                signature: None,
                index: 12,
                id: "toolu_7".into(),
                name: "f".into(),
            },
            StreamEvent::ToolCallArgsDelta {
                index: 12,
                fragment: "{}".into(),
            },
            StreamEvent::Usage(Usage {
                output_tokens: 42,
                ..Default::default()
            }),
            StreamEvent::Stop {
                reason: StopReason::ToolUse,
                stop_sequence: None,
            },
            StreamEvent::Done,
        ]);

        // 收集所有出现过的块下标，必须是 0,1,2。
        let mut seen = Vec::new();
        for frame in &frames {
            if let Some(idx) = parse(frame).get("index").and_then(Value::as_u64)
                && !seen.contains(&idx)
            {
                seen.push(idx);
            }
        }
        assert_eq!(seen, vec![0, 1, 2], "sparse IR indices must be renumbered");

        // 换块时必须先 stop 旧块再 start 新块。
        let names = event_names(&frames);
        assert_eq!(
            names,
            vec![
                "message_start",
                "content_block_start", // thinking, index 0
                "content_block_delta", // thinking_delta
                "content_block_delta", // signature_delta
                "content_block_stop",  // 关掉 thinking
                "content_block_start", // text, index 1
                "content_block_delta",
                "content_block_stop",  // 关掉 text
                "content_block_start", // tool_use, index 2
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );

        // signature 必须以 signature_delta 的形式出现在流里。
        let sig = frames
            .iter()
            .map(parse)
            .find(|v| v["delta"]["type"] == json!("signature_delta"))
            .expect("signature_delta must be emitted");
        assert_eq!(sig["delta"]["signature"], json!("SIG"));
        assert_eq!(sig["index"], json!(0));

        // 工具块要带 id 与 name，否则客户端没法执行。
        let tool_start = frames
            .iter()
            .map(parse)
            .find(|v| v["content_block"]["type"] == json!("tool_use"))
            .expect("tool_use block start");
        assert_eq!(tool_start["content_block"]["id"], json!("toolu_7"));
        assert_eq!(tool_start["content_block"]["name"], json!("f"));

        // message_start 带 input usage，message_delta 带 output。
        assert_eq!(
            parse(&frames[0])["message"]["usage"]["input_tokens"],
            json!(9)
        );
        let delta = parse(&frames[11]);
        assert_eq!(delta["usage"]["output_tokens"], json!(42));
        assert_eq!(delta["delta"]["stop_reason"], json!("tool_use"));
    }

    #[test]
    fn stream_encoder_finishes_even_without_any_events() {
        // 上游一个字都没回（比如立即被内容策略拦截），客户端仍需要一个
        // 结构完整的流，否则 SDK 会挂在等 message_start 上。
        let mut enc = MessagesStreamEncoder::default();
        let frames = enc.finish().expect("finish");
        assert_eq!(
            event_names(&frames),
            vec!["message_start", "message_delta", "message_stop"]
        );

        // 重复 finish 不该再产出帧。
        assert_eq!(enc.finish().expect("finish"), vec![]);
    }

    #[test]
    fn stream_encoder_stops_emitting_after_terminal_events() {
        let mut enc = MessagesStreamEncoder::default();
        let mut frames = enc
            .encode(&StreamEvent::TextDelta {
                index: 0,
                text: "x".into(),
            })
            .expect("encode");
        frames.extend(enc.encode(&StreamEvent::Done).expect("encode"));
        assert_eq!(*event_names(&frames).last().unwrap(), "message_stop");

        // message_stop 之后来的事件必须被丢弃，否则客户端状态机错乱。
        assert_eq!(
            enc.encode(&StreamEvent::TextDelta {
                index: 0,
                text: "late".into()
            })
            .expect("encode"),
            vec![]
        );
        assert_eq!(enc.finish().expect("finish"), vec![]);

        // 错误事件同样终结流。
        let mut enc = MessagesStreamEncoder::default();
        let frames = enc
            .encode(&StreamEvent::Error {
                message: "boom".into(),
                kind: "overloaded_error".into(),
            })
            .expect("encode");
        assert_eq!(event_names(&frames), vec!["error"]);
        assert_eq!(
            parse(&frames[0]),
            json!({ "type": "error", "error": { "type": "overloaded_error", "message": "boom" }})
        );
        assert_eq!(enc.finish().expect("finish"), vec![]);
    }

    #[test]
    fn stream_encoder_synthesizes_tool_block_for_orphan_args() {
        // OpenAI Chat 的后续工具帧只有 arguments 片段，没有 id/name。
        // 丢掉入参会让工具调用变成空壳，所以要补一个块。
        let frames = encode_all(&[
            StreamEvent::ToolCallArgsDelta {
                index: 0,
                fragment: "{\"a\":1".into(),
            },
            StreamEvent::Done,
        ]);
        assert_eq!(
            event_names(&frames),
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        let start = parse(&frames[1]);
        assert_eq!(start["content_block"]["type"], json!("tool_use"));
        // id 缺失时必须自己造一个，Anthropic 的 tool_use 块不能没有 id。
        assert!(
            start["content_block"]["id"]
                .as_str()
                .is_some_and(|s| s.starts_with("toolu_")),
            "synthesized tool block needs an id, got {:?}",
            start["content_block"]["id"]
        );
        // 截断的入参片段要原样透传，不能因为不是合法 JSON 就丢掉。
        assert_eq!(
            parse(&frames[2])["delta"],
            json!({ "type": "input_json_delta", "partial_json": "{\"a\":1" })
        );
    }

    #[test]
    fn roundtrip_through_stream_preserves_thinking_signature() {
        // 端到端：Anthropic SSE → IR 事件 → Anthropic SSE，signature 必须活着。
        let mut dec = MessagesStreamDecoder::default();
        let upstream = [
            SseFrame::named(
                "message_start",
                json!({ "type": "message_start",
                        "message": { "id": "msg_1", "model": "claude",
                                     "usage": { "input_tokens": 3 }}})
                .to_string(),
            ),
            SseFrame::named(
                "content_block_start",
                json!({ "type": "content_block_start", "index": 0,
                        "content_block": { "type": "thinking", "thinking": "" }})
                .to_string(),
            ),
            SseFrame::named(
                "content_block_delta",
                json!({ "type": "content_block_delta", "index": 0,
                        "delta": { "type": "thinking_delta", "thinking": "deep" }})
                .to_string(),
            ),
            SseFrame::named(
                "content_block_delta",
                json!({ "type": "content_block_delta", "index": 0,
                        "delta": { "type": "signature_delta", "signature": "SIG-XYZ" }})
                .to_string(),
            ),
            SseFrame::named(
                "content_block_stop",
                json!({ "type": "content_block_stop", "index": 0 }).to_string(),
            ),
            SseFrame::named(
                "message_stop",
                json!({ "type": "message_stop" }).to_string(),
            ),
        ];
        let mut events = Vec::new();
        for frame in &upstream {
            events.extend(dec.decode(frame).expect("decode"));
        }

        let out = encode_all(&events);
        let sig = out
            .iter()
            .map(parse)
            .find(|v| v["delta"]["type"] == json!("signature_delta"))
            .expect("signature must survive the full round trip");
        assert_eq!(sig["delta"]["signature"], json!("SIG-XYZ"));

        // 聚合回 IR 时 signature 也要在。
        let mut agg = crate::stream::StreamAggregator::new();
        for ev in &events {
            agg.absorb(ev);
        }
        assert_eq!(
            agg.into_content(),
            vec![ContentPart::Thinking {
                text: "deep".into(),
                signature: Some("SIG-XYZ".into()),
            }]
        );
    }

    #[test]
    fn protocol_is_messages() {
        assert_eq!(MESSAGES.protocol(), Protocol::Messages);
    }
}
