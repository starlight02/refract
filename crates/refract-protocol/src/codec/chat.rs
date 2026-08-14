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
    /// 是否已产出过 [`StreamEvent::ToolCallStart`]。
    started: bool,
    /// 是否已经拿到过非空的工具名。
    named: bool,
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
            return Ok(vec![StreamEvent::Done]);
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

                        // 首帧开头；少数中转站把 name 拖到第二帧才发，那时补一条
                        // 让下游能对齐 —— 但不重复补，否则下游会开两个块。
                        let (first, needs_name) = {
                            let slot = self.slot(ti);
                            let first = !slot.started;
                            let needs_name = !slot.named && !name.is_empty();
                            slot.started = true;
                            slot.named |= !name.is_empty();
                            (first, needs_name)
                        };
                        if first || needs_name {
                            out.push(StreamEvent::ToolCallStart {
                                index: ir_index,
                                id,
                                name,
                                signature: None,
                            });
                        }

                        if let Some(args) = function
                            .and_then(|f| f.get("arguments"))
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                        {
                            out.push(StreamEvent::ToolCallArgsDelta {
                                index: ir_index,
                                fragment: args.to_owned(),
                            });
                        }
                    }
                }
            }

            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
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
        Ok(vec![StreamEvent::Done])
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
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    /// 解码一个请求，失败即 panic。
    fn dec_req(raw: Value) -> UnifiedRequest {
        CHAT.decode_request(&raw).expect("decode_request 应该成功")
    }

    /// 编码一个请求。
    fn enc_req(ir: &UnifiedRequest) -> Value {
        CHAT.encode_request(ir).expect("encode_request 应该成功")
    }

    /// 把一串 chunk 喂给解码器，收集全部事件。
    fn decode_stream(chunks: &[&str]) -> Vec<StreamEvent> {
        let mut d = ChatStreamDecoder::new();
        let mut out = Vec::new();
        for c in chunks {
            out.extend(d.decode(&SseFrame::data(*c)).expect("解码不应失败"));
        }
        out.extend(d.finish().expect("finish 不应失败"));
        out
    }

    /// 把一串事件喂给编码器，收集帧里的 data。
    fn encode_stream(events: &[StreamEvent]) -> Vec<String> {
        let mut e = ChatStreamEncoder::new();
        let mut out = Vec::new();
        for ev in events {
            for f in e.encode(ev).expect("编码不应失败") {
                out.push(f.data);
            }
        }
        for f in e.finish().expect("finish 不应失败") {
            out.push(f.data);
        }
        out
    }

    // -----------------------------------------------------------------
    // 请求
    // -----------------------------------------------------------------

    #[test]
    fn basic_request_round_trips_through_ir() {
        let raw = json!({
            "model": "gpt-5",
            "messages": [{"role": "user", "content": "你好"}],
            "temperature": 0.7,
            "top_p": 0.9,
            "frequency_penalty": 0.1,
            "presence_penalty": 0.2,
            "seed": 42,
            "n": 2,
            "max_completion_tokens": 512,
            "user": "u-1",
        });
        let ir = dec_req(raw);
        assert_eq!(ir.model, "gpt-5");
        assert_eq!(ir.messages, vec![Message::text(Role::User, "你好")]);
        assert_eq!(ir.sampling.temperature, Some(0.7));
        assert_eq!(ir.sampling.top_p, Some(0.9));
        assert_eq!(ir.sampling.frequency_penalty, Some(0.1));
        assert_eq!(ir.sampling.presence_penalty, Some(0.2));
        assert_eq!(ir.sampling.seed, Some(42));
        assert_eq!(ir.sampling.candidate_count, Some(2));
        assert_eq!(ir.max_output_tokens, Some(512));
        assert_eq!(ir.user.as_deref(), Some("u-1"));
        assert!(!ir.stream);

        let back = enc_req(&ir);
        assert_eq!(back["model"], json!("gpt-5"));
        // 纯文本消息编码回字符串形态，而非 part 数组。
        assert_eq!(
            back["messages"],
            json!([{"role": "user", "content": "你好"}])
        );
        assert_eq!(back["max_completion_tokens"], json!(512));
        assert_eq!(back["seed"], json!(42));
        // 已废弃的 max_tokens 不应再出现在输出里。
        assert_eq!(back.get("max_tokens"), None);
    }

    #[test]
    fn missing_model_and_messages_are_rejected_with_usable_messages() {
        let err = CHAT
            .decode_request(&json!({"messages": []}))
            .expect_err("缺 model 应该报错");
        assert_eq!(err.kind, ErrorKind::InvalidRequest);
        assert!(
            err.message.contains("model"),
            "消息应指明字段: {}",
            err.message
        );

        let err = CHAT
            .decode_request(&json!({"model": "gpt-5"}))
            .expect_err("缺 messages 应该报错");
        assert!(
            err.message.contains("messages"),
            "消息应指明字段: {}",
            err.message
        );

        let err = CHAT
            .decode_request(&json!("not an object"))
            .expect_err("非对象请求体应该报错");
        assert_eq!(err.kind, ErrorKind::InvalidRequest);
    }

    #[test]
    fn max_completion_tokens_wins_over_deprecated_max_tokens() {
        let ir = dec_req(json!({
            "model": "m",
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 100,
            "max_completion_tokens": 200,
        }));
        assert_eq!(ir.max_output_tokens, Some(200));

        // 只有旧字段时仍要读到。
        let legacy = dec_req(json!({
            "model": "m",
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 100,
        }));
        assert_eq!(legacy.max_output_tokens, Some(100));
    }

    #[test]
    fn system_and_developer_roles_are_lifted_out_of_messages() {
        let ir = dec_req(json!({
            "model": "m",
            "messages": [
                {"role": "system", "content": "be terse"},
                {"role": "developer", "content": "be correct"},
                {"role": "user", "content": "hi"},
            ],
        }));
        assert_eq!(ir.system_text(), "be terse\nbe correct");
        // system/developer 不该留在 messages 里。
        assert_eq!(ir.messages.len(), 1);
        assert_eq!(ir.messages[0].role, Role::User);

        // 回写时 system 必须回到消息数组首位。
        let back = enc_req(&ir);
        let msgs = back["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], json!("system"));
        // 两条独立的 system/developer 消息合并时要换行，不能把词粘在一起。
        assert_eq!(msgs[0]["content"], json!("be terse\nbe correct"));
        assert_eq!(msgs[1]["role"], json!("user"));
    }

    #[test]
    fn stop_accepts_both_string_and_array() {
        let one = dec_req(json!({
            "model": "m", "messages": [{"role": "user", "content": "x"}], "stop": "END",
        }));
        assert_eq!(one.sampling.stop, vec!["END".to_owned()]);
        // 单个停止序列回写成字符串。
        assert_eq!(enc_req(&one)["stop"], json!("END"));

        let many = dec_req(json!({
            "model": "m", "messages": [{"role": "user", "content": "x"}], "stop": ["A", "B"],
        }));
        assert_eq!(many.sampling.stop, vec!["A".to_owned(), "B".to_owned()]);
        assert_eq!(enc_req(&many)["stop"], json!(["A", "B"]));
    }

    #[test]
    fn multimodal_parts_survive_the_round_trip() {
        let ir = dec_req(json!({
            "model": "m",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "看图"},
                {"type": "image_url", "image_url": {
                    "url": "data:image/png;base64,iVBORw0KGgo=", "detail": "high"}},
                {"type": "image_url", "image_url": {"url": "https://x/y.png"}},
                {"type": "input_audio", "input_audio": {"data": "QUJD", "format": "mp3"}},
                {"type": "file", "file": {"file_id": "file-123", "filename": "a.pdf"}},
            ]}],
        }));
        assert_eq!(
            ir.messages[0].content,
            vec![
                ContentPart::text("看图"),
                ContentPart::Image {
                    source: MediaSource::Base64("iVBORw0KGgo=".into()),
                    mime: Some("image/png".into()),
                    detail: Some("high".into()),
                },
                ContentPart::Image {
                    source: MediaSource::Url("https://x/y.png".into()),
                    mime: None,
                    detail: None,
                },
                ContentPart::Audio {
                    source: MediaSource::Base64("QUJD".into()),
                    format: Some("mp3".into()),
                },
                ContentPart::File {
                    source: MediaSource::FileId("file-123".into()),
                    mime: None,
                    name: Some("a.pdf".into()),
                },
            ]
        );

        // 回写后 base64 图片要重新拼成 data URI，音频保持裸 base64。
        let back = enc_req(&ir);
        let parts = back["messages"][0]["content"].as_array().unwrap();
        assert_eq!(
            parts[1]["image_url"]["url"],
            json!("data:image/png;base64,iVBORw0KGgo=")
        );
        assert_eq!(parts[1]["image_url"]["detail"], json!("high"));
        assert_eq!(
            parts[3]["input_audio"],
            json!({"data": "QUJD", "format": "mp3"})
        );
        assert_eq!(
            parts[4]["file"],
            json!({"file_id": "file-123", "filename": "a.pdf"})
        );

        // 再解一次必须完全一致 —— 这才叫无损。
        assert_eq!(dec_req(back).messages[0].content, ir.messages[0].content);
    }

    #[test]
    fn tool_call_three_phase_flow_round_trips() {
        // 三段式：声明工具 → 模型发起调用 → 结果回传。
        let raw = json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "北京天气？"},
                {"role": "assistant", "content": null, "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "get_weather", "arguments": "{\"city\":\"北京\"}"},
                }]},
                {"role": "tool", "tool_call_id": "call_1", "content": "晴，25 度"},
            ],
            "tools": [{"type": "function", "function": {
                "name": "get_weather",
                "description": "查天气",
                "parameters": {"type": "object", "properties": {"city": {"type": "string"}}},
                "strict": true,
            }}],
            "tool_choice": {"type": "function", "function": {"name": "get_weather"}},
            "parallel_tool_calls": false,
        });
        let ir = dec_req(raw);

        assert_eq!(ir.tools.len(), 1);
        assert_eq!(ir.tools[0].name, "get_weather");
        assert_eq!(ir.tools[0].description.as_deref(), Some("查天气"));
        assert_eq!(ir.tools[0].strict, Some(true));
        assert_eq!(ir.tool_choice, ToolChoice::Tool("get_weather".into()));
        assert_eq!(ir.parallel_tool_calls, Some(false));

        // arguments 是 JSON 字符串，必须被解析成结构化 Value。
        assert_eq!(
            ir.messages[1].content,
            vec![ContentPart::ToolUse {
                signature: None,
                id: "call_1".into(),
                name: "get_weather".into(),
                input: json!({"city": "北京"}),
            }]
        );
        assert_eq!(ir.messages[2].role, Role::Tool);
        assert_eq!(
            ir.messages[2].content,
            vec![ContentPart::ToolResult {
                name: None,
                id: "call_1".into(),
                content: vec![ContentPart::text("晴，25 度")],
                is_error: false,
            }]
        );

        let back = enc_req(&ir);
        let msgs = back["messages"].as_array().unwrap();
        // 没有可见内容的 assistant 消息 content 应为 null。
        assert_eq!(msgs[1]["content"], Value::Null);
        // arguments 必须重新序列化回字符串，不能是对象。
        assert_eq!(
            msgs[1]["tool_calls"][0]["function"]["arguments"],
            json!("{\"city\":\"北京\"}")
        );
        assert_eq!(
            msgs[2],
            json!({
                "role": "tool", "tool_call_id": "call_1", "content": "晴，25 度",
            })
        );
        assert_eq!(
            back["tool_choice"],
            json!({
                "type": "function", "function": {"name": "get_weather"},
            })
        );
    }

    #[test]
    fn tool_message_without_call_id_is_rejected() {
        // 丢了 tool_call_id 就没法关联回调用，转成 Anthropic 会被上游 400。
        let err = CHAT
            .decode_request(&json!({
                "model": "m",
                "messages": [{"role": "tool", "content": "结果"}],
            }))
            .expect_err("缺 tool_call_id 应该报错");
        assert_eq!(err.kind, ErrorKind::InvalidRequest);
        assert!(
            err.message.contains("tool_call_id"),
            "消息应指明字段: {}",
            err.message
        );
    }

    #[test]
    fn multiple_tool_results_split_into_separate_tool_messages() {
        // 一条 Chat tool 消息只能带一个 tool_call_id，所以必须拆开。
        let ir = UnifiedRequest::new(
            "m",
            vec![Message::new(
                Role::Tool,
                vec![
                    ContentPart::ToolResult {
                        name: None,
                        id: "a".into(),
                        content: vec![ContentPart::text("一")],
                        is_error: false,
                    },
                    ContentPart::ToolResult {
                        name: None,
                        id: "b".into(),
                        content: vec![ContentPart::text("二")],
                        is_error: true,
                    },
                ],
            )],
        );

        let msgs = enc_req(&ir)["messages"].as_array().unwrap().clone();
        assert_eq!(msgs.len(), 2);
        assert_eq!(
            msgs[0],
            json!({"role": "tool", "tool_call_id": "a", "content": "一"})
        );
        assert_eq!(
            msgs[1],
            json!({"role": "tool", "tool_call_id": "b", "content": "二"})
        );
    }

    #[test]
    fn tool_choice_keywords_map_both_ways() {
        for (raw, expected) in [
            (json!("none"), ToolChoice::None),
            (json!("auto"), ToolChoice::Auto),
            (json!("required"), ToolChoice::Required),
            // "any" 是 Anthropic 的说法，中转站会混用。
            (json!("any"), ToolChoice::Required),
        ] {
            let ir = dec_req(json!({
                "model": "m",
                "messages": [{"role": "user", "content": "x"}],
                "tool_choice": raw,
            }));
            assert_eq!(ir.tool_choice, expected);
        }

        // 未指定时不该往输出里塞 tool_choice。
        let plain = dec_req(json!({"model": "m", "messages": [{"role": "user", "content": "x"}]}));
        assert_eq!(plain.tool_choice, ToolChoice::Unspecified);
        assert_eq!(enc_req(&plain).get("tool_choice"), None);
    }

    #[test]
    fn request_thinking_parts_are_dropped_without_nonstandard_fields() {
        // Chat 请求体里没有推理块的合法位置：reasoning_content 会被 DeepSeek
        // 等上游直接 400，自造顶层字段会被 OpenAI 拒绝。编码必须干净地丢弃，
        // 不得输出任何非标字段。
        let ir = UnifiedRequest::new(
            "m",
            vec![Message::new(
                Role::Assistant,
                vec![
                    ContentPart::Thinking {
                        text: "让我想想".into(),
                        signature: Some("sig-abc".into()),
                    },
                    ContentPart::text("答案是 42"),
                ],
            )],
        );

        let encoded = enc_req(&ir);
        assert_eq!(encoded.get("dropped_thinking"), None, "禁止自造顶层字段");
        let msg = &encoded["messages"][0];
        assert_eq!(msg.get("reasoning_content"), None, "禁止非标消息字段");
        assert_eq!(msg["content"], json!("答案是 42"), "可见文本不受影响");
    }

    #[test]
    fn request_redacted_thinking_is_dropped_but_text_kept() {
        let ir = UnifiedRequest::new(
            "m",
            vec![Message::new(
                Role::Assistant,
                vec![
                    ContentPart::text("前言"),
                    ContentPart::RedactedThinking {
                        data: "opaque==".into(),
                    },
                ],
            )],
        );
        let back = dec_req(enc_req(&ir));
        assert_eq!(back.messages.len(), 1);
        assert_eq!(
            back.messages[0].content,
            vec![ContentPart::text("前言")],
            "加密推理块丢弃，可见文本保留"
        );
    }

    #[test]
    fn mixed_tool_results_split_into_separate_tool_messages() {
        // 一条 Tool 消息带两个结果 → 拆成两条 chat tool 消息；后续 assistant
        // 的推理块丢弃、文本保留。
        let ir = UnifiedRequest::new(
            "m",
            vec![
                Message::new(
                    Role::Tool,
                    vec![
                        ContentPart::ToolResult {
                            name: None,
                            id: "a".into(),
                            content: vec![ContentPart::text("一")],
                            is_error: false,
                        },
                        ContentPart::ToolResult {
                            name: None,
                            id: "b".into(),
                            content: vec![ContentPart::text("二")],
                            is_error: false,
                        },
                    ],
                ),
                Message::new(
                    Role::Assistant,
                    vec![
                        ContentPart::Thinking {
                            text: "综合两个结果".into(),
                            signature: Some("sig-shift".into()),
                        },
                        ContentPart::text("结论"),
                    ],
                ),
            ],
        );

        let encoded = enc_req(&ir);
        let msgs = encoded["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3, "两条 tool + 一条 assistant");
        assert_eq!(msgs[0]["role"], json!("tool"));
        assert_eq!(msgs[0]["tool_call_id"], json!("a"));
        assert_eq!(msgs[1]["role"], json!("tool"));
        assert_eq!(msgs[1]["tool_call_id"], json!("b"));
        assert_eq!(msgs[2]["role"], json!("assistant"));
        assert_eq!(msgs[2]["content"], json!("结论"));

        let back = dec_req(encoded);
        assert_eq!(back.messages.len(), 3);
        assert_eq!(back.messages[2].role, Role::Assistant);
        assert_eq!(back.messages[2].content, vec![ContentPart::text("结论")]);
    }

    #[test]
    fn reasoning_effort_and_budget_convert_in_both_directions() {
        let ir = dec_req(json!({
            "model": "m",
            "messages": [{"role": "user", "content": "x"}],
            "reasoning_effort": "high",
        }));
        assert_eq!(
            ir.reasoning.as_ref().unwrap().effort.as_deref(),
            Some("high")
        );
        assert_eq!(enc_req(&ir)["reasoning_effort"], json!("high"));

        // 从 Anthropic/Gemini 过来时只有预算，要折算成档位，不能静默丢掉。
        let mut budgeted = UnifiedRequest::new("m", vec![Message::text(Role::User, "x")]);
        budgeted.max_output_tokens = Some(10_000);
        budgeted.reasoning = Some(ReasoningConfig {
            effort: None,
            budget_tokens: Some(8_000),
            include_thoughts: Some(true),
        });
        assert_eq!(enc_req(&budgeted)["reasoning_effort"], json!("high"));
    }

    #[test]
    fn response_format_variants_round_trip() {
        for (raw, expected) in [
            (json!({"type": "text"}), ResponseFormat::Text),
            (json!({"type": "json_object"}), ResponseFormat::JsonObject),
            (
                json!({"type": "json_schema", "json_schema": {
                    "name": "answer",
                    "schema": {"type": "object"},
                    "strict": true,
                }}),
                ResponseFormat::JsonSchema {
                    name: "answer".into(),
                    schema: json!({"type": "object"}),
                    strict: true,
                },
            ),
        ] {
            let ir = dec_req(json!({
                "model": "m",
                "messages": [{"role": "user", "content": "x"}],
                "response_format": raw.clone(),
            }));
            assert_eq!(ir.response_format, Some(expected));
            assert_eq!(enc_req(&ir)["response_format"], raw);
        }
    }

    #[test]
    fn unknown_fields_go_to_extensions_and_come_back() {
        let ir = dec_req(json!({
            "model": "m",
            "messages": [{"role": "user", "content": "x"}],
            "logit_bias": {"50256": -100},
            "service_tier": "flex",
        }));
        // 未知字段不该让解析失败，而要带前缀进 extensions。
        assert_eq!(ir.extension("chat.service_tier"), Some(&json!("flex")));
        assert_eq!(
            ir.extension("chat.logit_bias"),
            Some(&json!({"50256": -100}))
        );

        let back = enc_req(&ir);
        assert_eq!(back["service_tier"], json!("flex"));
        assert_eq!(back["logit_bias"], json!({"50256": -100}));
    }

    #[test]
    fn extensions_never_clobber_normalized_fields() {
        // 扩展是兜底，不该盖掉已经归一化过的值。
        let mut ir = UnifiedRequest::new("real-model", vec![Message::text(Role::User, "x")]);
        ir.set_extension("chat.model", json!("hijacked"));
        ir.set_extension("responses.store", json!(true));
        let back = enc_req(&ir);
        assert_eq!(back["model"], json!("real-model"));
        // 别的协议的扩展不属于 Chat，不该被还原。
        assert_eq!(back.get("store"), None);
    }

    #[test]
    fn malformed_tool_arguments_are_kept_as_raw_string() {
        // 流被截断时 arguments 可能是半截 JSON，不能吞掉，也不能报错。
        let ir = dec_req(json!({
            "model": "m",
            "messages": [{"role": "assistant", "tool_calls": [{
                "id": "c1",
                "type": "function",
                "function": {"name": "f", "arguments": "{\"city\":\"北"},
            }]}],
        }));
        assert_eq!(
            ir.messages[0].content,
            vec![ContentPart::ToolUse {
                signature: None,
                id: "c1".into(),
                name: "f".into(),
                input: Value::String("{\"city\":\"北".into()),
            }]
        );

        // 空 arguments 归一成空对象，避免下游拿到 "" 去 parse。
        let empty = dec_req(json!({
            "model": "m",
            "messages": [{"role": "assistant", "tool_calls": [{
                "id": "c2", "type": "function", "function": {"name": "f", "arguments": ""},
            }]}],
        }));
        match &empty.messages[0].content[0] {
            ContentPart::ToolUse { input, .. } => assert_eq!(input, &json!({})),
            other => panic!("期望 ToolUse，得到 {other:?}"),
        }
    }

    #[test]
    fn empty_and_null_content_do_not_produce_phantom_parts() {
        let ir = dec_req(json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": ""},
                {"role": "assistant", "content": null},
                {"role": "user", "content": []},
            ],
        }));
        assert_eq!(ir.messages.len(), 3);
        assert!(ir.messages.iter().all(|m| m.content.is_empty()));

        // 空消息回写成空字符串，而不是 null 或缺字段。
        let back = enc_req(&ir);
        assert_eq!(back["messages"][0]["content"], json!(""));
    }

    #[test]
    fn unsupported_role_is_rejected() {
        let err = CHAT
            .decode_request(&json!({
                "model": "m",
                "messages": [{"role": "function", "content": "x"}],
            }))
            .expect_err("未知角色应该报错");
        assert_eq!(err.kind, ErrorKind::InvalidRequest);
        assert!(
            err.message.contains("function"),
            "消息应含角色名: {}",
            err.message
        );
    }

    #[test]
    fn stream_options_include_usage_is_read_and_written() {
        let ir = dec_req(json!({
            "model": "m",
            "messages": [{"role": "user", "content": "x"}],
            "stream": true,
            "stream_options": {"include_usage": true},
        }));
        assert!(ir.stream);
        assert!(ir.stream_include_usage);

        let back = enc_req(&ir);
        assert_eq!(back["stream"], json!(true));
        assert_eq!(back["stream_options"], json!({"include_usage": true}));

        // 非流式请求不该带 stream 字段。
        let mut off = ir.clone();
        off.stream = false;
        let back_off = enc_req(&off);
        assert_eq!(back_off.get("stream"), None);
        assert_eq!(back_off.get("stream_options"), None);
    }

    // -----------------------------------------------------------------
    // 响应
    // -----------------------------------------------------------------

    #[test]
    fn response_round_trips_with_usage_details() {
        let raw = json!({
            "id": "chatcmpl-1",
            "object": "chat.completion",
            "created": 1_700_000_000i64,
            "model": "gpt-5",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "你好"},
                "finish_reason": "stop",
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "total_tokens": 15,
                "prompt_tokens_details": {"cached_tokens": 4},
                "completion_tokens_details": {"reasoning_tokens": 3},
            },
        });
        let ir = CHAT.decode_response(&raw).expect("解码响应应该成功");
        assert_eq!(ir.id, "chatcmpl-1");
        assert_eq!(ir.model, "gpt-5");
        assert_eq!(ir.created, 1_700_000_000);
        assert_eq!(ir.text(), "你好");
        assert_eq!(ir.stop_reason, Some(StopReason::Stop));
        assert_eq!(
            ir.usage,
            Usage {
                input_tokens: 10,
                output_tokens: 5,
                cached_input_tokens: 4,
                cache_write_tokens: 0,
                reasoning_tokens: 3,
            }
        );

        let back = CHAT.encode_response(&ir).expect("编码响应应该成功");
        assert_eq!(back["object"], json!("chat.completion"));
        assert_eq!(back["choices"][0]["message"]["content"], json!("你好"));
        assert_eq!(back["choices"][0]["finish_reason"], json!("stop"));
        // total_tokens 由 input+output 推出，不能漏。
        assert_eq!(back["usage"]["total_tokens"], json!(15));
        assert_eq!(
            back["usage"]["prompt_tokens_details"]["cached_tokens"],
            json!(4)
        );
        assert_eq!(
            back["usage"]["completion_tokens_details"]["reasoning_tokens"],
            json!(3)
        );
    }

    #[test]
    fn finish_reason_maps_across_every_stop_reason() {
        for (raw, expected) in [
            ("stop", StopReason::Stop),
            ("length", StopReason::MaxTokens),
            ("tool_calls", StopReason::ToolUse),
            // 已废弃的旧字段，语义等同 tool_calls。
            ("function_call", StopReason::ToolUse),
            ("content_filter", StopReason::ContentFilter),
            ("something_new", StopReason::Other),
        ] {
            let ir = CHAT
                .decode_response(&json!({
                    "id": "x", "model": "m", "created": 0,
                    "choices": [{"index": 0, "message": {"role": "assistant", "content": "a"},
                                 "finish_reason": raw}],
                }))
                .expect("解码应该成功");
            assert_eq!(ir.stop_reason, Some(expected), "finish_reason={raw}");
        }

        // 反向：IR 的每个变体都要落到 Chat 的合法取值上。
        assert_eq!(stop_reason_to_finish(StopReason::Stop), "stop");
        assert_eq!(stop_reason_to_finish(StopReason::MaxTokens), "length");
        assert_eq!(stop_reason_to_finish(StopReason::ToolUse), "tool_calls");
        assert_eq!(
            stop_reason_to_finish(StopReason::ContentFilter),
            "content_filter"
        );
        // Chat 没有这些概念，只能收敛到 stop。
        assert_eq!(stop_reason_to_finish(StopReason::StopSequence), "stop");
        assert_eq!(stop_reason_to_finish(StopReason::Refusal), "stop");
        assert_eq!(stop_reason_to_finish(StopReason::PauseTurn), "stop");
        assert_eq!(stop_reason_to_finish(StopReason::Other), "stop");
    }

    #[test]
    fn refusal_and_tool_calls_in_response_are_decoded() {
        let ir = CHAT
            .decode_response(&json!({
                "id": "r1", "model": "m", "created": 1,
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": null,
                        "refusal": "我不能这么做",
                        "tool_calls": [{"id": "c1", "type": "function",
                                        "function": {"name": "f", "arguments": "{\"a\":1}"}}],
                    },
                    "finish_reason": "tool_calls",
                }],
            }))
            .expect("解码应该成功");
        assert_eq!(
            ir.content,
            vec![
                ContentPart::Refusal {
                    text: "我不能这么做".into()
                },
                ContentPart::ToolUse {
                    signature: None,
                    id: "c1".into(),
                    name: "f".into(),
                    input: json!({"a": 1}),
                },
            ]
        );

        let back = CHAT.encode_response(&ir).expect("编码应该成功");
        let msg = &back["choices"][0]["message"];
        assert_eq!(msg["content"], Value::Null);
        assert_eq!(msg["refusal"], json!("我不能这么做"));
        assert_eq!(
            msg["tool_calls"][0]["function"]["arguments"],
            json!("{\"a\":1}")
        );
    }

    #[test]
    fn response_thinking_signature_round_trips() {
        let mut ir = UnifiedResponse::new("r1", "m");
        ir.created = 5;
        ir.content = vec![
            ContentPart::Thinking {
                text: "推理中".into(),
                signature: Some("sig-xyz".into()),
            },
            ContentPart::text("结论"),
        ];
        ir.stop_reason = Some(StopReason::Stop);

        let encoded = CHAT.encode_response(&ir).expect("编码应该成功");
        assert_eq!(
            encoded["dropped_thinking"][0]["signature"],
            json!("sig-xyz")
        );
        assert_eq!(
            encoded["choices"][0]["message"]["reasoning_content"],
            json!("推理中")
        );

        let back = CHAT.decode_response(&encoded).expect("解码应该成功");
        // 顺序与签名都要还原，且推理块不能出现两次。
        assert_eq!(back.content, ir.content);
    }

    #[test]
    fn nonstandard_reasoning_content_becomes_a_thinking_part() {
        // 没有 dropped_thinking 时，中转站的 reasoning_content 也要认。
        let ir = CHAT
            .decode_response(&json!({
                "id": "r", "model": "m", "created": 0,
                "choices": [{"index": 0, "message": {
                    "role": "assistant",
                    "reasoning_content": "先想一下",
                    "content": "答案",
                }, "finish_reason": "stop"}],
            }))
            .expect("解码应该成功");
        assert_eq!(
            ir.content,
            vec![
                ContentPart::Thinking {
                    text: "先想一下".into(),
                    signature: None
                },
                ContentPart::text("答案"),
            ]
        );
    }

    #[test]
    fn error_body_maps_to_typed_gateway_error() {
        let err = CHAT
            .decode_response(&json!({"error": {
                "message": "Invalid value for 'temperature'",
                "type": "invalid_request_error",
                "param": "temperature",
                "code": null,
            }}))
            .expect_err("错误体应该报错");
        assert_eq!(err.kind, ErrorKind::InvalidRequest);
        assert_eq!(err.protocol, Some(Protocol::Chat));
        // param 要拼进消息，否则客户端不知道哪个字段错了。
        assert!(err.message.contains("temperature"), "消息: {}", err.message);

        for (body, kind) in [
            (
                json!({"type": "authentication_error"}),
                ErrorKind::Unauthenticated,
            ),
            (json!({"type": "rate_limit_error"}), ErrorKind::RateLimited),
            (json!({"type": "not_found_error"}), ErrorKind::NotFound),
            (
                json!({"type": "insufficient_quota"}),
                ErrorKind::PermissionDenied,
            ),
            (
                json!({"type": "invalid_request_error", "code": "context_length_exceeded"}),
                ErrorKind::PayloadTooLarge,
            ),
            (json!({"type": "server_error"}), ErrorKind::UpstreamError),
        ] {
            let err = CHAT
                .decode_response(&json!({"error": body}))
                .expect_err("错误体应该报错");
            assert_eq!(err.kind, kind);
        }
    }

    #[test]
    fn response_without_choices_decodes_to_empty_content() {
        // 中转站在内容被过滤时会回空 choices，不该 panic 也不该报错。
        let ir = CHAT
            .decode_response(&json!({"id": "r", "model": "m", "created": 0, "choices": []}))
            .expect("空 choices 应该能解码");
        assert!(ir.content.is_empty());
        assert_eq!(ir.stop_reason, None);
        assert!(ir.usage.is_empty());
    }

    // -----------------------------------------------------------------
    // 流式解码
    // -----------------------------------------------------------------

    #[test]
    fn stream_decodes_text_chunks_into_events() {
        let events = decode_stream(&[
            r#"{"id":"c1","model":"gpt-5","choices":[{"index":0,"delta":{"role":"assistant"}}]}"#,
            r#"{"id":"c1","model":"gpt-5","choices":[{"index":0,"delta":{"content":"你"}}]}"#,
            r#"{"id":"c1","model":"gpt-5","choices":[{"index":0,"delta":{"content":"好"}}]}"#,
            r#"{"id":"c1","model":"gpt-5","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            "[DONE]",
        ]);
        assert_eq!(
            events,
            vec![
                // 首帧要补出 Start，即使上游只发了 role。
                StreamEvent::Start {
                    id: "c1".into(),
                    model: "gpt-5".into(),
                    usage: None,
                },
                StreamEvent::TextDelta {
                    index: 0,
                    text: "你".into()
                },
                StreamEvent::TextDelta {
                    index: 0,
                    text: "好".into()
                },
                StreamEvent::Stop {
                    reason: StopReason::Stop,
                    stop_sequence: None
                },
                StreamEvent::Done,
            ]
        );
    }

    #[test]
    fn stream_decodes_nonstandard_reasoning_fields() {
        // DeepSeek 系用 reasoning_content，另一派中转站用 reasoning，都要认。
        let a = decode_stream(&[
            r#"{"id":"c","model":"m","choices":[{"index":0,"delta":{"reasoning_content":"想"}}]}"#,
            "[DONE]",
        ]);
        assert!(a.contains(&StreamEvent::ThinkingDelta {
            index: 0,
            text: "想".into()
        }));

        let b = decode_stream(&[
            r#"{"id":"c","model":"m","choices":[{"index":0,"delta":{"reasoning":"想"}}]}"#,
            "[DONE]",
        ]);
        assert!(b.contains(&StreamEvent::ThinkingDelta {
            index: 0,
            text: "想".into()
        }));

        // 有些中转站把 reasoning 包成对象。
        let c = decode_stream(&[
            r#"{"id":"c","model":"m","choices":[{"index":0,"delta":{"reasoning":{"content":"想"}}}]}"#,
            "[DONE]",
        ]);
        assert!(c.contains(&StreamEvent::ThinkingDelta {
            index: 0,
            text: "想".into()
        }));
    }

    #[test]
    fn stream_accumulates_tool_calls_by_index() {
        let events = decode_stream(&[
            r#"{"id":"c","model":"m","choices":[{"index":0,"delta":{"tool_calls":[
                {"index":0,"id":"call_a","type":"function",
                 "function":{"name":"get_weather","arguments":""}}]}}]}"#,
            r#"{"id":"c","model":"m","choices":[{"index":0,"delta":{"tool_calls":[
                {"index":0,"function":{"arguments":"{\"city\":"}}]}}]}"#,
            r#"{"id":"c","model":"m","choices":[{"index":0,"delta":{"tool_calls":[
                {"index":0,"function":{"arguments":"\"北京\"}"}}]}}]}"#,
            r#"{"id":"c","model":"m","choices":[{"index":0,"delta":{"tool_calls":[
                {"index":1,"id":"call_b","type":"function",
                 "function":{"name":"get_time","arguments":"{}"}}]}}]}"#,
            r#"{"id":"c","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
            "[DONE]",
        ]);
        assert_eq!(
            events,
            vec![
                StreamEvent::Start {
                    id: "c".into(),
                    model: "m".into(),
                    usage: None
                },
                // 工具块下标避开文本块 0。
                StreamEvent::ToolCallStart {
                    signature: None,
                    index: 1,
                    id: "call_a".into(),
                    name: "get_weather".into(),
                },
                StreamEvent::ToolCallArgsDelta {
                    index: 1,
                    fragment: "{\"city\":".into()
                },
                StreamEvent::ToolCallArgsDelta {
                    index: 1,
                    fragment: "\"北京\"}".into()
                },
                StreamEvent::ToolCallStart {
                    signature: None,
                    index: 2,
                    id: "call_b".into(),
                    name: "get_time".into(),
                },
                StreamEvent::ToolCallArgsDelta {
                    index: 2,
                    fragment: "{}".into()
                },
                StreamEvent::Stop {
                    reason: StopReason::ToolUse,
                    stop_sequence: None
                },
                StreamEvent::Done,
            ]
        );

        // 聚合回内容时两个调用都要完整重建。
        let mut agg = StreamAggregator::new();
        for e in &events {
            agg.absorb(e);
        }
        let content = agg.into_content();
        assert_eq!(
            content,
            vec![
                ContentPart::ToolUse {
                    signature: None,
                    id: "call_a".into(),
                    name: "get_weather".into(),
                    input: json!({"city": "北京"}),
                },
                ContentPart::ToolUse {
                    signature: None,
                    id: "call_b".into(),
                    name: "get_time".into(),
                    input: json!({}),
                },
            ]
        );
    }

    #[test]
    fn stream_decoder_tolerates_missing_ceremony_and_junk() {
        // 中转站直接甩 delta，没有 role 开场帧、没有 id/model。
        let events = decode_stream(&[
            "",
            ": keep-alive payload",
            r#"{"choices":[{"index":0,"delta":{"content":"hi"}}]}"#,
            "[DONE]",
        ]);
        assert_eq!(
            events,
            vec![
                StreamEvent::Start {
                    id: String::new(),
                    model: String::new(),
                    usage: None
                },
                StreamEvent::TextDelta {
                    index: 0,
                    text: "hi".into()
                },
                StreamEvent::Done,
            ]
        );
    }

    #[test]
    fn stream_decoder_emits_done_even_if_upstream_never_sent_it() {
        // 上游断流时也要给下游一个终结事件，否则编码器不会收尾。
        let events = decode_stream(&[r#"{"choices":[{"index":0,"delta":{"content":"a"}}]}"#]);
        assert_eq!(events.last(), Some(&StreamEvent::Done));
        // 但 [DONE] 已经来过时不该重复补。
        let twice = decode_stream(&[r#"{"choices":[]}"#, "[DONE]"]);
        assert_eq!(twice.iter().filter(|e| **e == StreamEvent::Done).count(), 1);
    }

    #[test]
    fn stream_decodes_usage_and_error_frames() {
        let events = decode_stream(&[
            r#"{"id":"c","model":"m","choices":[],"usage":{"prompt_tokens":7,
                "completion_tokens":2,"completion_tokens_details":{"reasoning_tokens":1}}}"#,
            "[DONE]",
        ]);
        assert!(events.contains(&StreamEvent::Usage(Usage {
            input_tokens: 7,
            output_tokens: 2,
            cached_input_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: 1,
        })));

        let errs = decode_stream(&[r#"{"error":{"message":"boom","type":"server_error"}}"#]);
        assert!(errs.contains(&StreamEvent::Error {
            message: "boom".into(),
            kind: "server_error".into(),
        }));
    }

    // -----------------------------------------------------------------
    // 流式编码
    // -----------------------------------------------------------------

    #[test]
    fn stream_encoder_produces_a_complete_chat_sequence() {
        let frames = encode_stream(&[
            StreamEvent::Start {
                id: "c1".into(),
                model: "gpt-5".into(),
                usage: None,
            },
            StreamEvent::TextDelta {
                index: 0,
                text: "你好".into(),
            },
            StreamEvent::Stop {
                reason: StopReason::Stop,
                stop_sequence: None,
            },
            StreamEvent::Usage(Usage {
                input_tokens: 3,
                output_tokens: 4,
                ..Default::default()
            }),
            StreamEvent::Done,
        ]);

        // 开场帧带 role，客户端 SDK 依赖它。
        let first: Value = serde_json::from_str(&frames[0]).unwrap();
        assert_eq!(first["object"], json!("chat.completion.chunk"));
        assert_eq!(first["id"], json!("c1"));
        assert_eq!(first["model"], json!("gpt-5"));
        assert_eq!(first["choices"][0]["delta"]["role"], json!("assistant"));

        let text: Value = serde_json::from_str(&frames[1]).unwrap();
        assert_eq!(text["choices"][0]["delta"]["content"], json!("你好"));

        let stop: Value = serde_json::from_str(&frames[2]).unwrap();
        assert_eq!(stop["choices"][0]["finish_reason"], json!("stop"));

        // usage 单独一帧，choices 为空数组。
        let usage: Value = serde_json::from_str(&frames[3]).unwrap();
        assert_eq!(usage["choices"], json!([]));
        assert_eq!(usage["usage"]["prompt_tokens"], json!(3));
        assert_eq!(usage["usage"]["total_tokens"], json!(7));

        assert_eq!(frames.last().map(String::as_str), Some(DONE_SENTINEL));
        // [DONE] 只能出现一次。
        assert_eq!(frames.iter().filter(|f| *f == DONE_SENTINEL).count(), 1);
    }

    #[test]
    fn stream_encoder_always_terminates_the_stream() {
        // 上游既没发 Stop 也没发 Done —— 编码器必须自己补齐，否则 SDK 会一直挂着。
        let frames = encode_stream(&[
            StreamEvent::Start {
                id: "c".into(),
                model: "m".into(),
                usage: None,
            },
            StreamEvent::TextDelta {
                index: 0,
                text: "半截".into(),
            },
        ]);
        assert_eq!(frames.last().map(String::as_str), Some(DONE_SENTINEL));
        let finish: Value = serde_json::from_str(&frames[frames.len() - 2]).unwrap();
        assert_eq!(finish["choices"][0]["finish_reason"], json!("stop"));

        // 完全没有事件时至少也要发 [DONE]。
        assert_eq!(encode_stream(&[]), vec![DONE_SENTINEL.to_owned()]);
    }

    #[test]
    fn stream_encoder_opens_the_stream_even_without_a_start_event() {
        // 上游省略 Start 时，第一个 delta 也要先补开场帧。
        let frames = encode_stream(&[StreamEvent::TextDelta {
            index: 0,
            text: "x".into(),
        }]);
        let first: Value = serde_json::from_str(&frames[0]).unwrap();
        assert_eq!(first["choices"][0]["delta"]["role"], json!("assistant"));
        // id 缺失时自己造一个，SDK 会读它。
        assert!(
            first["id"].as_str().unwrap().starts_with("chatcmpl-"),
            "应有兜底 id: {}",
            first["id"]
        );
    }

    #[test]
    fn stream_encoder_maps_ir_tool_index_back_to_openai_index() {
        let frames = encode_stream(&[
            StreamEvent::Start {
                id: "c".into(),
                model: "m".into(),
                usage: None,
            },
            // IR 下标从 1 起（0 是文本块），要映回 OpenAI 的 0 起编号。
            StreamEvent::ToolCallStart {
                signature: None,
                index: 1,
                id: "a".into(),
                name: "f".into(),
            },
            StreamEvent::ToolCallArgsDelta {
                index: 1,
                fragment: "{}".into(),
            },
            StreamEvent::ToolCallStart {
                signature: None,
                index: 2,
                id: "b".into(),
                name: "g".into(),
            },
            StreamEvent::Stop {
                reason: StopReason::ToolUse,
                stop_sequence: None,
            },
            StreamEvent::Done,
        ]);
        let start_a: Value = serde_json::from_str(&frames[1]).unwrap();
        let call_a = &start_a["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(call_a["index"], json!(0));
        assert_eq!(call_a["id"], json!("a"));
        assert_eq!(call_a["function"]["name"], json!("f"));

        let args: Value = serde_json::from_str(&frames[2]).unwrap();
        let delta_args = &args["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(delta_args["index"], json!(0));
        assert_eq!(delta_args["function"]["arguments"], json!("{}"));
        // 续帧不该重发 id。
        assert_eq!(delta_args.get("id"), None);

        let start_b: Value = serde_json::from_str(&frames[3]).unwrap();
        assert_eq!(
            start_b["choices"][0]["delta"]["tool_calls"][0]["index"],
            json!(1)
        );

        let stop: Value = serde_json::from_str(&frames[4]).unwrap();
        assert_eq!(stop["choices"][0]["finish_reason"], json!("tool_calls"));
    }

    #[test]
    fn stream_encoder_maps_noncontiguous_ir_indices_by_arrival() {
        // 从 Anthropic 转来时工具块下标取决于前面有几个文本/推理块，
        // 「减 1」会串号，所以按到达顺序分配。
        let frames = encode_stream(&[
            StreamEvent::Start {
                id: "c".into(),
                model: "m".into(),
                usage: None,
            },
            StreamEvent::ToolCallStart {
                signature: None,
                index: 7,
                id: "a".into(),
                name: "f".into(),
            },
            StreamEvent::ToolCallStart {
                signature: None,
                index: 3,
                id: "b".into(),
                name: "g".into(),
            },
            // 回到第一个工具，必须仍然映射到 0。
            StreamEvent::ToolCallArgsDelta {
                index: 7,
                fragment: "{}".into(),
            },
            StreamEvent::Done,
        ]);
        let a: Value = serde_json::from_str(&frames[1]).unwrap();
        let b: Value = serde_json::from_str(&frames[2]).unwrap();
        let a_args: Value = serde_json::from_str(&frames[3]).unwrap();
        assert_eq!(a["choices"][0]["delta"]["tool_calls"][0]["index"], json!(0));
        assert_eq!(b["choices"][0]["delta"]["tool_calls"][0]["index"], json!(1));
        assert_eq!(
            a_args["choices"][0]["delta"]["tool_calls"][0]["index"],
            json!(0)
        );
    }

    #[test]
    fn stream_encoder_passes_thinking_and_refusal_through() {
        let frames = encode_stream(&[
            StreamEvent::Start {
                id: "c".into(),
                model: "m".into(),
                usage: None,
            },
            StreamEvent::ThinkingDelta {
                index: 0,
                text: "想".into(),
            },
            StreamEvent::ThinkingSignature {
                index: 0,
                signature: "sig".into(),
            },
            StreamEvent::RefusalDelta {
                index: 0,
                text: "不行".into(),
            },
            StreamEvent::Done,
        ]);
        let think: Value = serde_json::from_str(&frames[1]).unwrap();
        assert_eq!(
            think["choices"][0]["delta"]["reasoning_content"],
            json!("想")
        );
        let sig: Value = serde_json::from_str(&frames[2]).unwrap();
        assert_eq!(
            sig["choices"][0]["delta"]["reasoning_signature"],
            json!("sig")
        );
        let refusal: Value = serde_json::from_str(&frames[3]).unwrap();
        assert_eq!(refusal["choices"][0]["delta"]["refusal"], json!("不行"));
    }

    #[test]
    fn stream_encoder_drops_ceremony_events_chat_cannot_express() {
        // ContentStart/Stop/Ping 在 Chat 线格式里没有对应物，不该产出空帧。
        let frames = encode_stream(&[
            StreamEvent::Start {
                id: "c".into(),
                model: "m".into(),
                usage: None,
            },
            StreamEvent::ContentStart {
                index: 0,
                kind: PartKind::Text,
            },
            StreamEvent::Ping,
            StreamEvent::ContentStop { index: 0 },
            StreamEvent::TextDelta {
                index: 0,
                text: "a".into(),
            },
            StreamEvent::Done,
        ]);
        // 开场帧 + 文本帧 + finish 帧 + [DONE]，仅此而已。
        assert_eq!(frames.len(), 4);
        assert_eq!(frames.last().map(String::as_str), Some(DONE_SENTINEL));
    }

    #[test]
    fn stream_survives_a_full_decode_encode_round_trip() {
        // 端到端：Chat SSE → IR 事件 → Chat SSE，内容不能变形。
        let events = decode_stream(&[
            r#"{"id":"c1","model":"gpt-5","choices":[{"index":0,"delta":{"content":"你"}}]}"#,
            r#"{"id":"c1","model":"gpt-5","choices":[{"index":0,"delta":{"content":"好"}}]}"#,
            r#"{"id":"c1","model":"gpt-5","choices":[{"index":0,"delta":{},"finish_reason":"length"}]}"#,
            r#"{"id":"c1","model":"gpt-5","choices":[],"usage":{"prompt_tokens":1,"completion_tokens":2}}"#,
            "[DONE]",
        ]);
        let frames = encode_stream(&events);

        // 把编码结果重新解一遍，文本与停止原因要原样回来。
        let reparsed = decode_stream(&frames.iter().map(String::as_str).collect::<Vec<_>>());
        let mut agg = StreamAggregator::new();
        for e in &reparsed {
            agg.absorb(e);
        }
        assert_eq!(agg.id, "c1");
        assert_eq!(agg.model, "gpt-5");
        assert_eq!(agg.stop_reason, Some(StopReason::MaxTokens));
        assert_eq!(agg.usage.input_tokens, 1);
        assert_eq!(agg.usage.output_tokens, 2);
        assert_eq!(agg.into_content(), vec![ContentPart::text("你好")]);
    }

    #[test]
    fn codec_reports_its_protocol() {
        assert_eq!(CHAT.protocol(), Protocol::Chat);
    }
}
