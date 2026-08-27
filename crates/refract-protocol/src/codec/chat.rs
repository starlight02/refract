//! OpenAI Chat Completions 协议编解码。
//!
//! Chat 是四个协议里表达能力最弱的一个：没有推理块、没有内容块下标、工具入参
//! 是 JSON **字符串**而非对象。所以本 codec 的大部分复杂度都在两件事上：
//!
//! 1. **保住 IR 里 Chat 表达不了的东西**。最要命的是 [`ContentPart::Thinking`]
//!    的 signature —— 从 Anthropic 转过来再转回去时若丢了它，多轮工具调用会被
//!    上游整个拒绝。这里的做法是编码时存进 `dropped_thinking`，解码时原样还原。
//! 2. **容忍中转站**。真实流量里大量请求来自实现不完整的中转站：content 可能是
//!    裸字符串、推理走非标准的 `reasoning_content`、仪式性的开场帧直接省略。
//!    解码一律宽容，编码一律补齐。

use refract_core::{ErrorKind, GatewayError, Protocol};
use serde_json::{Map, Value, json};

use crate::codec::{ProtocolCodec, RequestCodec, ResponseCodec, StreamCodec};
use crate::ir::*;
use crate::stream::*;

/// OpenAI Chat Completions codec。
pub struct ChatCodec;

/// 供 [`crate::codec::CodecSet`] 注册的单例。
pub static CHAT: ChatCodec = ChatCodec;

/// 本协议在 [`Extensions`] 里的键前缀。
const EXT_PREFIX: &str = "chat.";

/// 文本、推理、拒答共用的块下标。
///
/// Chat 的 delta 没有内容块概念，一条流只有一路输出，所以全部落在 0；
/// 区分靠 [`StreamEvent`] 的变体而非下标。
const TEXT_INDEX: u32 = 0;

/// 工具调用块的 IR 下标基址，避开文本块 0。
const TOOL_INDEX_BASE: u32 = 1;

/// 流终止哨兵。
const DONE_SENTINEL: &str = "[DONE]";

/// 请求里已被 IR 显式建模的字段，其余一律进 [`Extensions`]。
const KNOWN_REQUEST_FIELDS: &[&str] = &[
    "model",
    "messages",
    "stream",
    "stream_options",
    "max_tokens",
    "max_completion_tokens",
    "temperature",
    "top_p",
    "frequency_penalty",
    "presence_penalty",
    "seed",
    "n",
    "stop",
    "reasoning_effort",
    "response_format",
    "tools",
    "tool_choice",
    "parallel_tool_calls",
    "user",
    "dropped_thinking",
];

/// 响应里已被 IR 显式建模的字段。
const KNOWN_RESPONSE_FIELDS: &[&str] = &[
    "id",
    "object",
    "created",
    "model",
    "choices",
    "usage",
    "dropped_thinking",
];

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

/// 宽松地把一个 JSON 数取成 `u32`。
fn as_u32(v: Option<&Value>) -> Option<u32> {
    v.and_then(Value::as_u64)
        .map(|n| n.min(u64::from(u32::MAX)) as u32)
}

/// 把未在 `known` 中列出的顶层字段收进 [`Extensions`]。
fn collect_unknown(obj: &Map<String, Value>, known: &[&str], ext: &mut Extensions) {
    for (k, v) in obj {
        if known.contains(&k.as_str()) {
            continue;
        }
        ext.insert(format!("{EXT_PREFIX}{k}"), v.clone());
    }
}

/// 把 [`Extensions`] 里属于本协议的字段还原回顶层。
///
/// 已经由 IR 字段生成的键优先 —— 扩展是兜底，不该覆盖归一化过的值。
/// `builtin_tools` 是自造的搬运键（已并入 `tools` 数组），写成顶层字段
/// 会被上游当未知字段拒绝，跳过。
fn restore_extensions(ext: &Extensions, body: &mut Map<String, Value>) {
    for (k, v) in ext {
        let Some(field) = k.strip_prefix(EXT_PREFIX) else {
            continue;
        };
        if field.is_empty() || field == "builtin_tools" || body.contains_key(field) {
            continue;
        }
        body.insert(field.to_owned(), v.clone());
    }
}

/// `stop` 可以是字符串也可以是数组。
fn parse_stop(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

/// 解析 `content` 字段：字符串、part 数组、或干脆没有。
fn parse_content(value: Option<&Value>) -> Vec<ContentPart> {
    match value {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::String(s)) if s.is_empty() => Vec::new(),
        Some(Value::String(s)) => vec![ContentPart::text(s.clone())],
        Some(Value::Array(items)) => items.iter().filter_map(parse_content_part).collect(),
        Some(other) => {
            tracing::debug!(shape = ?other, "chat: 无法识别的 content 形态，已忽略");
            Vec::new()
        }
    }
}

/// 解析单个 content part。未知类型能捞出文本就当文本，否则丢弃。
fn parse_content_part(raw: &Value) -> Option<ContentPart> {
    // 见过中转站直接往 part 数组里塞裸字符串。
    if let Value::String(s) = raw {
        return Some(ContentPart::text(s.clone()));
    }
    let obj = raw.as_object()?;
    let kind = obj.get("type").and_then(Value::as_str).unwrap_or("");
    match kind {
        // input_text / output_text 是 Responses 协议的叫法，中转站常混用。
        "text" | "input_text" | "output_text" => Some(ContentPart::text(
            obj.get("text").and_then(Value::as_str).unwrap_or_default(),
        )),
        "refusal" => Some(ContentPart::Refusal {
            text: obj
                .get("refusal")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        }),
        "image_url" => {
            let img = obj.get("image_url");
            let url = match img {
                Some(Value::String(s)) => s.as_str(),
                Some(v) => v.get("url").and_then(Value::as_str).unwrap_or_default(),
                None => "",
            };
            let detail = img
                .and_then(|v| v.get("detail"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            let (source, mime) = MediaSource::parse_data_uri(url);
            Some(ContentPart::Image {
                source,
                mime,
                detail,
            })
        }
        "input_audio" => {
            let audio = obj.get("input_audio")?;
            let data = audio
                .get("data")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let format = audio
                .get("format")
                .and_then(Value::as_str)
                .map(str::to_owned);
            // 规范里 data 是裸 base64，但也见过写成 data URI 的。
            let (source, _) = MediaSource::parse_data_uri(data);
            let source = match source {
                MediaSource::Url(u) if !u.starts_with("http") => MediaSource::Base64(u),
                other => other,
            };
            Some(ContentPart::Audio { source, format })
        }
        "file" => {
            let file = obj.get("file")?;
            let name = file
                .get("filename")
                .and_then(Value::as_str)
                .map(str::to_owned);
            if let Some(id) = file.get("file_id").and_then(Value::as_str) {
                return Some(ContentPart::File {
                    source: MediaSource::FileId(id.to_owned()),
                    mime: None,
                    name,
                });
            }
            let data = file
                .get("file_data")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let (source, mime) = MediaSource::parse_data_uri(data);
            Some(ContentPart::File { source, mime, name })
        }
        _ => {
            if let Some(t) = obj.get("text").and_then(Value::as_str) {
                return Some(ContentPart::text(t));
            }
            tracing::debug!(kind, "chat: 未知 content part 类型，已忽略");
            None
        }
    }
}

/// 解析 assistant 的 `tool_calls[]`。
fn parse_tool_calls(calls: &[Value], out: &mut Vec<ContentPart>) {
    for call in calls {
        let id = call
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let function = call.get("function");
        let name = function
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let input = match function.and_then(|f| f.get("arguments")) {
            // 规范要求 arguments 是 JSON 字符串。解析失败时原样留成字符串，
            // 让上层看得到上游到底发了什么，而不是把内容吞掉。
            Some(Value::String(s)) if s.trim().is_empty() => json!({}),
            Some(Value::String(s)) => {
                serde_json::from_str(s).unwrap_or_else(|_| Value::String(s.clone()))
            }
            // 部分中转站直接给对象，照单全收。
            Some(other) => other.clone(),
            None => json!({}),
        };
        out.push(ContentPart::ToolUse {
            id,
            name,
            input,
            signature: None,
        });
    }
}

/// 按 `position` 把 `dropped_thinking` 记录还原成内容片段插回原位。
fn restore_dropped_parts(items: &[&Value], out: &mut Vec<ContentPart>) {
    for item in items {
        let part = if let Some(data) = item.get("redacted").and_then(Value::as_str) {
            ContentPart::RedactedThinking {
                data: data.to_owned(),
            }
        } else {
            ContentPart::Thinking {
                text: item
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                signature: item
                    .get("signature")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            }
        };
        let pos =
            (item.get("position").and_then(Value::as_u64).unwrap_or(0) as usize).min(out.len());
        out.insert(pos, part);
    }
}

/// 取出定位到某条线格式消息的 `dropped_thinking` 记录。
fn dropped_for(items: Option<&Vec<Value>>, wire_index: usize) -> Vec<&Value> {
    items.map_or_else(Vec::new, |items| {
        items
            .iter()
            .filter(|it| it.get("message").and_then(Value::as_u64) == Some(wire_index as u64))
            .collect()
    })
}

/// 解析 `response_format`。
fn parse_response_format(raw: &Value) -> Option<ResponseFormat> {
    match raw.get("type").and_then(Value::as_str)? {
        "text" => Some(ResponseFormat::Text),
        "json_object" => Some(ResponseFormat::JsonObject),
        "json_schema" => {
            let schema = raw.get("json_schema")?;
            Some(ResponseFormat::JsonSchema {
                name: schema
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("response")
                    .to_owned(),
                schema: schema.get("schema").cloned().unwrap_or_else(|| json!({})),
                strict: schema
                    .get("strict")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            })
        }
        other => {
            tracing::debug!(other, "chat: 未知 response_format.type，已忽略");
            None
        }
    }
}

/// 渲染 `response_format`。
fn response_format_json(rf: &ResponseFormat) -> Value {
    match rf {
        ResponseFormat::Text => json!({"type": "text"}),
        ResponseFormat::JsonObject => json!({"type": "json_object"}),
        ResponseFormat::JsonSchema {
            name,
            schema,
            strict,
        } => json!({
            "type": "json_schema",
            "json_schema": {"name": name, "schema": schema, "strict": strict},
        }),
    }
}

/// 渲染一条工具声明。
fn tool_json(tool: &ToolDef) -> Value {
    let mut function = Map::new();
    function.insert("name".into(), json!(tool.name));
    if let Some(d) = &tool.description {
        function.insert("description".into(), json!(d));
    }
    function.insert("parameters".into(), tool.parameters.clone());
    if let Some(s) = tool.strict {
        function.insert("strict".into(), json!(s));
    }
    json!({"type": "function", "function": Value::Object(function)})
}

/// 渲染内容片段列表。
///
/// 全是文本时输出字符串而非数组 —— 语义同 [`Message::is_plain_text`]，但作用在
/// 剥掉 tool_calls / refusal / 推理块之后的可见片段上，所以带工具调用的纯文本
/// 消息也能享受这个兼容性优化。
fn content_json(parts: &[&ContentPart]) -> Value {
    if parts.is_empty() {
        return Value::String(String::new());
    }
    if parts.iter().all(|p| matches!(p, ContentPart::Text { .. })) {
        let mut joined = String::new();
        for p in parts {
            if let ContentPart::Text { text } = p {
                joined.push_str(text);
            }
        }
        return Value::String(joined);
    }
    Value::Array(parts.iter().copied().filter_map(part_json).collect())
}

/// 渲染单个内容片段。返回 `None` 表示该片段由调用方单独处理。
fn part_json(part: &ContentPart) -> Option<Value> {
    Some(match part {
        ContentPart::Text { text } => json!({"type": "text", "text": text}),
        ContentPart::Image {
            source,
            mime,
            detail,
        } => {
            let mut image_url = json!({"url": source.to_data_uri(mime.as_deref())});
            if let Some(d) = detail {
                image_url["detail"] = json!(d);
            }
            json!({"type": "image_url", "image_url": image_url})
        }
        ContentPart::Audio { source, format } => {
            // input_audio.data 收的是裸 base64，不加 data URI 前缀。
            let data = match source {
                MediaSource::Base64(d) => d.clone(),
                other => other.to_data_uri(None),
            };
            json!({
                "type": "input_audio",
                "input_audio": {"data": data, "format": format.as_deref().unwrap_or("wav")},
            })
        }
        ContentPart::File { source, mime, name } => {
            let mut file = Map::new();
            match source {
                MediaSource::FileId(id) => {
                    file.insert("file_id".into(), json!(id));
                }
                other => {
                    file.insert(
                        "file_data".into(),
                        json!(other.to_data_uri(mime.as_deref())),
                    );
                }
            }
            if let Some(n) = name {
                file.insert("filename".into(), json!(n));
            }
            json!({"type": "file", "file": Value::Object(file)})
        }
        ContentPart::Refusal { text } => json!({"type": "refusal", "refusal": text}),
        // Opaque 是其他协议的私有块（chat 自己解码不产生 Opaque），没有
        // 对应的 Chat 形态，丢弃。
        ContentPart::Thinking { .. }
        | ContentPart::RedactedThinking { .. }
        | ContentPart::ToolUse { .. }
        | ContentPart::ToolResult { .. }
        | ContentPart::Opaque { .. } => return None,
    })
}

/// 解析 usage。
fn parse_usage(raw: &Value) -> Usage {
    Usage {
        input_tokens: raw
            .get("prompt_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        output_tokens: raw
            .get("completion_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cached_input_tokens: raw
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        // Chat 不区分「写入缓存」，这个字段只有 Anthropic 有。
        cache_write_tokens: 0,
        reasoning_tokens: raw
            .get("completion_tokens_details")
            .and_then(|d| d.get("reasoning_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
    }
}

/// 渲染 usage。
fn usage_json(usage: &Usage) -> Value {
    let mut out = Map::new();
    out.insert("prompt_tokens".into(), json!(usage.input_tokens));
    out.insert("completion_tokens".into(), json!(usage.output_tokens));
    out.insert("total_tokens".into(), json!(usage.total()));
    if usage.cached_input_tokens > 0 {
        out.insert(
            "prompt_tokens_details".into(),
            json!({"cached_tokens": usage.cached_input_tokens}),
        );
    }
    if usage.reasoning_tokens > 0 {
        out.insert(
            "completion_tokens_details".into(),
            json!({"reasoning_tokens": usage.reasoning_tokens}),
        );
    }
    Value::Object(out)
}

/// `finish_reason` → IR。
fn finish_reason_to_ir(raw: &str) -> StopReason {
    match raw {
        "stop" => StopReason::Stop,
        "length" => StopReason::MaxTokens,
        // function_call 是已废弃的旧字段，语义等同 tool_calls。
        "tool_calls" | "function_call" => StopReason::ToolUse,
        "content_filter" => StopReason::ContentFilter,
        other => {
            tracing::debug!(other, "chat: 未知 finish_reason");
            StopReason::Other
        }
    }
}

/// IR → `finish_reason`。
///
/// Chat 只有四个取值，IR 里多出来的那些只能收敛到 `stop`：OpenAI 用
/// `message.refusal` 而非 finish_reason 表达拒答，也没有 pause_turn 的概念。
fn stop_reason_to_finish(reason: StopReason) -> &'static str {
    match reason {
        StopReason::MaxTokens => "length",
        StopReason::ToolUse => "tool_calls",
        StopReason::ContentFilter => "content_filter",
        StopReason::Stop
        | StopReason::StopSequence
        | StopReason::Refusal
        | StopReason::PauseTurn
        | StopReason::Other => "stop",
    }
}

/// 把 `{error:{message,type,param,code}}` 翻译成网关错误。
fn error_from_body(err: &Value) -> GatewayError {
    let mut message = err
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("upstream returned an error")
        .to_owned();
    let kind_str = err.get("type").and_then(Value::as_str).unwrap_or("");
    let code = err.get("code").and_then(Value::as_str).unwrap_or("");
    let kind = match (kind_str, code) {
        ("invalid_request_error", "context_length_exceeded") => ErrorKind::PayloadTooLarge,
        ("invalid_request_error", _) => ErrorKind::InvalidRequest,
        ("authentication_error", _) => ErrorKind::Unauthenticated,
        ("permission_error" | "insufficient_quota", _) => ErrorKind::PermissionDenied,
        ("not_found_error", _) => ErrorKind::NotFound,
        ("rate_limit_error", _) | (_, "rate_limit_exceeded") => ErrorKind::RateLimited,
        (_, "model_not_found") => ErrorKind::NotFound,
        _ => ErrorKind::UpstreamError,
    };
    // param 指明出错字段，对排查很有用，直接拼进消息里给客户端看。
    if let Some(param) = err.get("param").and_then(Value::as_str) {
        message = format!("{message} (param: {param})");
    }
    GatewayError::new(kind, message).with_protocol(Protocol::Chat)
}

// ---------------------------------------------------------------------------
// 请求
// ---------------------------------------------------------------------------

impl RequestCodec for ChatCodec {
    fn decode_request(&self, raw: &Value) -> Result<UnifiedRequest, GatewayError> {
        let obj = raw
            .as_object()
            .ok_or_else(|| GatewayError::invalid_request("request body must be a JSON object"))?;

        let model = obj
            .get("model")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| GatewayError::invalid_request("missing required field `model`"))?
            .to_owned();

        let raw_messages = obj
            .get("messages")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                GatewayError::invalid_request(
                    "missing required field `messages` (expected an array)",
                )
            })?;

        let mut req = UnifiedRequest::new(model, Vec::new());

        // dropped_thinking 的记录带 signature，比 reasoning_content 更完整。
        // 两者并存时（本 codec 编码出去、又原样转回来）只认前者，否则同一段
        // 推理会被还原两次。记录按**线格式消息下标**定位，而非 IR 下标 ——
        // 一条多结果的 Tool 消息会拆成多条 chat 消息，两种下标会错位。
        let dropped = obj.get("dropped_thinking").and_then(Value::as_array);

        for (i, m) in raw_messages.iter().enumerate() {
            let role = m.get("role").and_then(Value::as_str).ok_or_else(|| {
                GatewayError::invalid_request(format!("messages[{i}] is missing `role`"))
            })?;
            let name = m.get("name").and_then(Value::as_str).map(str::to_owned);

            match role {
                // developer 是 system 的新叫法，两者都提到顶层 system。
                "system" | "developer" => req.system.extend(parse_content(m.get("content"))),
                "user" => {
                    let mut msg = Message::new(Role::User, parse_content(m.get("content")));
                    msg.name = name;
                    req.messages.push(msg);
                }
                "assistant" => {
                    let restored = dropped_for(dropped, i);
                    let mut parts = Vec::new();
                    // 非标准字段，但 DeepSeek 系与大量中转站靠它做多轮推理续传。
                    // 有 dropped 记录时以记录为准，避免同一段推理还原两次。
                    if let Some(t) = m
                        .get("reasoning_content")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .filter(|_| restored.is_empty())
                    {
                        parts.push(ContentPart::Thinking {
                            text: t.to_owned(),
                            signature: m
                                .get("reasoning_signature")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                        });
                    }
                    parts.extend(parse_content(m.get("content")));
                    if let Some(r) = m
                        .get("refusal")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                    {
                        parts.push(ContentPart::Refusal { text: r.to_owned() });
                    }
                    if let Some(calls) = m.get("tool_calls").and_then(Value::as_array) {
                        parse_tool_calls(calls, &mut parts);
                    }
                    // 按 position 把推理块插回原位（含 Anthropic signature）。
                    restore_dropped_parts(&restored, &mut parts);
                    let mut msg = Message::new(Role::Assistant, parts);
                    msg.name = name;
                    req.messages.push(msg);
                }
                "tool" => {
                    // tool_call_id 是硬性要求：丢了它，转成 Anthropic/Gemini 后
                    // 工具结果无法关联回调用，上游会直接 400。宁可现在报错。
                    let id = m
                        .get("tool_call_id")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .ok_or_else(|| {
                            GatewayError::invalid_request(format!(
                                "messages[{i}] with role `tool` is missing `tool_call_id`"
                            ))
                        })?
                        .to_owned();
                    req.messages.push(Message::new(
                        Role::Tool,
                        vec![ContentPart::ToolResult {
                            id,
                            // Chat 的 tool 消息不带函数名；路由层编码前会按
                            // 对话历史里的 ToolUse 反查补全（Gemini 需要）。
                            name: None,
                            content: parse_content(m.get("content")),
                            is_error: false,
                        }],
                    ));
                }
                other => {
                    return Err(GatewayError::invalid_request(format!(
                        "messages[{i}] has unsupported role `{other}`"
                    )));
                }
            }
        }

        req.stream = obj.get("stream").and_then(Value::as_bool).unwrap_or(false);
        req.stream_include_usage = obj
            .get("stream_options")
            .and_then(|o| o.get("include_usage"))
            .and_then(Value::as_bool)
            .unwrap_or(false);

        // max_tokens 已废弃，max_completion_tokens 优先；两个都读。
        req.max_output_tokens =
            as_u32(obj.get("max_completion_tokens")).or_else(|| as_u32(obj.get("max_tokens")));

        req.sampling = Sampling {
            temperature: obj.get("temperature").and_then(Value::as_f64),
            top_p: obj.get("top_p").and_then(Value::as_f64),
            // Chat 没有 top_k。
            top_k: None,
            frequency_penalty: obj.get("frequency_penalty").and_then(Value::as_f64),
            presence_penalty: obj.get("presence_penalty").and_then(Value::as_f64),
            stop: parse_stop(obj.get("stop")),
            seed: obj.get("seed").and_then(Value::as_i64),
            candidate_count: as_u32(obj.get("n")),
        };

        req.reasoning = obj
            .get("reasoning_effort")
            .and_then(Value::as_str)
            .map(|e| ReasoningConfig {
                effort: Some(e.to_owned()),
                budget_tokens: None,
                include_thoughts: None,
            });

        req.response_format = obj.get("response_format").and_then(parse_response_format);

        if let Some(tools) = obj.get("tools").and_then(Value::as_array) {
            let mut builtins = Vec::new();
            for t in tools {
                // 只有 function 工具有跨协议对等物；内置工具（web_search
                // 之类）原文留存，chat→chat 直通时还原。
                let Some(f) = t.get("function") else {
                    builtins.push(t.clone());
                    continue;
                };
                let Some(name) = f.get("name").and_then(Value::as_str) else {
                    continue;
                };
                req.tools.push(ToolDef {
                    name: name.to_owned(),
                    description: f
                        .get("description")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    parameters: f
                        .get("parameters")
                        .cloned()
                        .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
                    strict: f.get("strict").and_then(Value::as_bool),
                });
            }
            if !builtins.is_empty() {
                req.set_extension(format!("{EXT_PREFIX}builtin_tools"), Value::Array(builtins));
            }
        }

        req.tool_choice = match obj.get("tool_choice") {
            None | Some(Value::Null) => ToolChoice::Unspecified,
            Some(Value::String(s)) => match s.as_str() {
                "none" => ToolChoice::None,
                "auto" => ToolChoice::Auto,
                // "any" 是 Anthropic 的说法，中转站会混用。
                "required" | "any" => ToolChoice::Required,
                other => {
                    tracing::debug!(other, "chat: 未知 tool_choice");
                    ToolChoice::Unspecified
                }
            },
            Some(v) => v
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .map_or(ToolChoice::Unspecified, |n| ToolChoice::Tool(n.to_owned())),
        };

        req.parallel_tool_calls = obj.get("parallel_tool_calls").and_then(Value::as_bool);
        req.user = obj.get("user").and_then(Value::as_str).map(str::to_owned);

        collect_unknown(obj, KNOWN_REQUEST_FIELDS, &mut req.extensions);
        Ok(req)
    }

    fn encode_request(&self, ir: &UnifiedRequest) -> Result<Value, GatewayError> {
        let mut body = Map::new();
        body.insert("model".into(), json!(ir.model));

        let mut messages = Vec::with_capacity(ir.messages.len() + 1);
        if !ir.system.is_empty() {
            // system 是从多条独立 system/developer 消息累积来的，直接拼接会把
            // 相邻两条的词粘在一起。用 system_text() 的换行语义还原成一条。
            let content = if ir
                .system
                .iter()
                .all(|p| matches!(p, ContentPart::Text { .. }))
            {
                Value::String(ir.system_text())
            } else {
                let refs: Vec<&ContentPart> = ir.system.iter().collect();
                content_json(&refs)
            };
            messages.push(json!({"role": "system", "content": content}));
        }

        for msg in &ir.messages {
            encode_message(msg, &mut messages);
        }
        body.insert("messages".into(), Value::Array(messages));

        if let Some(v) = ir.sampling.temperature {
            body.insert("temperature".into(), json!(v));
        }
        if let Some(v) = ir.sampling.top_p {
            body.insert("top_p".into(), json!(v));
        }
        if ir.sampling.top_k.is_some() {
            tracing::debug!("chat: top_k 无对等字段，已丢弃");
        }
        if let Some(v) = ir.sampling.frequency_penalty {
            body.insert("frequency_penalty".into(), json!(v));
        }
        if let Some(v) = ir.sampling.presence_penalty {
            body.insert("presence_penalty".into(), json!(v));
        }
        if let Some(v) = ir.sampling.seed {
            body.insert("seed".into(), json!(v));
        }
        if let Some(v) = ir.sampling.candidate_count {
            body.insert("n".into(), json!(v));
        }
        // 单个停止序列输出成字符串 —— 有些中转站只认这一种形态。
        match ir.sampling.stop.len() {
            0 => {}
            1 => {
                body.insert("stop".into(), json!(ir.sampling.stop[0]));
            }
            _ => {
                body.insert("stop".into(), json!(ir.sampling.stop));
            }
        }

        if let Some(m) = ir.max_output_tokens {
            body.insert("max_completion_tokens".into(), json!(m));
        }
        if ir.stream {
            body.insert("stream".into(), json!(true));
            if ir.stream_include_usage {
                body.insert("stream_options".into(), json!({"include_usage": true}));
            }
        }
        if let Some(r) = &ir.reasoning {
            // chat→chat 时 effort 原样透传（"none"/"minimal" 等新档位不能被
            // 折算映射吞掉）；只有 budget_tokens（来自 Anthropic/Gemini）时
            // 才折算成档位。
            if let Some(effort) = &r.effort {
                body.insert("reasoning_effort".into(), json!(effort));
            } else if let Some(effort) = r.effort_or_from_budget(ir.max_output_tokens) {
                body.insert("reasoning_effort".into(), json!(effort));
            }
        }
        if let Some(rf) = &ir.response_format {
            body.insert("response_format".into(), response_format_json(rf));
        }
        let mut tools: Vec<Value> = ir.tools.iter().map(tool_json).collect();
        // chat→chat 直通时还原内置工具声明（web_search 等）。
        if let Some(Value::Array(builtins)) = ir.extension(&format!("{EXT_PREFIX}builtin_tools")) {
            tools.extend(builtins.iter().cloned());
        }
        if !tools.is_empty() {
            body.insert("tools".into(), Value::Array(tools));
        }
        match &ir.tool_choice {
            ToolChoice::Unspecified => {}
            ToolChoice::Auto => {
                body.insert("tool_choice".into(), json!("auto"));
            }
            ToolChoice::Required => {
                body.insert("tool_choice".into(), json!("required"));
            }
            ToolChoice::None => {
                body.insert("tool_choice".into(), json!("none"));
            }
            ToolChoice::Tool(name) => {
                body.insert(
                    "tool_choice".into(),
                    json!({"type": "function", "function": {"name": name}}),
                );
            }
        }
        if let Some(p) = ir.parallel_tool_calls {
            body.insert("parallel_tool_calls".into(), json!(p));
        }
        if let Some(u) = &ir.user {
            body.insert("user".into(), json!(u));
        }

        restore_extensions(&ir.extensions, &mut body);
        Ok(Value::Object(body))
    }
}

/// 渲染一条 Chat `tool` 消息，把结果里 Chat 无法表达的媒体单独搬运。
///
/// Chat 的 tool 消息 content 只接受字符串或 text part 数组。Anthropic 的
/// 工具结果可以带图片 —— 直接编进 tool 消息会被上游 400，丢弃又损失了
/// 模型需要看到的信息。折中：文本进 tool 消息，媒体紧随其后用一条 user
/// 消息补发（附说明，让模型知道它属于哪个工具结果）。
fn encode_tool_result(id: &str, content: &[ContentPart], is_error: bool, out: &mut Vec<Value>) {
    if is_error {
        tracing::debug!(%id, "chat: 工具错误标记无对等字段，已丢弃");
    }
    let (texts, media): (Vec<&ContentPart>, Vec<&ContentPart>) = content
        .iter()
        .partition(|p| matches!(p, ContentPart::Text { .. }));
    out.push(json!({
        "role": "tool",
        "tool_call_id": id,
        "content": content_json(&texts),
    }));
    let media: Vec<&ContentPart> = media
        .into_iter()
        .filter(|p| {
            matches!(
                p,
                ContentPart::Image { .. } | ContentPart::Audio { .. } | ContentPart::File { .. }
            )
        })
        .collect();
    if !media.is_empty() {
        let mut parts = vec![json!({
            "type": "text",
            "text": format!("[attachment from tool call {id}]"),
        })];
        parts.extend(media.into_iter().filter_map(part_json));
        out.push(json!({"role": "user", "content": Value::Array(parts)}));
    }
}

/// 把一条 IR 消息渲染成一条或多条 Chat 消息。
///
/// 编码产物是**发给上游的请求**，必须是干净的 wire 格式：推理块在 Chat
/// 请求里没有任何标准表达（DeepSeek 官方甚至要求把历史里的
/// `reasoning_content` 剔除，否则 400），所以这里静默丢弃 —— 这与
/// Anthropic「非最后回合的 thinking 自动忽略」的语义一致。
fn encode_message(msg: &Message, out: &mut Vec<Value>) {
    // 一条 Chat tool 消息只能带一个 tool_call_id，所以要按结果拆开。
    if msg.role == Role::Tool {
        let mut emitted = 0usize;
        for part in &msg.content {
            if let ContentPart::ToolResult {
                id,
                content,
                is_error,
                ..
            } = part
            {
                encode_tool_result(id, content, *is_error, out);
                emitted += 1;
            }
        }
        if emitted == 0 && !msg.content.is_empty() {
            // Tool 角色但没有 ToolResult（别的协议转过来可能这样）。没有
            // tool_call_id 就发不了 tool 消息，退化成 user 消息保住内容。
            tracing::debug!("chat: tool 消息无 ToolResult，降级为 user 消息");
            let refs: Vec<&ContentPart> = msg.content.iter().collect();
            out.push(json!({"role": "user", "content": content_json(&refs)}));
        }
        return;
    }

    let role = match msg.role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant | Role::Tool => "assistant",
    };

    // Anthropic/Gemini 的 user 回合可以同时带工具结果和新输入（混合回合）。
    // Chat 里工具结果必须是独立的 tool 消息，且要排在后续对话内容之前 ——
    // 它逻辑上属于上一个 assistant 回合的调用。先拆出去。
    for part in &msg.content {
        if let ContentPart::ToolResult {
            id,
            content,
            is_error,
            ..
        } = part
        {
            if msg.role == Role::User {
                encode_tool_result(id, content, *is_error, out);
            } else {
                tracing::debug!("chat: assistant 消息上的工具结果不合语义，已丢弃");
            }
        }
    }

    let mut visible: Vec<&ContentPart> = Vec::new();
    let mut tool_calls = Vec::new();
    let mut refusal: Option<String> = None;

    for part in &msg.content {
        match part {
            ContentPart::ToolUse {
                id, name, input, ..
            } => {
                tool_calls.push(json!({
                    "id": id,
                    "type": "function",
                    "function": {
                        "name": name,
                        // Chat 要求 arguments 是 JSON **字符串**而非对象。
                        "arguments": match input {
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        },
                    },
                }));
            }
            ContentPart::Refusal { text } => {
                refusal.get_or_insert_with(String::new).push_str(text);
            }
            ContentPart::Thinking { .. } | ContentPart::RedactedThinking { .. } => {
                // 请求体里没有推理块的合法位置。Anthropic 语义：过往回合的
                // thinking 本来就会被上游忽略；真正需要回传 thinking 的
                // messages→messages 场景走直通，不经过这里。
                tracing::debug!("chat: 请求方向丢弃推理块（无标准表达）");
            }
            // 已在上面拆成独立 tool 消息。
            ContentPart::ToolResult { .. } => {}
            other => visible.push(other),
        }
    }

    // 拆完工具结果后空空如也的消息不输出（纯 tool_result 的混合回合）。
    // 但原本就是空内容的消息要保留占位 —— 丢整条消息会改变对话轮次。
    if visible.is_empty() && tool_calls.is_empty() && refusal.is_none() && !msg.content.is_empty() {
        return;
    }

    let mut m = Map::new();
    m.insert("role".into(), json!(role));
    // 只有工具调用/拒答时 content 必须存在但可以为 null。
    if visible.is_empty() && (!tool_calls.is_empty() || refusal.is_some()) {
        m.insert("content".into(), Value::Null);
    } else {
        m.insert("content".into(), content_json(&visible));
    }
    if let Some(r) = refusal {
        m.insert("refusal".into(), json!(r));
    }
    if !tool_calls.is_empty() {
        m.insert("tool_calls".into(), Value::Array(tool_calls));
    }
    if let Some(n) = &msg.name {
        m.insert("name".into(), json!(n));
    }
    out.push(Value::Object(m));
}

// ---------------------------------------------------------------------------
// 响应
// ---------------------------------------------------------------------------

impl ResponseCodec for ChatCodec {
    fn decode_response(&self, raw: &Value) -> Result<UnifiedResponse, GatewayError> {
        if let Some(err) = raw.get("error").filter(|v| !v.is_null()) {
            return Err(error_from_body(err));
        }
        let obj = raw
            .as_object()
            .ok_or_else(|| GatewayError::invalid_request("response body must be a JSON object"))?;

        let mut resp = UnifiedResponse::new(
            obj.get("id").and_then(Value::as_str).unwrap_or_default(),
            obj.get("model").and_then(Value::as_str).unwrap_or_default(),
        );
        if let Some(created) = obj.get("created").and_then(Value::as_i64) {
            resp.created = created;
        }

        let choices = obj
            .get("choices")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        if choices.len() > 1 {
            // IR 只有一路 content。n>1 极少用，多出来的候选就地丢弃。
            tracing::debug!(count = choices.len(), "chat: 只保留第一个 choice");
        }

        if let Some(choice) = choices.first() {
            let message = choice.get("message");
            let dropped = obj.get("dropped_thinking").and_then(Value::as_array);
            let mut parts = Vec::new();

            // dropped_thinking 带 signature，比 reasoning_content 更完整，
            // 两者都在时以前者为准（后面按 position 插回）。
            if dropped.is_none()
                && let Some(t) = message
                    .and_then(|m| m.get("reasoning_content"))
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
            {
                parts.push(ContentPart::Thinking {
                    text: t.to_owned(),
                    signature: message
                        .and_then(|m| m.get("reasoning_signature"))
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                });
            }
            parts.extend(parse_content(message.and_then(|m| m.get("content"))));
            if let Some(r) = message
                .and_then(|m| m.get("refusal"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                parts.push(ContentPart::Refusal { text: r.to_owned() });
            }
            if let Some(calls) = message
                .and_then(|m| m.get("tool_calls"))
                .and_then(Value::as_array)
            {
                parse_tool_calls(calls, &mut parts);
            }
            if let Some(items) = dropped {
                let refs: Vec<&Value> = items.iter().collect();
                restore_dropped_parts(&refs, &mut parts);
            }

            resp.content = parts;
            resp.stop_reason = choice
                .get("finish_reason")
                .and_then(Value::as_str)
                .map(finish_reason_to_ir);
        }

        if let Some(u) = obj.get("usage").filter(|v| v.is_object()) {
            resp.usage = parse_usage(u);
        }

        collect_unknown(obj, KNOWN_RESPONSE_FIELDS, &mut resp.extensions);
        Ok(resp)
    }

    fn encode_response(&self, ir: &UnifiedResponse) -> Result<Value, GatewayError> {
        let mut visible: Vec<&ContentPart> = Vec::new();
        let mut tool_calls = Vec::new();
        let mut refusal: Option<String> = None;
        let mut reasoning = String::new();
        let mut dropped = Vec::new();

        for (pi, part) in ir.content.iter().enumerate() {
            match part {
                ContentPart::ToolUse {
                    id, name, input, ..
                } => {
                    tool_calls.push(json!({
                        "id": id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": match input {
                                Value::String(s) => s.clone(),
                                other => other.to_string(),
                            },
                        },
                    }));
                }
                ContentPart::Refusal { text } => {
                    refusal.get_or_insert_with(String::new).push_str(text);
                }
                ContentPart::Thinking { text, signature } => {
                    reasoning.push_str(text);
                    let mut record = json!({"position": pi, "text": text});
                    if let Some(sig) = signature {
                        record["signature"] = json!(sig);
                    }
                    dropped.push(record);
                }
                ContentPart::RedactedThinking { data } => {
                    dropped.push(json!({"position": pi, "redacted": data}));
                }
                ContentPart::ToolResult { .. } => {
                    tracing::debug!("chat: 响应里的工具结果无对等表达，已丢弃");
                }
                other => visible.push(other),
            }
        }

        let mut message = Map::new();
        message.insert("role".into(), json!("assistant"));
        if visible.is_empty() && (!tool_calls.is_empty() || refusal.is_some()) {
            message.insert("content".into(), Value::Null);
        } else {
            message.insert("content".into(), content_json(&visible));
        }
        if !reasoning.is_empty() {
            message.insert("reasoning_content".into(), json!(reasoning));
        }
        if let Some(r) = refusal {
            message.insert("refusal".into(), json!(r));
        }
        if !tool_calls.is_empty() {
            message.insert("tool_calls".into(), Value::Array(tool_calls));
        }

        if ir.stop_sequence.is_some() {
            tracing::debug!("chat: 响应无 stop_sequence 字段，已丢弃");
        }

        let mut body = Map::new();
        body.insert("id".into(), json!(ir.id));
        body.insert("object".into(), json!("chat.completion"));
        body.insert("created".into(), json!(ir.created));
        body.insert("model".into(), json!(ir.model));
        body.insert(
            "choices".into(),
            json!([{
                "index": 0,
                "message": Value::Object(message),
                "finish_reason": ir.stop_reason.map(stop_reason_to_finish),
            }]),
        );
        body.insert("usage".into(), usage_json(&ir.usage));
        if !dropped.is_empty() {
            body.insert("dropped_thinking".into(), Value::Array(dropped));
        }

        restore_extensions(&ir.extensions, &mut body);
        Ok(Value::Object(body))
    }
}

// ---------------------------------------------------------------------------
// 流式
// ---------------------------------------------------------------------------

impl StreamCodec for ChatCodec {
    fn stream_decoder(&self) -> Box<dyn StreamDecoder> {
        Box::new(ChatStreamDecoder::new())
    }

    fn stream_encoder(&self) -> Box<dyn StreamEncoder> {
        Box::new(ChatStreamEncoder::new())
    }
}

impl ProtocolCodec for ChatCodec {
    fn protocol(&self) -> Protocol {
        Protocol::Chat
    }
}

/// 一个工具调用槽的累积状态。
#[derive(Default)]
struct ToolSlot {
    /// 是否见过该工具的任何帧。
    seen: bool,
    /// 是否已产出 [`StreamEvent::ToolCallStart`]。
    declared: bool,
    /// 累积的调用 id（取最后一个非空值）。
    id: String,
    /// 累积的函数名：少数中转站把 name 切成多帧发送，必须拼回完整名。
    name: String,
    /// 名字就位前先到达的 arguments 片段，随声明一并补发。
    pending_args: Vec<String>,
}

/// 流式解码器（有状态）。
///
/// Chat 的 chunk 不带事件名，工具调用靠 `tool_calls[].index` 累积，所以必须
/// 记住哪些 tool index 已经开过头。
///
/// 刻意**不产出** [`StreamEvent::ContentStart`]：Chat 的线格式里根本没有这个
/// 概念，硬造一个只会在文本/推理共用下标 0 时制造歧义。下游编码器本来就要能
/// 从第一个 delta 推断块的开始。
struct ChatStreamDecoder {
    /// 是否已产出 [`StreamEvent::Start`]。
    started: bool,
    /// 是否已产出 [`StreamEvent::Done`]。
    done: bool,
    /// 按 OpenAI tool index 排列的工具槽。
    tools: Vec<ToolSlot>,
}

impl ChatStreamDecoder {
    fn new() -> Self {
        Self {
            started: false,
            done: false,
            tools: Vec::new(),
        }
    }

    fn slot(&mut self, ti: usize) -> &mut ToolSlot {
        while self.tools.len() <= ti {
            self.tools.push(ToolSlot::default());
        }
        &mut self.tools[ti]
    }

    /// 为出现过但从未声明过的工具槽补发 [`StreamEvent::ToolCallStart`]。
    ///
    /// 无参调用（`arguments` 始终为空）不会在 decode 阶段触发声明，必须在流
    /// 终结前补上，否则下游永远看不到这条工具调用。
    fn flush_undeclared_tools(&mut self, out: &mut Vec<StreamEvent>) {
        for (ti, slot) in self.tools.iter_mut().enumerate() {
            if slot.seen && !slot.declared {
                slot.declared = true;
                let index = TOOL_INDEX_BASE + ti as u32;
                out.push(StreamEvent::ToolCallStart {
                    index,
                    id: slot.id.clone(),
                    name: slot.name.clone(),
                    signature: None,
                });
                // 畸形流里参数可能先于名字（甚至没有名字）到达，一并补发。
                for frag in std::mem::take(&mut slot.pending_args) {
                    out.push(StreamEvent::ToolCallArgsDelta {
                        index,
                        fragment: frag,
                    });
                }
            }
        }
    }
}

/// 取 delta 里某个字段的文本增量。
///
/// 同一个字段在不同中转站上可能是字符串、`{content: "..."}` 或 part 数组，
/// 三种都认 —— 报错换不来正确的流，只会让用户看到半截回答。
fn delta_text(delta: &Value, key: &str) -> Option<String> {
    match delta.get(key)? {
        Value::String(s) => Some(s.clone()),
        Value::Object(o) => o
            .get("content")
            .and_then(Value::as_str)
            .or_else(|| o.get("text").and_then(Value::as_str))
            .map(str::to_owned),
        Value::Array(a) => {
            let mut joined = String::new();
            for item in a {
                if let Some(t) = item.as_str() {
                    joined.push_str(t);
                } else if let Some(t) = item.get("text").and_then(Value::as_str) {
                    joined.push_str(t);
                }
            }
            Some(joined)
        }
        _ => None,
    }
}

impl StreamDecoder for ChatStreamDecoder {
    fn decode(&mut self, frame: &SseFrame) -> Result<Vec<StreamEvent>, GatewayError> {
        let data = frame.data.trim();
        if data.is_empty() {
            return Ok(Vec::new());
        }
        if data == DONE_SENTINEL {
            self.done = true;
            let mut out = Vec::new();
            self.flush_undeclared_tools(&mut out);
            out.push(StreamEvent::Done);
            return Ok(out);
        }

        let chunk: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(e) => {
                // 单帧解析失败不该杀掉整条流 —— 中转站偶尔插入非 JSON 的保活内容。
                tracing::debug!(error = %e, "chat: 跳过无法解析的流帧");
                return Ok(Vec::new());
            }
        };

        let mut out = Vec::new();

        if let Some(err) = chunk.get("error").filter(|v| !v.is_null()) {
            out.push(StreamEvent::Error {
                message: err
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("upstream error")
                    .to_owned(),
                kind: err
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("upstream_error")
                    .to_owned(),
            });
            return Ok(out);
        }

        if !self.started {
            self.started = true;
            out.push(StreamEvent::Start {
                id: chunk
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                model: chunk
                    .get("model")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                // Chat 的 usage 只在最后一帧出现。
                usage: None,
            });
        }

        if let Some(choice) = chunk
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
        {
            if let Some(delta) = choice.get("delta") {
                // reasoning_content（DeepSeek 系）与 reasoning（另一派中转站）
                // 都是非标准字段，两个都认。
                if let Some(t) = delta_text(delta, "reasoning_content")
                    .or_else(|| delta_text(delta, "reasoning"))
                    && !t.is_empty()
                {
                    out.push(StreamEvent::ThinkingDelta {
                        index: TEXT_INDEX,
                        text: t,
                    });
                }
                // 本 codec 编码器自己产出的非标准字段，用于 chat↔chat 不丢签名。
                if let Some(sig) = delta
                    .get("reasoning_signature")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    out.push(StreamEvent::ThinkingSignature {
                        index: TEXT_INDEX,
                        signature: sig.to_owned(),
                    });
                }
                if let Some(t) = delta_text(delta, "content")
                    && !t.is_empty()
                {
                    out.push(StreamEvent::TextDelta {
                        index: TEXT_INDEX,
                        text: t,
                    });
                }
                if let Some(t) = delta_text(delta, "refusal")
                    && !t.is_empty()
                {
                    out.push(StreamEvent::RefusalDelta {
                        index: TEXT_INDEX,
                        text: t,
                    });
                }
                if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                    for call in calls {
                        let ti = call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                        let ir_index = TOOL_INDEX_BASE + ti as u32;
                        let function = call.get("function");
                        let args = function
                            .and_then(|f| f.get("arguments"))
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned();

                        // id/name/arguments 都可能被中转站拆帧送达。按槽累积
                        // id 与 name，每个槽只声明一次 ToolCallStart：优先在
                        // 首个非空 arguments 且 name 已就位时声明（正常顺序）；
                        // 参数先于名字到达的畸形流先暂存参数，name 到位再连同
                        // Start 一起补发；流终结时 flush 兜底。
                        let mut pending = Vec::new();
                        let emit = {
                            let slot = self.slot(ti);
                            slot.seen = true;
                            if let Some(id) = call
                                .get("id")
                                .and_then(Value::as_str)
                                .filter(|s| !s.is_empty())
                            {
                                slot.id = id.to_owned();
                            }
                            if let Some(name) = function
                                .and_then(|f| f.get("name"))
                                .and_then(Value::as_str)
                                .filter(|s| !s.is_empty())
                            {
                                slot.name.push_str(name);
                            }
                            if !slot.declared && !args.is_empty() {
                                if slot.name.is_empty() {
                                    // 名字还没到：暂存，等声明时补发。
                                    slot.pending_args.push(args.clone());
                                    false
                                } else {
                                    slot.declared = true;
                                    pending = std::mem::take(&mut slot.pending_args);
                                    true
                                }
                            } else {
                                false
                            }
                        };
                        if emit {
                            let slot = self.slot(ti);
                            out.push(StreamEvent::ToolCallStart {
                                index: ir_index,
                                id: slot.id.clone(),
                                name: slot.name.clone(),
                                signature: None,
                            });
                            for frag in pending {
                                out.push(StreamEvent::ToolCallArgsDelta {
                                    index: ir_index,
                                    fragment: frag,
                                });
                            }
                        }
                        if !args.is_empty() && self.tools[ti].declared {
                            // 已声明才直接发；未声明的要么进了 pending_args，
                            // 要么留待 flush，重复发会乱序。
                            out.push(StreamEvent::ToolCallArgsDelta {
                                index: ir_index,
                                fragment: args,
                            });
                        }
                    }
                }
            }

            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                // 未声明的工具槽必须先于 Stop 补声明，否则下游见不到它们。
                self.flush_undeclared_tools(&mut out);
                out.push(StreamEvent::Stop {
                    reason: finish_reason_to_ir(reason),
                    // Chat 不回报命中了哪条停止序列。
                    stop_sequence: None,
                });
            }
        }

        if let Some(u) = chunk.get("usage").filter(|v| v.is_object()) {
            out.push(StreamEvent::Usage(parse_usage(u)));
        }

        Ok(out)
    }

    fn finish(&mut self) -> Result<Vec<StreamEvent>, GatewayError> {
        if self.done {
            return Ok(Vec::new());
        }
        // 上游断流没发 [DONE] 时也要给下游一个终结事件，否则编码器不会收尾。
        self.done = true;
        let mut out = Vec::new();
        self.flush_undeclared_tools(&mut out);
        out.push(StreamEvent::Done);
        Ok(out)
    }
}

/// 流式编码器（有状态）。
///
/// 客户端只看得到这里产出的帧，所以 Chat 要求的开场帧、finish_reason 帧和
/// `[DONE]` 全部由编码器自己补齐，不指望上游事件是完整的。
struct ChatStreamEncoder {
    id: String,
    model: String,
    created: i64,
    /// 是否已发出带 `role: assistant` 的开场帧。
    opened: bool,
    /// 是否已发出带 finish_reason 的收尾帧。
    finished: bool,
    /// 是否已发出 `[DONE]`。
    done: bool,
    /// 是否已发出 usage 帧。
    usage_sent: bool,
    usage: Usage,
    /// 按到达顺序把 IR 块下标映射到 Chat 的 tool index。
    ///
    /// 不用「IR index 减 1」是因为上游解码器可能来自任何协议 —— Anthropic 的
    /// 工具块下标取决于前面有几个文本/推理块，减法会串号。按到达顺序分配对
    /// 本 codec 自己的解码器输出（1,2,3…）退化成完全一致的结果。
    tool_slots: Vec<u32>,
}

impl ChatStreamEncoder {
    fn new() -> Self {
        Self {
            // 上游没给 id 时也得有一个，OpenAI SDK 会读它。
            id: format!("chatcmpl-{}", uuid::Uuid::new_v4().simple()),
            model: String::new(),
            created: chrono::Utc::now().timestamp(),
            opened: false,
            finished: false,
            done: false,
            usage_sent: false,
            usage: Usage::default(),
            tool_slots: Vec::new(),
        }
    }

    /// 组装一个 chunk 帧。
    fn chunk(&self, delta: Value, finish: Option<&str>) -> SseFrame {
        SseFrame::data(
            json!({
                "id": self.id,
                "object": "chat.completion.chunk",
                "created": self.created,
                "model": self.model,
                "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
            })
            .to_string(),
        )
    }

    /// 补出开场帧。Chat 客户端普遍假定首帧带 `role`。
    fn open(&mut self, out: &mut Vec<SseFrame>) {
        if self.opened {
            return;
        }
        self.opened = true;
        out.push(self.chunk(json!({"role": "assistant", "content": ""}), None));
    }

    /// usage 单独一帧，`choices` 为空数组 —— 这是 Chat 的既定形态。
    fn usage_frame(&self) -> SseFrame {
        SseFrame::data(
            json!({
                "id": self.id,
                "object": "chat.completion.chunk",
                "created": self.created,
                "model": self.model,
                "choices": [],
                "usage": usage_json(&self.usage),
            })
            .to_string(),
        )
    }

    fn tool_slot(&mut self, ir_index: u32) -> usize {
        if let Some(pos) = self.tool_slots.iter().position(|&i| i == ir_index) {
            return pos;
        }
        self.tool_slots.push(ir_index);
        self.tool_slots.len() - 1
    }

    /// 取每个字段的最大值合并 usage。
    ///
    /// 与 [`StreamAggregator`] 同样的理由：Anthropic 在 message_start 给
    /// input、在 message_delta 给累积的 output，累加会重复计数。
    fn merge_usage(&mut self, u: &Usage) {
        self.usage.merge_max(u);
    }

    /// 补齐收尾：finish_reason 帧、usage 帧、`[DONE]`。
    ///
    /// 少了 `[DONE]` 的流会让 OpenAI SDK 一直挂着等下一帧，所以它必须发出去，
    /// 哪怕上游是断的。
    fn wrap_up(&mut self) -> Vec<SseFrame> {
        if self.done {
            return Vec::new();
        }
        self.done = true;
        let mut out = Vec::new();
        if self.opened && !self.finished {
            self.finished = true;
            out.push(self.chunk(json!({}), Some("stop")));
        }
        if !self.usage_sent && !self.usage.is_empty() {
            self.usage_sent = true;
            out.push(self.usage_frame());
        }
        out.push(SseFrame::data(DONE_SENTINEL));
        out
    }
}

impl StreamEncoder for ChatStreamEncoder {
    fn encode(&mut self, event: &StreamEvent) -> Result<Vec<SseFrame>, GatewayError> {
        let mut out = Vec::new();
        match event {
            StreamEvent::Start { id, model, usage } => {
                if !id.is_empty() {
                    self.id = id.clone();
                }
                if !model.is_empty() {
                    self.model = model.clone();
                }
                // Anthropic 在开场就给 input_tokens，留着并进最终 usage 帧。
                if let Some(u) = usage {
                    self.merge_usage(u);
                }
                self.open(&mut out);
            }
            StreamEvent::TextDelta { text, .. } => {
                self.open(&mut out);
                out.push(self.chunk(json!({"content": text}), None));
            }
            StreamEvent::ThinkingDelta { text, .. } => {
                self.open(&mut out);
                out.push(self.chunk(json!({"reasoning_content": text}), None));
            }
            StreamEvent::ThinkingSignature { signature, .. } => {
                // Chat 没有 signature 字段。用非标准字段透出去，本 codec 的
                // 解码器认得它，chat→chat 中转不会丢签名。
                self.open(&mut out);
                out.push(self.chunk(json!({"reasoning_signature": signature}), None));
            }
            StreamEvent::RefusalDelta { text, .. } => {
                self.open(&mut out);
                out.push(self.chunk(json!({"refusal": text}), None));
            }
            StreamEvent::ToolCallStart {
                index, id, name, ..
            } => {
                self.open(&mut out);
                let ti = self.tool_slot(*index);
                out.push(self.chunk(
                    json!({"tool_calls": [{
                        "index": ti,
                        "id": id,
                        "type": "function",
                        "function": {"name": name, "arguments": ""},
                    }]}),
                    None,
                ));
            }
            StreamEvent::ToolCallArgsDelta { index, fragment } => {
                self.open(&mut out);
                let ti = self.tool_slot(*index);
                out.push(self.chunk(
                    json!({"tool_calls": [{
                        "index": ti,
                        "function": {"arguments": fragment},
                    }]}),
                    None,
                ));
            }
            StreamEvent::Usage(u) => {
                // 只攒不发。OpenAI 规范的 usage 帧位置是 finish_reason 帧
                // **之后**、`[DONE]` 之前；而 Anthropic 上游的 usage
                // （message_delta）先于终止事件到达，立即发帧会顺序颠倒，
                // 还会把多次 usage 更新发成多帧。统一由 wrap_up 收尾时发。
                self.merge_usage(u);
            }
            StreamEvent::Stop { reason, .. } => {
                self.open(&mut out);
                self.finished = true;
                out.push(self.chunk(json!({}), Some(stop_reason_to_finish(*reason))));
            }
            StreamEvent::Done => out.extend(self.wrap_up()),
            StreamEvent::Error { message, kind } => {
                out.push(SseFrame::data(
                    json!({"error": {"message": message, "type": kind}}).to_string(),
                ));
                if !self.done {
                    self.done = true;
                    out.push(SseFrame::data(DONE_SENTINEL));
                }
            }
            // Chat 线格式里没有这些仪式，静默吸收。
            StreamEvent::ContentStart { .. }
            | StreamEvent::ContentStop { .. }
            | StreamEvent::Ping => {}
        }
        Ok(out)
    }

    fn finish(&mut self) -> Result<Vec<SseFrame>, GatewayError> {
        Ok(self.wrap_up())
    }
}

#[cfg(test)]
#[path = "chat_tests.rs"]
mod tests;
