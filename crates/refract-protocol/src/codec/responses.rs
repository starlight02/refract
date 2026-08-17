//! OpenAI Responses API 协议编解码。
//!
//! Responses 是四个协议里结构最特殊的一个：
//!
//! - **item 列表而非消息数组**。请求的 `input` 与响应的 `output` 都是 item 序列，
//!   工具调用、工具结果、推理各自是**独立的顶层 item**，而不是消息内的 content block。
//!   所以 IR 的 `messages` 与 item 列表之间是「一对多」展开关系：一条含
//!   `ToolUse` 的 Assistant 消息会拆成 `message` + N 个 `function_call` item。
//! - **stateful**。`previous_response_id` / `store` / `include` 让上游自己保存对话历史，
//!   其他三个协议都没有对应物。这些字段只能进 [`Extensions`]，转到别的协议时必然丢失 ——
//!   本文件里所有标注「有损转换点」的地方都是这个原因。
//! - **流式仪式性事件最多**，且每个事件带全局递增的 `sequence_number`。
//!
//! 字段与事件名以 OpenAI 官方 OpenAPI spec 为准（2026-08 校对）。

use std::collections::BTreeMap;

use refract_core::{GatewayError, Protocol};
use serde_json::{Map, Value, json};

use crate::codec::{ProtocolCodec, RequestCodec, ResponseCodec, StreamCodec};
use crate::ir::*;
use crate::stream::*;

/// OpenAI Responses API codec。
pub struct ResponsesCodec;

/// 供 [`crate::codec::CodecSet`] 注册的单例。
pub static RESPONSES: ResponsesCodec = ResponsesCodec;

/// 请求里被显式解析的字段，其余进 extensions。
///
/// `previous_response_id` / `store` / `include` 也在这里 —— 它们被解析到
/// extensions 的**具名**键上，不能再作为未知字段兜底一遍。
const KNOWN_REQUEST_FIELDS: &[&str] = &[
    "model",
    "input",
    "instructions",
    "stream",
    "max_output_tokens",
    "tools",
    "tool_choice",
    "parallel_tool_calls",
    "text",
    "reasoning",
    "temperature",
    "top_p",
    "user",
    "metadata",
    "previous_response_id",
    "store",
    "include",
];

// ===== 请求解码 =====

impl RequestCodec for ResponsesCodec {
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

        let mut ir = UnifiedRequest::new(model, Vec::new());

        // instructions 等价于其他协议的 system。
        if let Some(text) = obj.get("instructions").and_then(Value::as_str) {
            ir.system.push(ContentPart::text(text));
        }

        // input 有两种形态：裸字符串（等价于单条 user 消息）或 item 数组。
        match obj.get("input") {
            Some(Value::String(s)) => {
                ir.messages.push(Message::text(Role::User, s.as_str()));
            }
            Some(Value::Array(items)) => {
                ir.messages = decode_input_items(items)?;
                // input 里的 system/developer 消息提升到 ir.system —— 其他
                // 协议的编码器都从 ir.system 取系统指令；残留在 messages 里
                // 会被当成 user 回合（Anthropic/Gemini 没有 system 角色），
                // 指令语义直接丢失。
                let mut rest = Vec::with_capacity(ir.messages.len());
                for msg in ir.messages.drain(..) {
                    if msg.role == Role::System {
                        ir.system.extend(msg.content);
                    } else {
                        rest.push(msg);
                    }
                }
                ir.messages = rest;
            }
            // 带 previous_response_id 时 input 可以为空（继续上一轮），所以不强制。
            Some(other) => {
                return Err(GatewayError::invalid_request(format!(
                    "field `input` must be a string or an array, got {}",
                    type_name(other)
                )));
            }
            None => {}
        }

        ir.stream = obj.get("stream").and_then(Value::as_bool).unwrap_or(false);
        // Responses 的流式 usage 总是在 response.completed 里带回来，
        // 不需要像 Chat 那样显式 stream_options.include_usage。
        ir.stream_include_usage = ir.stream;
        ir.max_output_tokens = obj.get("max_output_tokens").and_then(as_u32);
        ir.parallel_tool_calls = obj.get("parallel_tool_calls").and_then(Value::as_bool);
        ir.sampling.temperature = obj.get("temperature").and_then(Value::as_f64);
        ir.sampling.top_p = obj.get("top_p").and_then(Value::as_f64);
        ir.user = obj.get("user").and_then(Value::as_str).map(str::to_owned);

        if let Some(tools) = obj.get("tools").and_then(Value::as_array) {
            let (functions, builtins) = decode_tools(tools);
            ir.tools = functions;
            if !builtins.is_empty() {
                ir.set_extension("responses.builtin_tools", Value::Array(builtins));
            }
        }
        if let Some(choice) = obj.get("tool_choice") {
            ir.tool_choice = decode_tool_choice(choice);
        }
        if let Some(format) = obj.get("text").and_then(|t| t.get("format")) {
            ir.response_format = decode_response_format(format);
        }
        if let Some(reasoning) = obj.get("reasoning") {
            ir.reasoning = decode_reasoning(reasoning);
        }

        // stateful 字段：有损转换点。Responses 让上游保存对话状态，其他三个协议
        // 都是无状态的，转过去必然丢失，所以只能原样存着等回到 Responses 时还原。
        for key in ["previous_response_id", "store", "include", "metadata"] {
            if let Some(v) = obj.get(key).filter(|v| !v.is_null()) {
                ir.set_extension(format!("responses.{key}"), v.clone());
            }
        }

        // 未知字段兜底，保证透传不丢东西。
        for (k, v) in obj {
            if !KNOWN_REQUEST_FIELDS.contains(&k.as_str()) {
                ir.set_extension(format!("responses.{k}"), v.clone());
            }
        }

        Ok(ir)
    }

    fn encode_request(&self, ir: &UnifiedRequest) -> Result<Value, GatewayError> {
        let mut out = Map::new();
        out.insert("model".into(), json!(ir.model));

        if !ir.system.is_empty() {
            out.insert("instructions".into(), json!(ir.system_text()));
        }
        out.insert(
            "input".into(),
            Value::Array(encode_input_items(&ir.messages)),
        );

        if ir.stream {
            out.insert("stream".into(), json!(true));
        }
        if let Some(max) = ir.max_output_tokens {
            out.insert("max_output_tokens".into(), json!(max));
        }
        if let Some(t) = ir.sampling.temperature {
            out.insert("temperature".into(), json!(t));
        }
        if let Some(p) = ir.sampling.top_p {
            out.insert("top_p".into(), json!(p));
        }
        if let Some(p) = ir.parallel_tool_calls {
            out.insert("parallel_tool_calls".into(), json!(p));
        }
        if let Some(u) = &ir.user {
            out.insert("user".into(), json!(u));
        }

        // Responses 不支持这些采样旋钮，静默丢弃而非报错 —— 编码器要尽力而为。
        if ir.sampling.top_k.is_some()
            || ir.sampling.frequency_penalty.is_some()
            || ir.sampling.presence_penalty.is_some()
            || ir.sampling.seed.is_some()
            || !ir.sampling.stop.is_empty()
        {
            tracing::debug!(
                "responses: dropping unsupported sampling knobs (top_k/penalties/seed/stop)"
            );
        }

        let mut tools = encode_tools(&ir.tools);
        // 内置工具（web_search / mcp ...）原样还原 —— 只有回到 Responses
        // 协议才有意义，其他协议的编码器不认识这个 extension。
        if let Some(Value::Array(builtins)) = ir.extension("responses.builtin_tools") {
            tools.extend(builtins.iter().cloned());
        }
        if !tools.is_empty() {
            out.insert("tools".into(), Value::Array(tools));
        }
        if let Some(choice) = encode_tool_choice(&ir.tool_choice) {
            out.insert("tool_choice".into(), choice);
        }
        if let Some(format) = ir.response_format.as_ref().map(encode_response_format) {
            out.insert("text".into(), json!({"format": format}));
        }
        if let Some(v) = ir
            .reasoning
            .as_ref()
            .and_then(|r| encode_reasoning(r, ir.max_output_tokens))
        {
            out.insert("reasoning".into(), v);
        }

        // 还原本协议专属字段。builtin_tools 是我们自造的搬运键，上面已经
        // 并进 tools 数组，写成顶层字段会被上游 400。
        for (key, value) in &ir.extensions {
            if let Some(field) = key.strip_prefix("responses.")
                && field != "builtin_tools"
            {
                out.insert(field.to_owned(), value.clone());
            }
        }

        Ok(Value::Object(out))
    }
}

/// 把 `input` item 数组解码成 IR 消息序列。
///
/// item 是扁平的，消息边界要自己推断：连续的 `function_call` 会并入前一条
/// Assistant 消息（模型一次并行调用多个工具就是这个形态），否则新起一条。
fn decode_input_items(items: &[Value]) -> Result<Vec<Message>, GatewayError> {
    let mut messages: Vec<Message> = Vec::with_capacity(items.len());

    for item in items {
        let obj = match item.as_object() {
            Some(o) => o,
            None => {
                return Err(GatewayError::invalid_request(
                    "each element of `input` must be an object",
                ));
            }
        };
        // 缺 type 时按 message 处理：带 role 的裸对象是 SDK 常见简写。
        let kind = obj
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or(if obj.contains_key("role") {
                "message"
            } else {
                ""
            });

        match kind {
            "message" => {
                let role = match obj.get("role").and_then(Value::as_str) {
                    Some("assistant") => Role::Assistant,
                    Some("system" | "developer") => Role::System,
                    Some("tool") => Role::Tool,
                    // user 及未知角色都按 user 处理，宽松优先。
                    _ => Role::User,
                };
                let content = decode_message_content(obj.get("content"));
                messages.push(Message::new(role, content));
            }
            "function_call" => {
                let part = ContentPart::ToolUse {
                    id: obj
                        .get("call_id")
                        .or_else(|| obj.get("id"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    name: obj
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    // arguments 是 JSON **字符串**，不是对象。解析失败就原样留着，
                    // 让上层看到上游到底发了什么，而不是丢掉。
                    input: parse_arguments(obj.get("arguments")),
                    signature: None,
                };
                push_into(&mut messages, Role::Assistant, part);
            }
            "function_call_output" => {
                let part = ContentPart::ToolResult {
                    id: obj
                        .get("call_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    name: None,
                    content: decode_tool_output(obj.get("output")),
                    // Responses 没有 is_error 字段，错误只能靠 output 文本表达。
                    is_error: false,
                };
                push_into(&mut messages, Role::Tool, part);
            }
            "reasoning" => {
                // summary[] 拼成推理文本；encrypted_content 塞进 signature ——
                // 两者性质相同：不透明、且多轮必须原样回传。
                let mut text = String::new();
                if let Some(summary) = obj.get("summary").and_then(Value::as_array) {
                    for entry in summary {
                        if let Some(s) = entry.get("text").and_then(Value::as_str) {
                            if !text.is_empty() {
                                text.push('\n');
                            }
                            text.push_str(s);
                        }
                    }
                }
                // 新版模型还会带 content[{type:"reasoning_text"}]，一并吸收。
                if let Some(content) = obj.get("content").and_then(Value::as_array) {
                    for entry in content {
                        if let Some(s) = entry.get("text").and_then(Value::as_str) {
                            if !text.is_empty() {
                                text.push('\n');
                            }
                            text.push_str(s);
                        }
                    }
                }
                let part = ContentPart::Thinking {
                    text,
                    signature: obj
                        .get("encrypted_content")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                };
                push_into(&mut messages, Role::Assistant, part);
            }
            // 未知 item（web_search_call 等内置工具的调用记录）不丢弃：
            // 多轮对话把上一轮输出原样发回来是 Responses 的标准用法，
            // 丢了这些 item 会损坏对话历史。包成 Opaque，responses→responses
            // 直通时原样还原，跨协议时由目标编码器丢弃。
            other => {
                tracing::debug!(item_type = other, "responses: opaque input item");
                push_into(
                    &mut messages,
                    Role::Assistant,
                    ContentPart::Opaque {
                        protocol: "responses".to_owned(),
                        value: item.clone(),
                    },
                );
            }
        }
    }

    Ok(messages)
}

/// 把片段并入末条同角色消息，否则新起一条。
fn push_into(messages: &mut Vec<Message>, role: Role, part: ContentPart) {
    match messages.last_mut() {
        Some(last) if last.role == role => last.content.push(part),
        _ => messages.push(Message::new(role, vec![part])),
    }
}

/// 解码 message item 的 content（字符串或 part 数组）。
fn decode_message_content(content: Option<&Value>) -> Vec<ContentPart> {
    match content {
        Some(Value::String(s)) => vec![ContentPart::text(s.as_str())],
        Some(Value::Array(parts)) => parts.iter().filter_map(decode_content_part).collect(),
        _ => Vec::new(),
    }
}

/// 解码单个 content part。
fn decode_content_part(part: &Value) -> Option<ContentPart> {
    let obj = part.as_object()?;
    match obj.get("type").and_then(Value::as_str).unwrap_or_default() {
        // input_text 与 output_text 在 IR 里都是纯文本，区别只在方向。
        "input_text" | "output_text" | "text" | "summary_text" => Some(ContentPart::Text {
            text: obj
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        }),
        "input_image" | "image" => {
            let detail = obj
                .get("detail")
                .and_then(Value::as_str)
                // auto 是默认值，不必往下游传。
                .filter(|d| *d != "auto")
                .map(str::to_owned);
            if let Some(id) = obj.get("file_id").and_then(Value::as_str) {
                return Some(ContentPart::Image {
                    source: MediaSource::FileId(id.to_owned()),
                    mime: None,
                    detail,
                });
            }
            let url = obj.get("image_url").and_then(Value::as_str)?;
            // image_url 可能是 data URI，拆出 base64 与 mime 供 Anthropic 使用。
            let (source, mime) = MediaSource::parse_data_uri(url);
            Some(ContentPart::Image {
                source,
                mime,
                detail,
            })
        }
        "input_file" | "file" => {
            let name = obj
                .get("filename")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let (source, mime) = if let Some(data) = obj.get("file_data").and_then(Value::as_str) {
                MediaSource::parse_data_uri(data)
            } else if let Some(id) = obj.get("file_id").and_then(Value::as_str) {
                (MediaSource::FileId(id.to_owned()), None)
            } else {
                (
                    MediaSource::Url(obj.get("file_url").and_then(Value::as_str)?.to_owned()),
                    None,
                )
            };
            Some(ContentPart::File { source, mime, name })
        }
        "input_audio" | "audio" => {
            let inner = obj.get("input_audio").unwrap_or(part);
            let data = inner.get("data").and_then(Value::as_str)?;
            let (source, _) = MediaSource::parse_data_uri(data);
            Some(ContentPart::Audio {
                source,
                format: inner
                    .get("format")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            })
        }
        "refusal" => Some(ContentPart::Refusal {
            text: obj
                .get("refusal")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        }),
        _ => None,
    }
}

/// 解码 `function_call_output.output`（字符串或内容数组）。
fn decode_tool_output(output: Option<&Value>) -> Vec<ContentPart> {
    match output {
        Some(Value::String(s)) => vec![ContentPart::text(s.as_str())],
        Some(Value::Array(parts)) => parts.iter().filter_map(decode_content_part).collect(),
        // 结构化结果（对象/数字）序列化成文本，总比丢弃好。
        Some(other) if !other.is_null() => vec![ContentPart::text(other.to_string())],
        _ => Vec::new(),
    }
}

/// 解码 `tools[]`。返回 `(函数工具, 内置工具原文)`。
///
/// 注意 Responses 的函数工具是**扁平结构**（`{type,name,parameters}`），
/// 不像 Chat 那样嵌在 `function` 对象里。内置工具（web_search / mcp ...）
/// 在 IR 里无从表达，原文返回给调用方存 extensions —— 直通回 Responses
/// 时必须还原，静默吞掉会让内置工具在网关后面莫名失效。
fn decode_tools(tools: &[Value]) -> (Vec<ToolDef>, Vec<Value>) {
    let mut out = Vec::with_capacity(tools.len());
    let mut builtins = Vec::new();
    for tool in tools {
        let obj = match tool.as_object() {
            Some(o) => o,
            None => continue,
        };
        if obj
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("function")
            != "function"
        {
            builtins.push(tool.clone());
            continue;
        }
        // 兼容中转站把 Chat 的嵌套形态直接透传过来的情况。
        let src = obj
            .get("function")
            .and_then(Value::as_object)
            .unwrap_or(obj);
        let name = match src.get("name").and_then(Value::as_str) {
            Some(n) if !n.is_empty() => n.to_owned(),
            _ => continue,
        };
        out.push(ToolDef {
            name,
            description: src
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_owned),
            parameters: src
                .get("parameters")
                .filter(|v| !v.is_null())
                .cloned()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
            strict: src.get("strict").and_then(Value::as_bool),
        });
    }
    (out, builtins)
}

/// 解码 `tool_choice`。
fn decode_tool_choice(choice: &Value) -> ToolChoice {
    match choice {
        Value::String(s) => match s.as_str() {
            "none" => ToolChoice::None,
            "auto" => ToolChoice::Auto,
            "required" => ToolChoice::Required,
            _ => ToolChoice::Unspecified,
        },
        Value::Object(o) => match o.get("name").and_then(Value::as_str) {
            Some(name) => ToolChoice::Tool(name.to_owned()),
            // {type:"web_search"} 之类的内置工具约束无法表达，退化成 Required。
            None => ToolChoice::Required,
        },
        _ => ToolChoice::Unspecified,
    }
}

/// 解码 `text.format`。
fn decode_response_format(format: &Value) -> Option<ResponseFormat> {
    match format.get("type").and_then(Value::as_str)? {
        "text" => Some(ResponseFormat::Text),
        "json_object" => Some(ResponseFormat::JsonObject),
        "json_schema" => Some(ResponseFormat::JsonSchema {
            name: format
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("response")
                .to_owned(),
            schema: format.get("schema").cloned().unwrap_or(Value::Null),
            strict: format
                .get("strict")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }),
        _ => None,
    }
}

/// 解码 `reasoning`。
fn decode_reasoning(reasoning: &Value) -> Option<ReasoningConfig> {
    let effort = reasoning
        .get("effort")
        .and_then(Value::as_str)
        .map(str::to_owned);
    // summary 非 none 即表示要求输出推理摘要。
    let include_thoughts = reasoning
        .get("summary")
        .and_then(Value::as_str)
        .map(|s| s != "none");
    if effort.is_none() && include_thoughts.is_none() {
        return None;
    }
    Some(ReasoningConfig {
        effort,
        // Responses 只有档位没有预算，预算留空，由目标协议按需折算。
        budget_tokens: None,
        include_thoughts,
    })
}

/// 编码 `reasoning`。
fn encode_reasoning(cfg: &ReasoningConfig, max_output: Option<u32>) -> Option<Value> {
    let mut obj = Map::new();
    // 从 Anthropic/Gemini 过来时只有 budget_tokens，要折算回档位，
    // 否则思考能力会被静默降级。
    if let Some(effort) = cfg.effort_or_from_budget(max_output) {
        obj.insert("effort".into(), json!(effort));
    }
    if let Some(include) = cfg.include_thoughts {
        obj.insert(
            "summary".into(),
            json!(if include { "auto" } else { "none" }),
        );
    }
    if obj.is_empty() {
        return None;
    }
    Some(Value::Object(obj))
}

/// 编码 `tools[]`（扁平结构）。
fn encode_tools(tools: &[ToolDef]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            let mut obj = Map::new();
            obj.insert("type".into(), json!("function"));
            obj.insert("name".into(), json!(t.name));
            if let Some(d) = &t.description {
                obj.insert("description".into(), json!(d));
            }
            obj.insert("parameters".into(), t.parameters.clone());
            if let Some(s) = t.strict {
                obj.insert("strict".into(), json!(s));
            }
            Value::Object(obj)
        })
        .collect()
}

/// 编码 `tool_choice`。
fn encode_tool_choice(choice: &ToolChoice) -> Option<Value> {
    match choice {
        ToolChoice::Unspecified => None,
        ToolChoice::Auto => Some(json!("auto")),
        ToolChoice::Required => Some(json!("required")),
        ToolChoice::None => Some(json!("none")),
        ToolChoice::Tool(name) => Some(json!({"type": "function", "name": name})),
    }
}

/// 编码 `text.format`。
fn encode_response_format(format: &ResponseFormat) -> Value {
    match format {
        ResponseFormat::Text => json!({"type": "text"}),
        ResponseFormat::JsonObject => json!({"type": "json_object"}),
        ResponseFormat::JsonSchema {
            name,
            schema,
            strict,
        } => json!({
            "type": "json_schema",
            "name": name,
            "schema": schema,
            "strict": strict,
        }),
    }
}

/// 把 IR 消息展开成 `input` item 列表。
///
/// 一条消息可能产出多个 item：`ToolUse` 与 `Thinking` 都是独立顶层 item，
/// 只有普通内容才包进 `message`。
fn encode_input_items(messages: &[Message]) -> Vec<Value> {
    let mut items = Vec::with_capacity(messages.len());

    for msg in messages {
        let mut parts = Vec::new();

        for part in &msg.content {
            match part {
                ContentPart::ToolUse {
                    id, name, input, ..
                } => items.push(json!({
                    "type": "function_call",
                    "call_id": id,
                    "name": name,
                    // arguments 必须是字符串。
                    "arguments": stringify_arguments(input),
                })),
                ContentPart::ToolResult {
                    id,
                    content,
                    is_error,
                    ..
                } => {
                    let mut text = flatten_text(content);
                    // is_error 无字段可放，只能加前缀 —— 否则模型看不出工具失败了。
                    // 空文本的错误结果也要有标记，不然失败被静默吞掉。
                    if *is_error {
                        if text.is_empty() {
                            text.push_str("Error: tool call failed");
                        } else {
                            text.insert_str(0, "Error: ");
                        }
                    }
                    items.push(json!({
                        "type": "function_call_output",
                        "call_id": id,
                        "output": text,
                    }));
                }
                ContentPart::Thinking { text, signature } => {
                    let mut obj = Map::new();
                    obj.insert("type".into(), json!("reasoning"));
                    // 空 summary 用空数组，不发空文本条目 —— 上游会拒绝
                    // 空的 summary_text。
                    if text.is_empty() {
                        obj.insert("summary".into(), json!([]));
                    } else {
                        obj.insert(
                            "summary".into(),
                            json!([{"type": "summary_text", "text": text}]),
                        );
                    }
                    // signature 无损回传：Responses 这一侧它叫 encrypted_content。
                    if let Some(sig) = signature {
                        obj.insert("encrypted_content".into(), json!(sig));
                    }
                    items.push(Value::Object(obj));
                }
                // 加密推理块同样必须原样回传。
                ContentPart::RedactedThinking { data } => items.push(json!({
                    "type": "reasoning",
                    "summary": [],
                    "encrypted_content": data,
                })),
                ContentPart::Opaque { protocol, value } => {
                    // responses→responses 直通：内置工具的调用记录原样回传
                    // （web_search_call 等，多轮必须带回）。跨协议丢弃。
                    if protocol == "responses" {
                        items.push(value.clone());
                    } else {
                        tracing::debug!(%protocol, "responses: 非本协议 Opaque item，已丢弃");
                    }
                }
                other => {
                    if let Some(p) = encode_content_part(other, msg.role) {
                        parts.push(p);
                    }
                }
            }
        }

        if !parts.is_empty() {
            items.push(json!({
                "type": "message",
                "role": role_str(msg.role),
                "content": parts,
            }));
        }
    }

    items
}

/// 编码单个 content part。
///
/// 方向敏感：assistant 的文本是 `output_text`，其余是 `input_text`。发错了
/// 上游会拒绝整条 item。
fn encode_content_part(part: &ContentPart, role: Role) -> Option<Value> {
    match part {
        ContentPart::Text { text } => {
            let kind = if role == Role::Assistant {
                "output_text"
            } else {
                "input_text"
            };
            // output_text 的 annotations 是必填字段。
            if role == Role::Assistant {
                Some(json!({"type": kind, "text": text, "annotations": []}))
            } else {
                Some(json!({"type": kind, "text": text}))
            }
        }
        ContentPart::Image {
            source,
            mime,
            detail,
        } => {
            let mut obj = Map::new();
            obj.insert("type".into(), json!("input_image"));
            match source {
                MediaSource::FileId(id) => {
                    obj.insert("file_id".into(), json!(id));
                }
                // base64 要还原成 data URI，Responses 没有独立的 base64 字段。
                other => {
                    obj.insert(
                        "image_url".into(),
                        json!(other.to_data_uri(mime.as_deref())),
                    );
                }
            }
            obj.insert("detail".into(), json!(detail.as_deref().unwrap_or("auto")));
            Some(Value::Object(obj))
        }
        ContentPart::File { source, mime, name } => {
            let mut obj = Map::new();
            obj.insert("type".into(), json!("input_file"));
            match source {
                MediaSource::FileId(id) => {
                    obj.insert("file_id".into(), json!(id));
                }
                MediaSource::Url(u) => {
                    obj.insert("file_url".into(), json!(u));
                }
                // base64 与纯文本都要还原成 data URI，Responses 没有独立字段。
                MediaSource::Base64(_) | MediaSource::Text(_) => {
                    obj.insert(
                        "file_data".into(),
                        json!(source.to_data_uri(mime.as_deref())),
                    );
                }
            }
            if let Some(n) = name {
                obj.insert("filename".into(), json!(n));
            }
            Some(Value::Object(obj))
        }
        ContentPart::Refusal { text } => Some(json!({"type": "refusal", "refusal": text})),
        // Responses 目前没有音频输入 item，丢弃并记录。
        ContentPart::Audio { .. } => {
            tracing::debug!("responses: dropping audio part (no input item type)");
            None
        }
        _ => None,
    }
}

/// 角色的协议字符串。
///
/// IR 的 `Tool` 角色在 Responses 里没有 message 形态（工具结果是独立 item），
/// 走到这里说明是残留内容，按 user 发出去最保险。
const fn role_str(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::Assistant => "assistant",
        Role::User | Role::Tool => "user",
    }
}

/// 把内容片段拼成纯文本。
fn flatten_text(parts: &[ContentPart]) -> String {
    let mut out = String::new();
    for part in parts {
        if let ContentPart::Text { text } = part {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(text);
        }
    }
    out
}

/// 把 `arguments` 字符串解析成 JSON。
///
/// 解析失败时保留原始字符串而不是报错 —— 流被截断的工具入参是常态。
fn parse_arguments(value: Option<&Value>) -> Value {
    match value {
        Some(Value::String(s)) if s.trim().is_empty() => json!({}),
        Some(Value::String(s)) => {
            serde_json::from_str(s).unwrap_or_else(|_| Value::String(s.clone()))
        }
        // 有些中转站直接发对象，宽容接受。
        Some(other) if !other.is_null() => other.clone(),
        _ => json!({}),
    }
}

/// 把入参渲染成 `arguments` 字符串。
fn stringify_arguments(input: &Value) -> String {
    match input {
        // 已经是字符串说明是截断的原始片段，原样回传。
        Value::String(s) => s.clone(),
        Value::Null => "{}".to_owned(),
        other => other.to_string(),
    }
}

/// 宽松读取 u32。
fn as_u32(value: &Value) -> Option<u32> {
    value.as_u64().and_then(|v| u32::try_from(v).ok())
}

/// JSON 值的类型名，用于错误消息。
const fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

// ===== 响应解码 =====

impl ResponseCodec for ResponsesCodec {
    fn decode_response(&self, raw: &Value) -> Result<UnifiedResponse, GatewayError> {
        let obj = raw
            .as_object()
            .ok_or_else(|| GatewayError::invalid_request("response body must be a JSON object"))?;

        // 错误体优先：上游用 200 包裹错误的情况不少见。
        if let Some(err) = obj.get("error").filter(|v| !v.is_null()) {
            return Err(decode_error_body(err));
        }

        // 有些实现把响应包在 {response:{...}} 里（流式 response.completed 复用同一结构）。
        if let Some(inner) = obj.get("response").and_then(Value::as_object) {
            return self.decode_response(&Value::Object(inner.clone()));
        }

        let id = obj
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let model = obj
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();

        let mut ir = UnifiedResponse::new(id, model);
        if let Some(created) = obj.get("created_at").and_then(Value::as_i64) {
            ir.created = created;
        }

        let output = obj.get("output").and_then(Value::as_array);
        if let Some(items) = output {
            ir.content = decode_output_items(items);
        }

        let status = obj
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("completed");
        let incomplete_reason = obj
            .get("incomplete_details")
            .and_then(|d| d.get("reason"))
            .and_then(Value::as_str);
        let has_tool_call = ir
            .content
            .iter()
            .any(|p| matches!(p, ContentPart::ToolUse { .. }));
        let has_refusal = ir
            .content
            .iter()
            .any(|p| matches!(p, ContentPart::Refusal { .. }));
        ir.stop_reason = Some(map_status(
            status,
            incomplete_reason,
            has_tool_call,
            has_refusal,
        ));

        if let Some(usage) = obj.get("usage") {
            ir.usage = decode_usage(usage);
        }

        // stateful 标识回传，供同协议直连时保留会话链。
        for key in ["previous_response_id", "store", "conversation"] {
            if let Some(v) = obj.get(key).filter(|v| !v.is_null()) {
                ir.extensions.insert(format!("responses.{key}"), v.clone());
            }
        }

        Ok(ir)
    }

    fn encode_response(&self, ir: &UnifiedResponse) -> Result<Value, GatewayError> {
        let (output, has_tool_call) = encode_output_items(ir);
        let reason = ir.stop_reason.unwrap_or(StopReason::Stop);

        let mut out = Map::new();
        out.insert("id".into(), json!(ir.id));
        out.insert("object".into(), json!("response"));
        out.insert("created_at".into(), json!(ir.created));
        out.insert("model".into(), json!(ir.model));
        out.insert("status".into(), json!(status_str(reason)));
        out.insert("output".into(), Value::Array(output));
        // output_text 是 SDK 的便捷聚合字段，客户端普遍依赖它。
        out.insert("output_text".into(), json!(ir.text()));
        out.insert("parallel_tool_calls".into(), json!(true));
        out.insert("tool_choice".into(), json!("auto"));
        out.insert("tools".into(), json!([]));
        out.insert("usage".into(), encode_usage(&ir.usage));

        if let Some(details) = incomplete_details(reason) {
            out.insert("incomplete_details".into(), details);
        } else {
            out.insert("incomplete_details".into(), Value::Null);
        }
        out.insert("error".into(), Value::Null);

        // 停止序列在 Responses 里没有落点，只能借 metadata 表达。
        if let Some(seq) = &ir.stop_sequence {
            out.insert("metadata".into(), json!({"stop_sequence": seq}));
        }
        // 有工具调用时 tool_choice 保持 auto 即可，但记录一下便于排查。
        if has_tool_call {
            tracing::debug!("responses: encoded response carries function_call items");
        }

        for (key, value) in &ir.extensions {
            if let Some(field) = key.strip_prefix("responses.") {
                out.insert(field.to_owned(), value.clone());
            }
        }

        Ok(Value::Object(out))
    }
}

/// 解码 `output` item 列表。
fn decode_output_items(items: &[Value]) -> Vec<ContentPart> {
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let obj = match item.as_object() {
            Some(o) => o,
            None => continue,
        };
        match obj.get("type").and_then(Value::as_str).unwrap_or_default() {
            "message" => {
                if let Some(parts) = obj.get("content").and_then(Value::as_array) {
                    out.extend(parts.iter().filter_map(decode_content_part));
                }
            }
            "function_call" => out.push(ContentPart::ToolUse {
                // call_id 才是关联工具结果用的 ID，item 的 id 只是 item 标识。
                id: obj
                    .get("call_id")
                    .or_else(|| obj.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                name: obj
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                input: parse_arguments(obj.get("arguments")),
                signature: None,
            }),
            "reasoning" => {
                let mut text = String::new();
                for key in ["summary", "content"] {
                    if let Some(entries) = obj.get(key).and_then(Value::as_array) {
                        for entry in entries {
                            if let Some(s) = entry.get("text").and_then(Value::as_str) {
                                if !text.is_empty() {
                                    text.push('\n');
                                }
                                text.push_str(s);
                            }
                        }
                    }
                }
                out.push(ContentPart::Thinking {
                    text,
                    signature: obj
                        .get("encrypted_content")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                });
            }
            other => {
                // web_search_call 等内置工具的执行记录：responses→responses
                // 直通时必须原样保留（客户端下一轮会回传），跨协议丢弃。
                tracing::debug!(item_type = other, "responses: opaque output item");
                out.push(ContentPart::Opaque {
                    protocol: "responses".to_owned(),
                    value: item.clone(),
                });
            }
        }
    }
    out
}

/// 把 IR 内容编码成 `output` item 列表，并报告是否含工具调用。
fn encode_output_items(ir: &UnifiedResponse) -> (Vec<Value>, bool) {
    let mut items = Vec::new();
    let mut message_parts = Vec::new();
    let mut has_tool_call = false;

    for part in &ir.content {
        match part {
            ContentPart::ToolUse {
                id, name, input, ..
            } => {
                has_tool_call = true;
                items.push(json!({
                    "type": "function_call",
                    "id": format!("fc_{id}"),
                    "call_id": id,
                    "name": name,
                    "arguments": stringify_arguments(input),
                    "status": "completed",
                }));
            }
            ContentPart::Thinking { text, signature } => {
                let mut obj = Map::new();
                obj.insert("type".into(), json!("reasoning"));
                obj.insert("id".into(), json!(format!("rs_{}", ir.id)));
                if text.is_empty() {
                    obj.insert("summary".into(), json!([]));
                } else {
                    obj.insert(
                        "summary".into(),
                        json!([{"type": "summary_text", "text": text}]),
                    );
                }
                if let Some(sig) = signature {
                    obj.insert("encrypted_content".into(), json!(sig));
                }
                items.push(Value::Object(obj));
            }
            ContentPart::RedactedThinking { data } => items.push(json!({
                "type": "reasoning",
                "id": format!("rs_{}", ir.id),
                "summary": [],
                "encrypted_content": data,
            })),
            ContentPart::Text { text } => message_parts.push(json!({
                "type": "output_text",
                "text": text,
                "annotations": [],
            })),
            ContentPart::Refusal { text } => {
                message_parts.push(json!({"type": "refusal", "refusal": text}));
            }
            ContentPart::Opaque { protocol, value } => {
                if protocol == "responses" {
                    items.push(value.clone());
                } else {
                    tracing::debug!(%protocol, "responses: 非本协议 Opaque item，已丢弃");
                }
            }
            other => {
                tracing::debug!("responses: dropping unsupported output part {other:?}");
            }
        }
    }

    // message item 收在最后：Responses 的惯例是 reasoning → function_call → message，
    // 而文本总是模型最终的回答。
    if !message_parts.is_empty() {
        items.push(json!({
            "type": "message",
            "id": format!("msg_{}", ir.id),
            "role": "assistant",
            "status": "completed",
            "content": message_parts,
        }));
    }

    (items, has_tool_call)
}

/// `status` + `incomplete_details.reason` → [`StopReason`]。
fn map_status(
    status: &str,
    incomplete_reason: Option<&str>,
    has_tool_call: bool,
    has_refusal: bool,
) -> StopReason {
    match status {
        "incomplete" => match incomplete_reason {
            Some("max_output_tokens" | "max_tokens") => StopReason::MaxTokens,
            Some("content_filter") => StopReason::ContentFilter,
            _ => StopReason::Other,
        },
        "failed" | "cancelled" => StopReason::Other,
        // queued/in_progress 出现在非终态响应里，语义上还没停。
        "queued" | "in_progress" => StopReason::Other,
        // completed：工具调用优先于拒答，因为它决定客户端要不要继续这一轮。
        _ if has_tool_call => StopReason::ToolUse,
        _ if has_refusal => StopReason::Refusal,
        _ => StopReason::Stop,
    }
}

/// [`StopReason`] → `status`。
const fn status_str(reason: StopReason) -> &'static str {
    match reason {
        StopReason::MaxTokens | StopReason::ContentFilter => "incomplete",
        // Stop / StopSequence / ToolUse / Refusal / PauseTurn / Other 都编成
        // completed —— Responses 的 status 只表达「生成是否走完」。Other 来自
        // 其他协议的未知停止原因（如 Gemini 的 OTHER），带着正常内容编成
        // failed 会让客户端把好响应当错误丢弃；真正的上游失败走 error 路径，
        // 不会到这里。
        _ => "completed",
    }
}

/// 截断类停止原因的 `incomplete_details`。
fn incomplete_details(reason: StopReason) -> Option<Value> {
    match reason {
        StopReason::MaxTokens => Some(json!({"reason": "max_output_tokens"})),
        StopReason::ContentFilter => Some(json!({"reason": "content_filter"})),
        _ => None,
    }
}

/// 解码 `usage`。
fn decode_usage(usage: &Value) -> Usage {
    Usage {
        input_tokens: usage
            .get("input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        output_tokens: usage
            .get("output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cached_input_tokens: usage
            .get("input_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cache_write_tokens: usage
            .get("input_tokens_details")
            .and_then(|d| d.get("cache_write_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        reasoning_tokens: usage
            .get("output_tokens_details")
            .and_then(|d| d.get("reasoning_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
    }
}

/// 编码 `usage`。
fn encode_usage(usage: &Usage) -> Value {
    json!({
        "input_tokens": usage.input_tokens,
        "input_tokens_details": {
            "cached_tokens": usage.cached_input_tokens,
            "cache_write_tokens": usage.cache_write_tokens,
        },
        "output_tokens": usage.output_tokens,
        "output_tokens_details": {
            "reasoning_tokens": usage.reasoning_tokens,
        },
        "total_tokens": usage.total(),
    })
}

/// 把 `{error:{...}}` 的内层对象解析成网关错误。
fn decode_error_body(err: &Value) -> GatewayError {
    let message = err
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("upstream returned an error without a message")
        .to_owned();
    let kind = err
        .get("type")
        .and_then(Value::as_str)
        .or_else(|| err.get("code").and_then(Value::as_str))
        .unwrap_or_default();

    let mut out = match kind {
        "invalid_request_error" | "invalid_prompt" => GatewayError::invalid_request(message),
        "authentication_error" => GatewayError::unauthenticated(message),
        "not_found_error" | "model_not_found" => GatewayError::not_found(message),
        "rate_limit_exceeded" | "rate_limit_error" => {
            GatewayError::new(refract_core::ErrorKind::RateLimited, message)
        }
        _ => GatewayError::new(refract_core::ErrorKind::UpstreamError, message),
    };
    out.protocol = Some(Protocol::Responses);
    out.upstream_body = Some(err.to_string());
    out
}

// ===== 流式 =====

impl StreamCodec for ResponsesCodec {
    fn stream_decoder(&self) -> Box<dyn StreamDecoder> {
        Box::new(ResponsesStreamDecoder::default())
    }

    fn stream_encoder(&self) -> Box<dyn StreamEncoder> {
        Box::new(ResponsesStreamEncoder::default())
    }
}

impl ProtocolCodec for ResponsesCodec {
    fn protocol(&self) -> Protocol {
        Protocol::Responses
    }
}

/// 流式解码器（有状态）。
///
/// Responses 用 `output_index` 标识 item，IR 用块 index 标识内容块，两者语义
/// 一致（一个 item 对应一个块），所以直接沿用 `output_index`。
///
/// 宽容策略：中转站常省略 `output_item.added` / `content_part.added` 直接发
/// delta，所以解码器在首次见到某下标的 delta 时**自己补** [`StreamEvent::ContentStart`]。
#[derive(Default)]
struct ResponsesStreamDecoder {
    /// 已经产出过 ContentStart 的块下标（配合 kind 判断是否要补）。
    opened: Vec<Option<PartKind>>,
    /// 是否已经产出过 Start。
    started: bool,
    /// 是否已经产出过 Done，防止 completed + [DONE] 重复终结。
    finished: bool,
}

impl ResponsesStreamDecoder {
    /// 必要时补一个 ContentStart。
    fn ensure_open(&mut self, index: u32, kind: PartKind, out: &mut Vec<StreamEvent>) {
        let idx = index as usize;
        if self.opened.len() <= idx {
            self.opened.resize(idx + 1, None);
        }
        if self.opened[idx] != Some(kind) {
            self.opened[idx] = Some(kind);
            out.push(StreamEvent::ContentStart { index, kind });
        }
    }
}

impl StreamDecoder for ResponsesStreamDecoder {
    fn decode(&mut self, frame: &SseFrame) -> Result<Vec<StreamEvent>, GatewayError> {
        // OpenAI 的 Responses 流不发 [DONE]，但中转站会补，容忍它。
        if frame.data.trim() == "[DONE]" {
            return Ok(if self.finished {
                Vec::new()
            } else {
                self.finished = true;
                vec![StreamEvent::Done]
            });
        }
        if frame.data.trim().is_empty() {
            return Ok(Vec::new());
        }

        // 非 JSON 帧一律忽略，而不是终止整个流。中转站会插入 `ping`、
        // 计数器之类的裸文本心跳；因为一个心跳把用户的回答整个丢掉，
        // 代价远大于「可能漏掉一个我们本就不认识的事件」。
        let Ok(payload) = serde_json::from_str::<Value>(&frame.data) else {
            tracing::debug!(
                bytes = frame.data.len(),
                "responses: ignoring non-JSON SSE frame"
            );
            return Ok(Vec::new());
        };

        // 事件名优先取 SSE 的 `event:`，回落到 payload 的 `type` ——
        // 有些中转站只发其中一个。
        let event = frame
            .event
            .as_deref()
            .or_else(|| payload.get("type").and_then(Value::as_str))
            .unwrap_or_default();

        let mut out = Vec::new();

        match event {
            "response.created" | "response.in_progress" => {
                // in_progress 紧跟 created，重复的 Start 会让聚合器重置，所以只发一次。
                if !self.started {
                    self.started = true;
                    let resp = payload.get("response");
                    out.push(StreamEvent::Start {
                        id: resp
                            .and_then(|r| r.get("id"))
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        model: resp
                            .and_then(|r| r.get("model"))
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        usage: resp
                            .and_then(|r| r.get("usage"))
                            .filter(|u| !u.is_null())
                            .map(decode_usage),
                    });
                }
            }

            "response.output_item.added" => {
                let index = payload.get("output_index").and_then(as_u32).unwrap_or(0);
                let item = payload.get("item");
                let kind = item
                    .and_then(|i| i.get("type"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                match kind {
                    "function_call" => {
                        self.ensure_open(index, PartKind::ToolUse, &mut out);
                        out.push(StreamEvent::ToolCallStart {
                            index,
                            id: item
                                .and_then(|i| i.get("call_id"))
                                .or_else(|| item.and_then(|i| i.get("id")))
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_owned(),
                            name: item
                                .and_then(|i| i.get("name"))
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_owned(),
                            signature: None,
                        });
                    }
                    "reasoning" => self.ensure_open(index, PartKind::Thinking, &mut out),
                    // message item 的具体块种类要等 content_part.added 才知道
                    // （可能是 output_text 也可能是 refusal），这里不预判。
                    _ => {}
                }
            }

            "response.content_part.added" => {
                let index = payload.get("output_index").and_then(as_u32).unwrap_or(0);
                let kind = match payload
                    .get("part")
                    .and_then(|p| p.get("type"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                {
                    "refusal" => PartKind::Refusal,
                    "reasoning_text" | "summary_text" => PartKind::Thinking,
                    _ => PartKind::Text,
                };
                self.ensure_open(index, kind, &mut out);
            }

            "response.output_text.delta" => {
                let index = payload.get("output_index").and_then(as_u32).unwrap_or(0);
                self.ensure_open(index, PartKind::Text, &mut out);
                out.push(StreamEvent::TextDelta {
                    index,
                    text: delta_text(&payload),
                });
            }

            "response.refusal.delta" => {
                let index = payload.get("output_index").and_then(as_u32).unwrap_or(0);
                self.ensure_open(index, PartKind::Refusal, &mut out);
                out.push(StreamEvent::RefusalDelta {
                    index,
                    text: delta_text(&payload),
                });
            }

            // 摘要与原始推理文本在 IR 里都是 Thinking。
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                let index = payload.get("output_index").and_then(as_u32).unwrap_or(0);
                self.ensure_open(index, PartKind::Thinking, &mut out);
                out.push(StreamEvent::ThinkingDelta {
                    index,
                    text: delta_text(&payload),
                });
            }

            "response.function_call_arguments.delta" => {
                let index = payload.get("output_index").and_then(as_u32).unwrap_or(0);
                self.ensure_open(index, PartKind::ToolUse, &mut out);
                out.push(StreamEvent::ToolCallArgsDelta {
                    index,
                    fragment: delta_text(&payload),
                });
            }

            "response.output_item.done" => {
                let index = payload.get("output_index").and_then(as_u32).unwrap_or(0);
                // reasoning item 的 encrypted_content 只在 done 事件里出现，
                // 而它必须无损保留，否则回传给 Anthropic 会被拒。
                if let Some(sig) = payload
                    .get("item")
                    .and_then(|i| i.get("encrypted_content"))
                    .and_then(Value::as_str)
                {
                    self.ensure_open(index, PartKind::Thinking, &mut out);
                    out.push(StreamEvent::ThinkingSignature {
                        index,
                        signature: sig.to_owned(),
                    });
                }
                out.push(StreamEvent::ContentStop { index });
            }

            // content_part.done 不产出 ContentStop —— 那是 output_item.done 的职责，
            // 否则一个块会被关两次。
            "response.content_part.done"
            | "response.output_text.done"
            | "response.refusal.done"
            | "response.function_call_arguments.done"
            | "response.reasoning_summary_text.done"
            | "response.reasoning_text.done"
            | "response.reasoning_summary_part.added"
            | "response.reasoning_summary_part.done"
            | "response.queued" => {}

            "response.completed" | "response.incomplete" => {
                let resp = payload.get("response");
                if let Some(usage) = resp.and_then(|r| r.get("usage")).filter(|u| !u.is_null()) {
                    out.push(StreamEvent::Usage(decode_usage(usage)));
                }
                let status = resp
                    .and_then(|r| r.get("status"))
                    .and_then(Value::as_str)
                    .unwrap_or(if event == "response.incomplete" {
                        "incomplete"
                    } else {
                        "completed"
                    });
                let items = resp
                    .and_then(|r| r.get("output"))
                    .and_then(Value::as_array)
                    .map(|v| v.as_slice())
                    .unwrap_or_default();
                let has_tool_call = items
                    .iter()
                    .any(|i| i.get("type").and_then(Value::as_str) == Some("function_call"));
                let has_refusal = items.iter().any(|i| {
                    i.get("content")
                        .and_then(Value::as_array)
                        .is_some_and(|parts| {
                            parts
                                .iter()
                                .any(|p| p.get("type").and_then(Value::as_str) == Some("refusal"))
                        })
                });
                let reason = map_status(
                    status,
                    resp.and_then(|r| r.get("incomplete_details"))
                        .and_then(|d| d.get("reason"))
                        .and_then(Value::as_str),
                    has_tool_call,
                    has_refusal,
                );
                out.push(StreamEvent::Stop {
                    reason,
                    stop_sequence: None,
                });
                if !self.finished {
                    self.finished = true;
                    out.push(StreamEvent::Done);
                }
            }

            "response.failed" | "error" => {
                // failed 事件的错误在 response.error 里，error 事件的在顶层。
                let err = payload
                    .get("response")
                    .and_then(|r| r.get("error"))
                    .filter(|e| !e.is_null())
                    .unwrap_or(&payload);
                out.push(StreamEvent::Error {
                    message: err
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("upstream stream failed")
                        .to_owned(),
                    kind: err
                        .get("type")
                        .or_else(|| err.get("code"))
                        .and_then(Value::as_str)
                        .unwrap_or("upstream_error")
                        .to_owned(),
                });
                self.finished = true;
            }

            other => {
                tracing::debug!(event = other, "responses: ignoring unknown stream event");
            }
        }

        Ok(out)
    }

    fn finish(&mut self) -> Result<Vec<StreamEvent>, GatewayError> {
        // 上游断流但没发 completed 时，也要让下游收到终结事件。
        if self.finished {
            return Ok(Vec::new());
        }
        self.finished = true;
        Ok(vec![StreamEvent::Done])
    }
}

/// 取 delta 文本。
///
/// 兼容把增量放在 `text` 而非 `delta` 的实现。
fn delta_text(payload: &Value) -> String {
    payload
        .get("delta")
        .or_else(|| payload.get("text"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// 流式编码器（有状态）。
///
/// 本文件最复杂的部分。Responses 客户端（尤其官方 SDK）依赖完整的仪式性事件
/// 序列与**全局递增**的 `sequence_number`，而上游可能是 Anthropic 或 Gemini ——
/// 它们根本没有这些概念。所以这里必须自己造出全套事件：
///
/// ```text
/// response.created → response.in_progress
///   → response.output_item.added → response.content_part.added
///     → response.output_text.delta ...
///   → response.content_part.done → response.output_item.done
/// → response.completed
/// ```
///
/// 三个不变量：
/// 1. `sequence_number` 从 0 开始，每发一帧 +1，永不重复、永不回退。
/// 2. 任何 delta 之前必须已经发过对应的 `output_item.added`（缺 `ContentStart`
///    时由编码器补）。
/// 3. 流终结前所有开着的块都要收尾，且 `response.completed` 必须带完整
///    response 对象与 usage —— 客户端只从这里拿最终结果。
#[derive(Default)]
struct ResponsesStreamEncoder {
    /// 全局事件序号。
    seq: u64,
    /// 响应 ID。上游没给就自己造一个，客户端要用它做关联。
    id: String,
    /// 模型名。
    model: String,
    /// 创建时间。0 表示还没定，由 `ensure_started` 填真实时间戳。
    created: i64,
    /// 开场事件是否已发。
    started: bool,
    /// 终结事件是否已发。
    finished: bool,
    /// IR 块下标 → 本协议 item 状态。
    blocks: BTreeMap<u32, OpenBlock>,
    /// 下一个可用的 `output_index`。
    next_output_index: u32,
    /// 已收尾的 item，按 output_index 排位（供 completed 事件回发）。
    items: Vec<Option<Value>>,
    /// 累积用量。
    usage: Usage,
    /// 停止原因。
    stop_reason: Option<StopReason>,
}

/// 一个正在流式产出的 item。
struct OpenBlock {
    /// 本协议的 item 下标。
    output_index: u32,
    /// item ID。
    item_id: String,
    /// 块种类。
    kind: PartKind,
    /// `output_item.added` 是否已发。
    ///
    /// 工具调用要等 [`StreamEvent::ToolCallStart`] 才知道函数名，所以它的
    /// 声明必须延后 —— 发一个 `name` 为空的 item 会让客户端解析失败。
    announced: bool,
    /// 累积文本（`.done` 事件要回发完整内容）。
    text: String,
    /// 推理签名。
    signature: Option<String>,
    /// 工具调用 ID。
    tool_id: String,
    /// 工具名。
    tool_name: String,
    /// 累积的工具入参片段。
    args: String,
}

impl ResponsesStreamEncoder {
    /// 取下一个序号。
    fn next_seq(&mut self) -> u64 {
        let n = self.seq;
        self.seq += 1;
        n
    }

    /// 发一帧具名事件，自动补 `type` 与 `sequence_number`。
    fn emit(&mut self, event: &str, mut payload: Map<String, Value>, out: &mut Vec<SseFrame>) {
        let seq = self.next_seq();
        payload.insert("type".into(), json!(event));
        payload.insert("sequence_number".into(), json!(seq));
        out.push(SseFrame::named(event, Value::Object(payload).to_string()));
    }

    /// 当前状态对应的 response 对象快照。
    fn snapshot(&self, status: &str, terminal: bool) -> Value {
        let output: Vec<Value> = if terminal {
            self.items.iter().flatten().cloned().collect()
        } else {
            Vec::new()
        };
        let text: String = output
            .iter()
            .filter(|i| i.get("type").and_then(Value::as_str) == Some("message"))
            .filter_map(|i| i.get("content").and_then(Value::as_array))
            .flatten()
            .filter(|p| p.get("type").and_then(Value::as_str) == Some("output_text"))
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect();

        let mut obj = Map::new();
        obj.insert("id".into(), json!(self.id));
        obj.insert("object".into(), json!("response"));
        obj.insert("created_at".into(), json!(self.created));
        obj.insert("model".into(), json!(self.model));
        obj.insert("status".into(), json!(status));
        obj.insert("output".into(), Value::Array(output));
        obj.insert("output_text".into(), json!(text));
        obj.insert("parallel_tool_calls".into(), json!(true));
        obj.insert("tool_choice".into(), json!("auto"));
        obj.insert("tools".into(), json!([]));
        obj.insert("error".into(), Value::Null);
        obj.insert(
            "incomplete_details".into(),
            self.stop_reason
                .and_then(incomplete_details)
                .unwrap_or(Value::Null),
        );
        // usage 只在终态给：非终态给 null 与官方行为一致。
        obj.insert(
            "usage".into(),
            if terminal {
                encode_usage(&self.usage)
            } else {
                Value::Null
            },
        );
        Value::Object(obj)
    }

    /// 补齐开场事件。
    fn ensure_started(&mut self, out: &mut Vec<SseFrame>) {
        if self.started {
            return;
        }
        self.started = true;
        if self.id.is_empty() {
            // 上游（如 Gemini）不给 ID，但客户端需要一个稳定标识。
            self.id = format!("resp_{}", uuid::Uuid::new_v4().simple());
        }
        if self.created == 0 {
            self.created = chrono::Utc::now().timestamp();
        }
        let created = self.snapshot("in_progress", false);
        let mut p = Map::new();
        p.insert("response".into(), created.clone());
        self.emit("response.created", p, out);
        let mut p = Map::new();
        p.insert("response".into(), created);
        self.emit("response.in_progress", p, out);
    }

    /// 确保某个块存在，返回其 `output_index` 与 item ID。
    ///
    /// 上游省略 `ContentStart` 直接发 delta 时，这里负责补出 item。
    fn ensure_block(&mut self, index: u32, kind: PartKind, out: &mut Vec<SseFrame>) {
        self.ensure_started(out);
        if let Some(existing) = self.blocks.get(&index) {
            // 种类冲突说明上游复用了下标，先把旧块收尾再开新的。
            if existing.kind == kind {
                return;
            }
            self.close_block(index, out);
        }
        let output_index = self.next_output_index;
        self.next_output_index += 1;
        let prefix = match kind {
            PartKind::ToolUse => "fc",
            PartKind::Thinking => "rs",
            PartKind::Text | PartKind::Refusal => "msg",
        };
        self.blocks.insert(
            index,
            OpenBlock {
                output_index,
                item_id: format!("{prefix}_{}_{output_index}", self.id),
                kind,
                announced: false,
                text: String::new(),
                signature: None,
                tool_id: String::new(),
                tool_name: String::new(),
                args: String::new(),
            },
        );
        // 工具调用要等函数名，其余立刻声明。
        if kind != PartKind::ToolUse {
            self.announce(index, out);
        }
    }

    /// 发出 `output_item.added`（以及文本类的 `content_part.added`）。
    fn announce(&mut self, index: u32, out: &mut Vec<SseFrame>) {
        let Some(block) = self.blocks.get(&index) else {
            return;
        };
        if block.announced {
            return;
        }
        let (output_index, item_id, kind) = (block.output_index, block.item_id.clone(), block.kind);
        let (tool_id, tool_name) = (block.tool_id.clone(), block.tool_name.clone());
        if let Some(b) = self.blocks.get_mut(&index) {
            b.announced = true;
        }

        let item = match kind {
            PartKind::ToolUse => json!({
                "type": "function_call",
                "id": item_id,
                "call_id": tool_id,
                "name": tool_name,
                "arguments": "",
                "status": "in_progress",
            }),
            PartKind::Thinking => json!({
                "type": "reasoning",
                "id": item_id,
                "summary": [],
            }),
            PartKind::Text | PartKind::Refusal => json!({
                "type": "message",
                "id": item_id,
                "status": "in_progress",
                "role": "assistant",
                "content": [],
            }),
        };
        let mut p = Map::new();
        p.insert("output_index".into(), json!(output_index));
        p.insert("item".into(), item);
        self.emit("response.output_item.added", p, out);

        // 只有 message item 有 content_part；function_call 没有，
        // reasoning 用的是 reasoning_summary_part。
        match kind {
            PartKind::Text | PartKind::Refusal => {
                let part = if kind == PartKind::Refusal {
                    json!({"type": "refusal", "refusal": ""})
                } else {
                    json!({"type": "output_text", "text": "", "annotations": []})
                };
                let mut p = Map::new();
                p.insert("item_id".into(), json!(item_id));
                p.insert("output_index".into(), json!(output_index));
                p.insert("content_index".into(), json!(0));
                p.insert("part".into(), part);
                self.emit("response.content_part.added", p, out);
            }
            PartKind::Thinking => {
                let mut p = Map::new();
                p.insert("item_id".into(), json!(item_id));
                p.insert("output_index".into(), json!(output_index));
                p.insert("summary_index".into(), json!(0));
                p.insert("part".into(), json!({"type": "summary_text", "text": ""}));
                self.emit("response.reasoning_summary_part.added", p, out);
            }
            PartKind::ToolUse => {}
        }
    }

    /// 收尾一个块：补齐 `.done` 系列事件，并把成品 item 存起来。
    fn close_block(&mut self, index: u32, out: &mut Vec<SseFrame>) {
        if !self.blocks.contains_key(&index) {
            return;
        }
        // 从没声明过的块（例如上游只发了 ContentStart 就断了）也要补声明，
        // 否则客户端会收到一个凭空出现的 done 事件。announce 自带幂等。
        self.announce(index, out);
        if let Some(block) = self.blocks.remove(&index) {
            self.finish_block(block, out);
        }
    }

    /// 真正发出收尾事件。
    fn finish_block(&mut self, block: OpenBlock, out: &mut Vec<SseFrame>) {
        let OpenBlock {
            output_index,
            item_id,
            kind,
            text,
            signature,
            tool_id,
            tool_name,
            args,
            ..
        } = block;

        let item = match kind {
            PartKind::Text => {
                let mut p = Map::new();
                p.insert("item_id".into(), json!(item_id));
                p.insert("output_index".into(), json!(output_index));
                p.insert("content_index".into(), json!(0));
                p.insert("text".into(), json!(text));
                p.insert("logprobs".into(), json!([]));
                self.emit("response.output_text.done", p, out);

                let part = json!({"type": "output_text", "text": text, "annotations": []});
                let mut p = Map::new();
                p.insert("item_id".into(), json!(item_id));
                p.insert("output_index".into(), json!(output_index));
                p.insert("content_index".into(), json!(0));
                p.insert("part".into(), part.clone());
                self.emit("response.content_part.done", p, out);

                json!({
                    "type": "message",
                    "id": item_id,
                    "status": "completed",
                    "role": "assistant",
                    "content": [part],
                })
            }
            PartKind::Refusal => {
                let mut p = Map::new();
                p.insert("item_id".into(), json!(item_id));
                p.insert("output_index".into(), json!(output_index));
                p.insert("content_index".into(), json!(0));
                p.insert("refusal".into(), json!(text));
                self.emit("response.refusal.done", p, out);

                let part = json!({"type": "refusal", "refusal": text});
                let mut p = Map::new();
                p.insert("item_id".into(), json!(item_id));
                p.insert("output_index".into(), json!(output_index));
                p.insert("content_index".into(), json!(0));
                p.insert("part".into(), part.clone());
                self.emit("response.content_part.done", p, out);

                json!({
                    "type": "message",
                    "id": item_id,
                    "status": "completed",
                    "role": "assistant",
                    "content": [part],
                })
            }
            PartKind::Thinking => {
                let mut p = Map::new();
                p.insert("item_id".into(), json!(item_id));
                p.insert("output_index".into(), json!(output_index));
                p.insert("summary_index".into(), json!(0));
                p.insert("text".into(), json!(text));
                self.emit("response.reasoning_summary_text.done", p, out);

                let mut p = Map::new();
                p.insert("item_id".into(), json!(item_id));
                p.insert("output_index".into(), json!(output_index));
                p.insert("summary_index".into(), json!(0));
                p.insert("part".into(), json!({"type": "summary_text", "text": text}));
                self.emit("response.reasoning_summary_part.done", p, out);

                let mut item = Map::new();
                item.insert("type".into(), json!("reasoning"));
                item.insert("id".into(), json!(item_id));
                item.insert(
                    "summary".into(),
                    json!([{"type": "summary_text", "text": text}]),
                );
                // signature 无损透传：Anthropic 的推理签名在这里落成
                // encrypted_content，客户端下一轮回传时不会丢。
                if let Some(sig) = signature {
                    item.insert("encrypted_content".into(), json!(sig));
                }
                Value::Object(item)
            }
            PartKind::ToolUse => {
                let mut p = Map::new();
                p.insert("item_id".into(), json!(item_id));
                p.insert("output_index".into(), json!(output_index));
                p.insert("name".into(), json!(tool_name));
                p.insert("arguments".into(), json!(args));
                self.emit("response.function_call_arguments.done", p, out);

                json!({
                    "type": "function_call",
                    "id": item_id,
                    "call_id": tool_id,
                    "name": tool_name,
                    // 入参可能是被截断的片段，原样回发而不是补成合法 JSON。
                    "arguments": args,
                    "status": "completed",
                })
            }
        };

        let mut p = Map::new();
        p.insert("output_index".into(), json!(output_index));
        p.insert("item".into(), item.clone());
        self.emit("response.output_item.done", p, out);

        let slot = output_index as usize;
        if self.items.len() <= slot {
            self.items.resize(slot + 1, None);
        }
        self.items[slot] = Some(item);
    }

    /// 收尾所有仍开着的块。
    fn close_all(&mut self, out: &mut Vec<SseFrame>) {
        let open: Vec<u32> = self.blocks.keys().copied().collect();
        for index in open {
            self.close_block(index, out);
        }
    }

    /// 发出终结事件。
    fn terminate(&mut self, out: &mut Vec<SseFrame>) {
        if self.finished {
            return;
        }
        self.ensure_started(out);
        self.close_all(out);
        self.finished = true;

        let reason = self.stop_reason.unwrap_or(StopReason::Stop);
        let status = status_str(reason);
        let event = match status {
            "incomplete" => "response.incomplete",
            "failed" => "response.failed",
            _ => "response.completed",
        };
        let snapshot = self.snapshot(status, true);
        let mut p = Map::new();
        p.insert("response".into(), snapshot);
        self.emit(event, p, out);
    }

    /// 取每字段最大值合并 usage，语义与 [`StreamAggregator`] 一致。
    fn merge_usage(&mut self, u: Usage) {
        self.usage.merge_max(&u);
    }
}

impl StreamEncoder for ResponsesStreamEncoder {
    fn encode(&mut self, event: &StreamEvent) -> Result<Vec<SseFrame>, GatewayError> {
        let mut out = Vec::new();
        if self.finished {
            // 终结之后再来的事件一律丢弃，否则客户端会看到 completed 之后还有 delta。
            return Ok(out);
        }

        match event {
            StreamEvent::Start { id, model, usage } => {
                if !id.is_empty() {
                    self.id = id.clone();
                }
                if !model.is_empty() {
                    self.model = model.clone();
                }
                if let Some(u) = usage {
                    self.merge_usage(*u);
                }
                self.ensure_started(&mut out);
            }

            StreamEvent::ContentStart { index, kind } => {
                self.ensure_block(*index, *kind, &mut out);
            }

            StreamEvent::TextDelta { index, text } => {
                self.ensure_block(*index, PartKind::Text, &mut out);
                let Some(b) = self.blocks.get_mut(index) else {
                    return Ok(out);
                };
                b.text.push_str(text);
                let (oi, item_id) = (b.output_index, b.item_id.clone());
                let mut p = Map::new();
                p.insert("item_id".into(), json!(item_id));
                p.insert("output_index".into(), json!(oi));
                p.insert("content_index".into(), json!(0));
                p.insert("delta".into(), json!(text));
                p.insert("logprobs".into(), json!([]));
                self.emit("response.output_text.delta", p, &mut out);
            }

            StreamEvent::RefusalDelta { index, text } => {
                self.ensure_block(*index, PartKind::Refusal, &mut out);
                let Some(b) = self.blocks.get_mut(index) else {
                    return Ok(out);
                };
                b.text.push_str(text);
                let (oi, item_id) = (b.output_index, b.item_id.clone());
                let mut p = Map::new();
                p.insert("item_id".into(), json!(item_id));
                p.insert("output_index".into(), json!(oi));
                p.insert("content_index".into(), json!(0));
                p.insert("delta".into(), json!(text));
                self.emit("response.refusal.delta", p, &mut out);
            }

            StreamEvent::ThinkingDelta { index, text } => {
                self.ensure_block(*index, PartKind::Thinking, &mut out);
                let Some(b) = self.blocks.get_mut(index) else {
                    return Ok(out);
                };
                b.text.push_str(text);
                let (oi, item_id) = (b.output_index, b.item_id.clone());
                let mut p = Map::new();
                p.insert("item_id".into(), json!(item_id));
                p.insert("output_index".into(), json!(oi));
                p.insert("summary_index".into(), json!(0));
                p.insert("delta".into(), json!(text));
                self.emit("response.reasoning_summary_text.delta", p, &mut out);
            }

            // 签名没有对应的流式事件，只能攒着，等 item 收尾时写进
            // encrypted_content —— 丢了它 Anthropic 下一轮会拒绝整个请求。
            StreamEvent::ThinkingSignature { index, signature } => {
                self.ensure_block(*index, PartKind::Thinking, &mut out);
                if let Some(b) = self.blocks.get_mut(index) {
                    b.signature = Some(signature.clone());
                }
            }

            StreamEvent::ToolCallStart {
                index, id, name, ..
            } => {
                self.ensure_block(*index, PartKind::ToolUse, &mut out);
                if let Some(b) = self.blocks.get_mut(index) {
                    if !id.is_empty() {
                        b.tool_id = id.clone();
                    }
                    if !name.is_empty() {
                        b.tool_name = name.clone();
                    }
                }
                // 名字到手，现在才能声明这个 item。
                self.announce(*index, &mut out);
            }

            StreamEvent::ToolCallArgsDelta { index, fragment } => {
                self.ensure_block(*index, PartKind::ToolUse, &mut out);
                // 上游可能跳过 ToolCallStart，这里兜底声明。
                self.announce(*index, &mut out);
                let Some(b) = self.blocks.get_mut(index) else {
                    return Ok(out);
                };
                b.args.push_str(fragment);
                let (oi, item_id) = (b.output_index, b.item_id.clone());
                let mut p = Map::new();
                p.insert("item_id".into(), json!(item_id));
                p.insert("output_index".into(), json!(oi));
                p.insert("delta".into(), json!(fragment));
                self.emit("response.function_call_arguments.delta", p, &mut out);
            }

            StreamEvent::ContentStop { index } => {
                if self.blocks.contains_key(index) {
                    self.close_block(*index, &mut out);
                }
            }

            StreamEvent::Usage(u) => self.merge_usage(*u),

            StreamEvent::Stop { reason, .. } => {
                // 只记录，不终结：usage 常常在 Stop 之后才到。
                self.stop_reason = Some(*reason);
            }

            StreamEvent::Done => self.terminate(&mut out),

            StreamEvent::Error { message, kind } => {
                self.ensure_started(&mut out);
                let mut p = Map::new();
                p.insert("code".into(), json!(kind));
                p.insert("message".into(), json!(message));
                p.insert("param".into(), Value::Null);
                self.emit("error", p, &mut out);
                self.finished = true;
            }

            // Responses 没有心跳事件，丢弃。SSE 注释行由传输层负责保活。
            StreamEvent::Ping => {}
        }

        Ok(out)
    }

    fn finish(&mut self) -> Result<Vec<SseFrame>, GatewayError> {
        // 上游断流没发 Done 时，也要给客户端一个完整的收尾。
        let mut out = Vec::new();
        if !self.finished && self.started {
            self.terminate(&mut out);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    /// 解析 SSE 帧的 data 为 JSON。
    fn payload(frame: &SseFrame) -> Value {
        serde_json::from_str(&frame.data).expect("frame data must be valid JSON")
    }

    /// 收集帧的事件名序列。
    fn event_names(frames: &[SseFrame]) -> Vec<String> {
        frames
            .iter()
            .map(|f| f.event.clone().unwrap_or_default())
            .collect()
    }

    /// 把一串统一事件喂给编码器，返回所有帧。
    fn encode_all(events: &[StreamEvent]) -> Vec<SseFrame> {
        let mut enc = ResponsesStreamEncoder::default();
        let mut frames = Vec::new();
        for ev in events {
            frames.extend(enc.encode(ev).expect("encode"));
        }
        frames.extend(enc.finish().expect("finish"));
        frames
    }

    /// 构造一个具名 SSE 帧。
    fn frame(event: &str, data: Value) -> SseFrame {
        SseFrame::named(event, data.to_string())
    }

    // ===== 请求 =====

    #[test]
    fn decodes_string_input_as_single_user_message() {
        let raw = json!({"model": "gpt-5", "input": "hello"});
        let ir = RESPONSES.decode_request(&raw).expect("decode");
        assert_eq!(ir.model, "gpt-5");
        assert_eq!(ir.messages, vec![Message::text(Role::User, "hello")]);
        assert!(!ir.stream);
    }

    #[test]
    fn decode_request_rejects_missing_model() {
        let err = RESPONSES
            .decode_request(&json!({"input": "hi"}))
            .expect_err("must reject");
        assert_eq!(err.kind, refract_core::ErrorKind::InvalidRequest);
        assert!(
            err.message.contains("model"),
            "message must name the field: {}",
            err.message
        );
    }

    #[test]
    fn request_roundtrip_preserves_sampling_and_instructions() {
        let raw = json!({
            "model": "gpt-5",
            "instructions": "be terse",
            "input": [{"type": "message", "role": "user", "content": [
                {"type": "input_text", "text": "hi"}
            ]}],
            "max_output_tokens": 256,
            "temperature": 0.4,
            "top_p": 0.9,
            "stream": true,
            "user": "u-1",
        });
        let ir = RESPONSES.decode_request(&raw).expect("decode");
        assert_eq!(ir.system_text(), "be terse");
        assert_eq!(ir.max_output_tokens, Some(256));
        assert_eq!(ir.sampling.temperature, Some(0.4));
        assert_eq!(ir.sampling.top_p, Some(0.9));
        assert!(ir.stream);

        let out = RESPONSES.encode_request(&ir).expect("encode");
        assert_eq!(out["instructions"], json!("be terse"));
        assert_eq!(out["max_output_tokens"], json!(256));
        assert_eq!(out["temperature"], json!(0.4));
        assert_eq!(out["user"], json!("u-1"));
        assert_eq!(
            out["input"],
            json!([{"type": "message", "role": "user",
                    "content": [{"type": "input_text", "text": "hi"}]}])
        );
    }

    #[test]
    fn decodes_multimodal_input_with_data_uri_and_file_id() {
        let raw = json!({
            "model": "gpt-5",
            "input": [{"type": "message", "role": "user", "content": [
                {"type": "input_text", "text": "what is this"},
                {"type": "input_image", "image_url": "data:image/png;base64,AAAB", "detail": "high"},
                {"type": "input_image", "file_id": "file-42"},
                {"type": "input_file", "file_id": "file-99", "filename": "a.pdf"},
            ]}],
        });
        let ir = RESPONSES.decode_request(&raw).expect("decode");
        assert_eq!(
            ir.messages[0].content,
            vec![
                ContentPart::text("what is this"),
                ContentPart::Image {
                    source: MediaSource::Base64("AAAB".into()),
                    mime: Some("image/png".into()),
                    detail: Some("high".into()),
                },
                ContentPart::Image {
                    source: MediaSource::FileId("file-42".into()),
                    mime: None,
                    detail: None,
                },
                ContentPart::File {
                    source: MediaSource::FileId("file-99".into()),
                    mime: None,
                    name: Some("a.pdf".into()),
                },
            ]
        );

        // 编码时 base64 图片必须还原成 data URI，否则上游收不到图。
        let out = RESPONSES.encode_request(&ir).expect("encode");
        let parts = &out["input"][0]["content"];
        assert_eq!(parts[1]["image_url"], json!("data:image/png;base64,AAAB"));
        assert_eq!(parts[1]["detail"], json!("high"));
        assert_eq!(parts[2]["file_id"], json!("file-42"));
    }

    #[test]
    fn decodes_tool_call_three_phase_conversation() {
        // 声明 → 调用 → 回传，这是最容易在协议转换里丢信息的路径。
        let raw = json!({
            "model": "gpt-5",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "weather?"}]},
                {"type": "function_call", "call_id": "call_1", "name": "get_weather",
                 "arguments": "{\"city\":\"Tokyo\"}"},
                {"type": "function_call_output", "call_id": "call_1", "output": "22C"},
            ],
            "tools": [{"type": "function", "name": "get_weather", "description": "look up",
                       "parameters": {"type": "object", "properties": {"city": {"type": "string"}}},
                       "strict": true}],
            "tool_choice": {"type": "function", "name": "get_weather"},
        });
        let ir = RESPONSES.decode_request(&raw).expect("decode");

        assert_eq!(ir.tools.len(), 1);
        assert_eq!(ir.tools[0].name, "get_weather");
        assert_eq!(ir.tools[0].strict, Some(true));
        assert_eq!(ir.tool_choice, ToolChoice::Tool("get_weather".into()));

        assert_eq!(ir.messages.len(), 3);
        assert_eq!(ir.messages[1].role, Role::Assistant);
        assert_eq!(
            ir.messages[1].content,
            vec![ContentPart::ToolUse {
                signature: None,
                id: "call_1".into(),
                name: "get_weather".into(),
                input: json!({"city": "Tokyo"}),
            }]
        );
        assert_eq!(ir.messages[2].role, Role::Tool);
        assert_eq!(
            ir.messages[2].content,
            vec![ContentPart::ToolResult {
                name: None,
                id: "call_1".into(),
                content: vec![ContentPart::text("22C")],
                is_error: false,
            }]
        );

        // 反向：ToolUse / ToolResult 必须拆回独立顶层 item，工具声明必须是扁平结构。
        let out = RESPONSES.encode_request(&ir).expect("encode");
        let items = out["input"].as_array().expect("array");
        assert_eq!(items.len(), 3);
        assert_eq!(items[1]["type"], json!("function_call"));
        assert_eq!(items[1]["call_id"], json!("call_1"));
        assert_eq!(items[1]["arguments"], json!(r#"{"city":"Tokyo"}"#));
        assert_eq!(items[2]["type"], json!("function_call_output"));
        assert_eq!(items[2]["output"], json!("22C"));
        assert_eq!(out["tools"][0]["name"], json!("get_weather"));
        assert!(
            out["tools"][0].get("function").is_none(),
            "Responses tools are flat, not nested under `function`"
        );
        assert_eq!(
            out["tool_choice"],
            json!({"type": "function", "name": "get_weather"})
        );
    }

    #[test]
    fn preserves_reasoning_encrypted_content_as_signature() {
        // 硬性要求：不透明推理凭据必须无损往返。
        let raw = json!({
            "model": "gpt-5",
            "input": [
                {"type": "reasoning", "id": "rs_1",
                 "summary": [{"type": "summary_text", "text": "think"}],
                 "encrypted_content": "OPAQUE=="},
            ],
        });
        let ir = RESPONSES.decode_request(&raw).expect("decode");
        assert_eq!(
            ir.messages[0].content,
            vec![ContentPart::Thinking {
                text: "think".into(),
                signature: Some("OPAQUE==".into()),
            }]
        );

        let out = RESPONSES.encode_request(&ir).expect("encode");
        assert_eq!(out["input"][0]["type"], json!("reasoning"));
        assert_eq!(out["input"][0]["encrypted_content"], json!("OPAQUE=="));
        assert_eq!(out["input"][0]["summary"][0]["text"], json!("think"));
    }

    #[test]
    fn stateful_fields_go_to_extensions_and_come_back() {
        let raw = json!({
            "model": "gpt-5",
            "input": "hi",
            "previous_response_id": "resp_prev",
            "store": false,
            "include": ["reasoning.encrypted_content"],
        });
        let ir = RESPONSES.decode_request(&raw).expect("decode");
        assert_eq!(
            ir.extension("responses.previous_response_id"),
            Some(&json!("resp_prev"))
        );
        assert_eq!(ir.extension("responses.store"), Some(&json!(false)));

        let out = RESPONSES.encode_request(&ir).expect("encode");
        assert_eq!(out["previous_response_id"], json!("resp_prev"));
        assert_eq!(out["store"], json!(false));
        assert_eq!(out["include"], json!(["reasoning.encrypted_content"]));
    }

    #[test]
    fn unknown_request_fields_land_in_extensions() {
        let raw = json!({"model": "gpt-5", "input": "hi", "truncation": "auto", "top_logprobs": 3});
        let ir = RESPONSES.decode_request(&raw).expect("decode");
        assert_eq!(ir.extension("responses.truncation"), Some(&json!("auto")));
        assert_eq!(ir.extension("responses.top_logprobs"), Some(&json!(3)));
        // 且必须能还原回去，透传不丢字段。
        let out = RESPONSES.encode_request(&ir).expect("encode");
        assert_eq!(out["truncation"], json!("auto"));
    }

    #[test]
    fn decodes_text_format_and_reasoning_config() {
        let raw = json!({
            "model": "gpt-5",
            "input": "hi",
            "text": {"format": {"type": "json_schema", "name": "out",
                                "schema": {"type": "object"}, "strict": true}},
            "reasoning": {"effort": "high", "summary": "auto"},
        });
        let ir = RESPONSES.decode_request(&raw).expect("decode");
        assert_eq!(
            ir.response_format,
            Some(ResponseFormat::JsonSchema {
                name: "out".into(),
                schema: json!({"type": "object"}),
                strict: true,
            })
        );
        assert_eq!(
            ir.reasoning,
            Some(ReasoningConfig {
                effort: Some("high".into()),
                budget_tokens: None,
                include_thoughts: Some(true),
            })
        );

        let out = RESPONSES.encode_request(&ir).expect("encode");
        assert_eq!(out["text"]["format"]["type"], json!("json_schema"));
        assert_eq!(out["text"]["format"]["name"], json!("out"));
        assert_eq!(out["reasoning"]["effort"], json!("high"));
        assert_eq!(out["reasoning"]["summary"], json!("auto"));
    }

    #[test]
    fn encodes_budget_only_reasoning_as_effort_tier() {
        // 从 Anthropic 过来只有 budget_tokens；不折算成档位思考就被静默关掉。
        let mut ir = UnifiedRequest::new("gpt-5", vec![Message::text(Role::User, "hi")]);
        ir.max_output_tokens = Some(10_000);
        ir.reasoning = Some(ReasoningConfig {
            effort: None,
            budget_tokens: Some(8_000),
            include_thoughts: None,
        });
        let out = RESPONSES.encode_request(&ir).expect("encode");
        assert_eq!(out["reasoning"]["effort"], json!("high"));
    }

    #[test]
    fn tool_choice_string_forms_roundtrip() {
        for (raw, expected) in [
            (json!("none"), ToolChoice::None),
            (json!("auto"), ToolChoice::Auto),
            (json!("required"), ToolChoice::Required),
        ] {
            let ir = RESPONSES
                .decode_request(&json!({"model": "m", "input": "x", "tool_choice": raw.clone()}))
                .expect("decode");
            assert_eq!(ir.tool_choice, expected, "for {raw}");
            let out = RESPONSES.encode_request(&ir).expect("encode");
            assert_eq!(out["tool_choice"], raw);
        }
    }

    #[test]
    fn decode_request_tolerates_empty_and_unknown_items() {
        // 空 content 不能让请求失败；内置工具 item（web_search_call 等）
        // 包成 Opaque，responses→responses 直通时原样还原。
        let ws_item = json!({"type": "web_search_call", "id": "ws_1", "status": "completed"});
        let raw = json!({
            "model": "gpt-5",
            "input": [
                {"type": "message", "role": "user", "content": []},
                ws_item.clone(),
            ],
        });
        let ir = RESPONSES.decode_request(&raw).expect("decode");
        assert_eq!(ir.messages.len(), 2);
        assert!(ir.messages[0].content.is_empty());
        assert_eq!(
            ir.messages[1].content[0],
            ContentPart::Opaque {
                protocol: "responses".into(),
                value: ws_item.clone(),
            }
        );

        // 空 content 的消息不产出 item；Opaque item 原样还原。
        let out = RESPONSES.encode_request(&ir).expect("encode");
        assert_eq!(out["input"], json!([ws_item]));
    }

    #[test]
    fn assistant_text_encodes_as_output_text() {
        // 方向搞错上游会拒收：assistant 必须是 output_text 且带 annotations。
        let ir = UnifiedRequest::new(
            "gpt-5",
            vec![
                Message::text(Role::User, "hi"),
                Message::text(Role::Assistant, "hello"),
            ],
        );
        let out = RESPONSES.encode_request(&ir).expect("encode");
        assert_eq!(out["input"][0]["content"][0]["type"], json!("input_text"));
        assert_eq!(
            out["input"][1]["content"][0],
            json!({"type": "output_text", "text": "hello", "annotations": []})
        );
    }

    // ===== 响应 =====

    #[test]
    fn decodes_response_with_text_and_usage() {
        let raw = json!({
            "id": "resp_1",
            "object": "response",
            "created_at": 1_760_000_000_i64,
            "model": "gpt-5",
            "status": "completed",
            "output": [{"type": "message", "id": "msg_1", "role": "assistant", "status": "completed",
                        "content": [{"type": "output_text", "text": "hi there", "annotations": []}]}],
            "usage": {"input_tokens": 12, "output_tokens": 5, "total_tokens": 17,
                      "input_tokens_details": {"cached_tokens": 4, "cache_write_tokens": 2},
                      "output_tokens_details": {"reasoning_tokens": 3}},
        });
        let ir = RESPONSES.decode_response(&raw).expect("decode");
        assert_eq!(ir.id, "resp_1");
        assert_eq!(ir.created, 1_760_000_000);
        assert_eq!(ir.text(), "hi there");
        assert_eq!(ir.stop_reason, Some(StopReason::Stop));
        assert_eq!(
            ir.usage,
            Usage {
                input_tokens: 12,
                output_tokens: 5,
                cached_input_tokens: 4,
                cache_write_tokens: 2,
                reasoning_tokens: 3,
            }
        );

        let out = RESPONSES.encode_response(&ir).expect("encode");
        assert_eq!(out["object"], json!("response"));
        assert_eq!(out["status"], json!("completed"));
        assert_eq!(out["output_text"], json!("hi there"));
        assert_eq!(out["usage"]["total_tokens"], json!(17));
        assert_eq!(
            out["usage"]["input_tokens_details"]["cached_tokens"],
            json!(4)
        );
        assert_eq!(
            out["usage"]["output_tokens_details"]["reasoning_tokens"],
            json!(3)
        );
    }

    #[test]
    fn maps_every_status_and_stop_reason() {
        // status → StopReason
        assert_eq!(
            map_status("completed", None, false, false),
            StopReason::Stop
        );
        assert_eq!(
            map_status("completed", None, true, false),
            StopReason::ToolUse
        );
        assert_eq!(
            map_status("completed", None, false, true),
            StopReason::Refusal
        );
        assert_eq!(
            map_status("incomplete", Some("max_output_tokens"), false, false),
            StopReason::MaxTokens
        );
        assert_eq!(
            map_status("incomplete", Some("content_filter"), false, false),
            StopReason::ContentFilter
        );
        assert_eq!(map_status("failed", None, false, false), StopReason::Other);
        assert_eq!(
            map_status("cancelled", None, false, false),
            StopReason::Other
        );

        // StopReason → status，覆盖全枚举。
        assert_eq!(status_str(StopReason::Stop), "completed");
        assert_eq!(status_str(StopReason::StopSequence), "completed");
        assert_eq!(status_str(StopReason::ToolUse), "completed");
        assert_eq!(status_str(StopReason::Refusal), "completed");
        assert_eq!(status_str(StopReason::PauseTurn), "completed");
        assert_eq!(status_str(StopReason::MaxTokens), "incomplete");
        assert_eq!(status_str(StopReason::ContentFilter), "incomplete");
        // Other 是其他协议的未知停止原因，带着正常内容编成 failed 会让
        // 客户端把好响应当错误丢弃。
        assert_eq!(status_str(StopReason::Other), "completed");
    }

    #[test]
    fn incomplete_response_maps_to_max_tokens_and_back() {
        let raw = json!({
            "id": "resp_2", "model": "gpt-5", "status": "incomplete",
            "incomplete_details": {"reason": "max_output_tokens"},
            "output": [],
        });
        let ir = RESPONSES.decode_response(&raw).expect("decode");
        assert_eq!(ir.stop_reason, Some(StopReason::MaxTokens));

        let out = RESPONSES.encode_response(&ir).expect("encode");
        assert_eq!(out["status"], json!("incomplete"));
        assert_eq!(
            out["incomplete_details"],
            json!({"reason": "max_output_tokens"})
        );
    }

    #[test]
    fn decodes_function_call_output_items_and_reasoning() {
        let raw = json!({
            "id": "resp_3", "model": "gpt-5", "status": "completed",
            "output": [
                {"type": "reasoning", "id": "rs_1",
                 "summary": [{"type": "summary_text", "text": "ponder"}],
                 "encrypted_content": "SIG=="},
                {"type": "function_call", "id": "fc_1", "call_id": "call_9",
                 "name": "lookup", "arguments": "{\"q\":1}", "status": "completed"},
            ],
        });
        let ir = RESPONSES.decode_response(&raw).expect("decode");
        assert_eq!(
            ir.content,
            vec![
                ContentPart::Thinking {
                    text: "ponder".into(),
                    signature: Some("SIG==".into()),
                },
                ContentPart::ToolUse {
                    signature: None,
                    id: "call_9".into(),
                    name: "lookup".into(),
                    input: json!({"q": 1}),
                },
            ]
        );
        // 有 function_call → ToolUse。
        assert_eq!(ir.stop_reason, Some(StopReason::ToolUse));

        let out = RESPONSES.encode_response(&ir).expect("encode");
        assert_eq!(out["output"][0]["encrypted_content"], json!("SIG=="));
        assert_eq!(out["output"][1]["call_id"], json!("call_9"));
        assert_eq!(out["output"][1]["arguments"], json!(r#"{"q":1}"#));
    }

    #[test]
    fn decode_response_surfaces_error_body() {
        let raw = json!({"error": {"message": "bad tool schema",
                                   "type": "invalid_request_error",
                                   "param": "tools[0]", "code": null}});
        let err = RESPONSES.decode_response(&raw).expect_err("must fail");
        assert_eq!(err.kind, refract_core::ErrorKind::InvalidRequest);
        assert_eq!(err.message, "bad tool schema");
        assert_eq!(err.protocol, Some(Protocol::Responses));

        let rate = RESPONSES
            .decode_response(&json!({"error": {"message": "slow down",
                                               "type": "rate_limit_exceeded"}}))
            .expect_err("must fail");
        assert_eq!(rate.kind, refract_core::ErrorKind::RateLimited);
    }

    #[test]
    fn truncated_tool_arguments_survive_as_raw_string() {
        // 流被截断时入参不是合法 JSON，不能丢也不能 panic。
        let raw = json!({
            "id": "r", "model": "m", "status": "completed",
            "output": [{"type": "function_call", "call_id": "c1", "name": "f",
                        "arguments": "{\"partial\":"}],
        });
        let ir = RESPONSES.decode_response(&raw).expect("decode");
        match &ir.content[0] {
            ContentPart::ToolUse { input, .. } => {
                assert_eq!(input, &json!(r#"{"partial":"#));
            }
            other => panic!("unexpected {other:?}"),
        }
        // 回编码时原样吐回去，不能变成 "\"{\\\"partial\\\":\"" 这种双重转义。
        let out = RESPONSES.encode_response(&ir).expect("encode");
        assert_eq!(out["output"][0]["arguments"], json!(r#"{"partial":"#));
    }

    // ===== 流式解码 =====

    #[test]
    fn decodes_full_stream_event_sequence() {
        let mut dec = ResponsesStreamDecoder::default();
        let mut events = Vec::new();

        for f in [
            frame(
                "response.created",
                json!({"type": "response.created", "sequence_number": 0,
                       "response": {"id": "resp_s", "model": "gpt-5", "status": "in_progress"}}),
            ),
            frame(
                "response.in_progress",
                json!({"type": "response.in_progress", "sequence_number": 1,
                       "response": {"id": "resp_s", "model": "gpt-5"}}),
            ),
            frame(
                "response.output_item.added",
                json!({"type": "response.output_item.added", "sequence_number": 2,
                       "output_index": 0,
                       "item": {"type": "message", "id": "msg_1", "role": "assistant"}}),
            ),
            frame(
                "response.content_part.added",
                json!({"type": "response.content_part.added", "sequence_number": 3,
                       "item_id": "msg_1", "output_index": 0, "content_index": 0,
                       "part": {"type": "output_text", "text": ""}}),
            ),
            frame(
                "response.output_text.delta",
                json!({"type": "response.output_text.delta", "sequence_number": 4,
                       "item_id": "msg_1", "output_index": 0, "content_index": 0,
                       "delta": "Hel"}),
            ),
            frame(
                "response.output_text.delta",
                json!({"type": "response.output_text.delta", "sequence_number": 5,
                       "item_id": "msg_1", "output_index": 0, "content_index": 0,
                       "delta": "lo"}),
            ),
            frame(
                "response.content_part.done",
                json!({"type": "response.content_part.done", "sequence_number": 6,
                       "item_id": "msg_1", "output_index": 0, "content_index": 0,
                       "part": {"type": "output_text", "text": "Hello"}}),
            ),
            frame(
                "response.output_item.done",
                json!({"type": "response.output_item.done", "sequence_number": 7,
                       "output_index": 0,
                       "item": {"type": "message", "id": "msg_1", "status": "completed"}}),
            ),
            frame(
                "response.completed",
                json!({"type": "response.completed", "sequence_number": 8,
                       "response": {"id": "resp_s", "model": "gpt-5", "status": "completed",
                                    "output": [],
                                    "usage": {"input_tokens": 7, "output_tokens": 2,
                                              "total_tokens": 9}}}),
            ),
        ] {
            events.extend(dec.decode(&f).expect("decode"));
        }

        assert_eq!(
            events,
            vec![
                StreamEvent::Start {
                    id: "resp_s".into(),
                    model: "gpt-5".into(),
                    usage: None,
                },
                StreamEvent::ContentStart {
                    index: 0,
                    kind: PartKind::Text,
                },
                StreamEvent::TextDelta {
                    index: 0,
                    text: "Hel".into(),
                },
                StreamEvent::TextDelta {
                    index: 0,
                    text: "lo".into(),
                },
                StreamEvent::ContentStop { index: 0 },
                StreamEvent::Usage(Usage {
                    input_tokens: 7,
                    output_tokens: 2,
                    ..Default::default()
                }),
                StreamEvent::Stop {
                    reason: StopReason::Stop,
                    stop_sequence: None,
                },
                StreamEvent::Done,
            ]
        );
    }

    #[test]
    fn decoder_synthesizes_content_start_when_upstream_omits_it() {
        // 中转站常直接发 delta，缺仪式性事件不能报错。
        let mut dec = ResponsesStreamDecoder::default();
        let events = dec
            .decode(&frame(
                "response.output_text.delta",
                json!({"output_index": 0, "delta": "bare"}),
            ))
            .expect("decode");
        assert_eq!(
            events,
            vec![
                StreamEvent::ContentStart {
                    index: 0,
                    kind: PartKind::Text,
                },
                StreamEvent::TextDelta {
                    index: 0,
                    text: "bare".into(),
                },
            ]
        );
        // 第二个 delta 不再重复补 ContentStart。
        let again = dec
            .decode(&frame(
                "response.output_text.delta",
                json!({"output_index": 0, "delta": "!"}),
            ))
            .expect("decode");
        assert_eq!(
            again,
            vec![StreamEvent::TextDelta {
                index: 0,
                text: "!".into(),
            }]
        );
    }

    #[test]
    fn decodes_streamed_tool_call_and_reasoning_signature() {
        let mut dec = ResponsesStreamDecoder::default();
        let mut events = Vec::new();
        for f in [
            frame(
                "response.output_item.added",
                json!({"output_index": 0,
                       "item": {"type": "function_call", "id": "fc_1", "call_id": "call_1",
                                "name": "get_weather", "arguments": ""}}),
            ),
            frame(
                "response.function_call_arguments.delta",
                json!({"item_id": "fc_1", "output_index": 0, "delta": "{\"city\""}),
            ),
            frame(
                "response.function_call_arguments.delta",
                json!({"item_id": "fc_1", "output_index": 0, "delta": ":\"Tokyo\"}"}),
            ),
            frame(
                "response.output_item.done",
                json!({"output_index": 1,
                       "item": {"type": "reasoning", "id": "rs_1",
                                "encrypted_content": "SIG=="}}),
            ),
        ] {
            events.extend(dec.decode(&f).expect("decode"));
        }

        assert_eq!(
            events,
            vec![
                StreamEvent::ContentStart {
                    index: 0,
                    kind: PartKind::ToolUse,
                },
                StreamEvent::ToolCallStart {
                    signature: None,
                    index: 0,
                    id: "call_1".into(),
                    name: "get_weather".into(),
                },
                StreamEvent::ToolCallArgsDelta {
                    index: 0,
                    fragment: "{\"city\"".into(),
                },
                StreamEvent::ToolCallArgsDelta {
                    index: 0,
                    fragment: ":\"Tokyo\"}".into(),
                },
                // signature 只在 item.done 里出现，必须捞出来。
                StreamEvent::ContentStart {
                    index: 1,
                    kind: PartKind::Thinking,
                },
                StreamEvent::ThinkingSignature {
                    index: 1,
                    signature: "SIG==".into(),
                },
                StreamEvent::ContentStop { index: 1 },
            ]
        );
    }

    #[test]
    fn decodes_reasoning_delta_and_error_events() {
        let mut dec = ResponsesStreamDecoder::default();
        let think = dec
            .decode(&frame(
                "response.reasoning_summary_text.delta",
                json!({"item_id": "rs_1", "output_index": 0, "summary_index": 0,
                       "delta": "hmm"}),
            ))
            .expect("decode");
        assert_eq!(
            think,
            vec![
                StreamEvent::ContentStart {
                    index: 0,
                    kind: PartKind::Thinking,
                },
                StreamEvent::ThinkingDelta {
                    index: 0,
                    text: "hmm".into(),
                },
            ]
        );

        let failed = dec
            .decode(&frame(
                "response.failed",
                json!({"response": {"status": "failed",
                                    "error": {"code": "server_error", "message": "boom"}}}),
            ))
            .expect("decode");
        assert_eq!(
            failed,
            vec![StreamEvent::Error {
                message: "boom".into(),
                kind: "server_error".into(),
            }]
        );
    }

    #[test]
    fn decoder_maps_incomplete_stream_to_max_tokens() {
        let mut dec = ResponsesStreamDecoder::default();
        let events = dec
            .decode(&frame(
                "response.incomplete",
                json!({"response": {"status": "incomplete",
                                    "incomplete_details": {"reason": "max_output_tokens"},
                                    "output": []}}),
            ))
            .expect("decode");
        assert!(events.contains(&StreamEvent::Stop {
            reason: StopReason::MaxTokens,
            stop_sequence: None,
        }));
        assert!(events.contains(&StreamEvent::Done));
        // finish 不再重复发 Done。
        assert!(dec.finish().expect("finish").is_empty());
    }

    #[test]
    fn decoder_finish_emits_done_when_upstream_truncates() {
        let mut dec = ResponsesStreamDecoder::default();
        dec.decode(&frame("response.output_text.delta", json!({"delta": "x"})))
            .expect("decode");
        assert_eq!(dec.finish().expect("finish"), vec![StreamEvent::Done]);
    }

    #[test]
    fn decoder_ignores_malformed_payload_instead_of_killing_the_stream() {
        // 中转站会插入裸文本心跳。为一帧垃圾丢掉整个回答是最糟的失败模式，
        // 所以这里必须是「跳过」而不是「报错」。
        let mut dec = ResponsesStreamDecoder::default();
        assert!(
            dec.decode(&SseFrame::named("response.created", "{not json"))
                .expect("junk frames must not fail the stream")
                .is_empty()
        );

        // 垃圾帧之后仍能正常解析真实事件。
        let events = dec
            .decode(&SseFrame::named(
                "response.output_text.delta",
                r#"{"type":"response.output_text.delta","sequence_number":1,"item_id":"m","output_index":0,"content_index":0,"delta":"hi"}"#,
            ))
            .expect("decode after junk");
        assert!(
            events
                .iter()
                .any(|e| matches!(e, StreamEvent::TextDelta { text, .. } if text == "hi"))
        );
    }

    // ===== 流式编码 =====

    #[test]
    fn encoder_emits_complete_ceremonial_sequence_for_text() {
        let frames = encode_all(&[
            StreamEvent::Start {
                id: "resp_e".into(),
                model: "gpt-5".into(),
                usage: None,
            },
            StreamEvent::ContentStart {
                index: 0,
                kind: PartKind::Text,
            },
            StreamEvent::TextDelta {
                index: 0,
                text: "Hi".into(),
            },
            StreamEvent::ContentStop { index: 0 },
            StreamEvent::Usage(Usage {
                input_tokens: 3,
                output_tokens: 1,
                ..Default::default()
            }),
            StreamEvent::Stop {
                reason: StopReason::Stop,
                stop_sequence: None,
            },
            StreamEvent::Done,
        ]);

        assert_eq!(
            event_names(&frames),
            vec![
                "response.created",
                "response.in_progress",
                "response.output_item.added",
                "response.content_part.added",
                "response.output_text.delta",
                "response.output_text.done",
                "response.content_part.done",
                "response.output_item.done",
                "response.completed",
            ]
        );

        // completed 必须带完整 response 对象与 usage —— 客户端只从这里取最终结果。
        let last = payload(frames.last().expect("frames"));
        assert_eq!(last["response"]["status"], json!("completed"));
        assert_eq!(last["response"]["output_text"], json!("Hi"));
        assert_eq!(last["response"]["usage"]["input_tokens"], json!(3));
        assert_eq!(last["response"]["usage"]["total_tokens"], json!(4));
        assert_eq!(
            last["response"]["output"][0]["content"][0],
            json!({"type": "output_text", "text": "Hi", "annotations": []})
        );
    }

    #[test]
    fn encoder_sequence_numbers_are_gapless_and_monotonic() {
        let frames = encode_all(&[
            StreamEvent::Start {
                id: "resp_seq".into(),
                model: "m".into(),
                usage: None,
            },
            StreamEvent::TextDelta {
                index: 0,
                text: "a".into(),
            },
            StreamEvent::TextDelta {
                index: 0,
                text: "b".into(),
            },
            StreamEvent::ToolCallStart {
                signature: None,
                index: 1,
                id: "call_1".into(),
                name: "f".into(),
            },
            StreamEvent::ToolCallArgsDelta {
                index: 1,
                fragment: "{}".into(),
            },
            StreamEvent::Stop {
                reason: StopReason::ToolUse,
                stop_sequence: None,
            },
            StreamEvent::Done,
        ]);

        let seqs: Vec<u64> = frames
            .iter()
            .map(|f| payload(f)["sequence_number"].as_u64().expect("seq"))
            .collect();
        let expected: Vec<u64> = (0..frames.len() as u64).collect();
        assert_eq!(
            seqs, expected,
            "sequence_number must start at 0 and never skip"
        );

        // 每一帧的 type 必须与 SSE event 名一致，SDK 两边都会校验。
        for f in &frames {
            assert_eq!(
                payload(f)["type"].as_str(),
                f.event.as_deref(),
                "type/event mismatch in {f:?}"
            );
        }
    }

    #[test]
    fn encoder_synthesizes_opening_events_for_bare_delta() {
        // 上游（Anthropic/Gemini 转过来）可能没有 Start，也没有 ContentStart。
        let frames = encode_all(&[
            StreamEvent::TextDelta {
                index: 0,
                text: "x".into(),
            },
            StreamEvent::Done,
        ]);
        let names = event_names(&frames);
        assert_eq!(
            names,
            vec![
                "response.created",
                "response.in_progress",
                "response.output_item.added",
                "response.content_part.added",
                "response.output_text.delta",
                "response.output_text.done",
                "response.content_part.done",
                "response.output_item.done",
                "response.completed",
            ]
        );
        // 自造的 ID 必须非空且贯穿全流程。
        let created = payload(&frames[0]);
        let id = created["response"]["id"].as_str().expect("id");
        assert!(id.starts_with("resp_"), "generated id: {id}");
        let done = payload(frames.last().expect("last"));
        assert_eq!(done["response"]["id"], json!(id));
    }

    #[test]
    fn encoder_delays_function_call_item_until_name_known() {
        let frames = encode_all(&[
            StreamEvent::ContentStart {
                index: 0,
                kind: PartKind::ToolUse,
            },
            StreamEvent::ToolCallStart {
                signature: None,
                index: 0,
                id: "call_7".into(),
                name: "get_weather".into(),
            },
            StreamEvent::ToolCallArgsDelta {
                index: 0,
                fragment: "{\"c\":1}".into(),
            },
            StreamEvent::ContentStop { index: 0 },
            StreamEvent::Stop {
                reason: StopReason::ToolUse,
                stop_sequence: None,
            },
            StreamEvent::Done,
        ]);

        assert_eq!(
            event_names(&frames),
            vec![
                "response.created",
                "response.in_progress",
                // ContentStart 时还不知道函数名，所以 added 必须等到这里才发。
                "response.output_item.added",
                "response.function_call_arguments.delta",
                "response.function_call_arguments.done",
                "response.output_item.done",
                "response.completed",
            ]
        );

        let added = payload(&frames[2]);
        assert_eq!(added["item"]["type"], json!("function_call"));
        assert_eq!(added["item"]["name"], json!("get_weather"));
        assert_eq!(added["item"]["call_id"], json!("call_7"));
        // function_call 没有 content_part 事件。
        assert!(
            !event_names(&frames)
                .iter()
                .any(|n| n.starts_with("response.content_part")),
            "function_call items must not emit content_part events"
        );

        let done = payload(frames.last().expect("last"));
        assert_eq!(
            done["response"]["output"][0]["arguments"],
            json!(r#"{"c":1}"#)
        );
        assert_eq!(done["response"]["status"], json!("completed"));
    }

    #[test]
    fn encoder_writes_thinking_signature_into_encrypted_content() {
        // 硬性要求：signature 无损。Anthropic → Responses 这条路最容易丢它。
        let frames = encode_all(&[
            StreamEvent::ContentStart {
                index: 0,
                kind: PartKind::Thinking,
            },
            StreamEvent::ThinkingDelta {
                index: 0,
                text: "reason".into(),
            },
            StreamEvent::ThinkingSignature {
                index: 0,
                signature: "SIG==".into(),
            },
            StreamEvent::ContentStop { index: 0 },
            StreamEvent::Done,
        ]);

        assert_eq!(
            event_names(&frames),
            vec![
                "response.created",
                "response.in_progress",
                "response.output_item.added",
                "response.reasoning_summary_part.added",
                "response.reasoning_summary_text.delta",
                "response.reasoning_summary_text.done",
                "response.reasoning_summary_part.done",
                "response.output_item.done",
                "response.completed",
            ]
        );

        let item_done = payload(&frames[frames.len() - 2]);
        assert_eq!(item_done["item"]["encrypted_content"], json!("SIG=="));
        assert_eq!(item_done["item"]["summary"][0]["text"], json!("reason"));
        let completed = payload(frames.last().expect("last"));
        assert_eq!(
            completed["response"]["output"][0]["encrypted_content"],
            json!("SIG==")
        );
    }

    #[test]
    fn encoder_closes_open_blocks_on_truncated_stream() {
        // 上游断流没发 ContentStop/Done，客户端仍然要收到完整收尾。
        let mut enc = ResponsesStreamEncoder::default();
        let mut frames = Vec::new();
        for ev in [
            StreamEvent::Start {
                id: "resp_t".into(),
                model: "m".into(),
                usage: None,
            },
            StreamEvent::TextDelta {
                index: 0,
                text: "partial".into(),
            },
        ] {
            frames.extend(enc.encode(&ev).expect("encode"));
        }
        let tail = enc.finish().expect("finish");
        assert_eq!(
            event_names(&tail),
            vec![
                "response.output_text.done",
                "response.content_part.done",
                "response.output_item.done",
                "response.completed",
            ]
        );
        // 序号跨 finish 继续递增，不能回到 0。
        let first_tail_seq = payload(&tail[0])["sequence_number"].as_u64().expect("seq");
        assert_eq!(first_tail_seq, frames.len() as u64);
    }

    #[test]
    fn encoder_maps_max_tokens_stop_to_incomplete_event() {
        let frames = encode_all(&[
            StreamEvent::TextDelta {
                index: 0,
                text: "cut".into(),
            },
            StreamEvent::Stop {
                reason: StopReason::MaxTokens,
                stop_sequence: None,
            },
            StreamEvent::Done,
        ]);
        let last = frames.last().expect("last");
        assert_eq!(last.event.as_deref(), Some("response.incomplete"));
        let body = payload(last);
        assert_eq!(body["response"]["status"], json!("incomplete"));
        assert_eq!(
            body["response"]["incomplete_details"],
            json!({"reason": "max_output_tokens"})
        );
    }

    #[test]
    fn encoder_emits_error_event_and_stops_afterwards() {
        let mut enc = ResponsesStreamEncoder::default();
        let mut frames = enc
            .encode(&StreamEvent::Error {
                message: "overloaded".into(),
                kind: "server_error".into(),
            })
            .expect("encode");
        // 错误之后的事件一律丢弃，客户端不能在 error 之后收到 delta。
        let after = enc
            .encode(&StreamEvent::TextDelta {
                index: 0,
                text: "late".into(),
            })
            .expect("encode");
        assert!(after.is_empty(), "no frames after error, got {after:?}");
        frames.extend(enc.finish().expect("finish"));

        let names = event_names(&frames);
        assert_eq!(names.last().map(String::as_str), Some("error"));
        let err = payload(frames.last().expect("last"));
        assert_eq!(err["message"], json!("overloaded"));
        assert_eq!(err["code"], json!("server_error"));
        assert_eq!(err["param"], Value::Null);
    }

    #[test]
    fn encoder_multiplexes_two_blocks_into_separate_items() {
        let frames = encode_all(&[
            StreamEvent::ContentStart {
                index: 0,
                kind: PartKind::Thinking,
            },
            StreamEvent::ThinkingDelta {
                index: 0,
                text: "t".into(),
            },
            StreamEvent::ContentStop { index: 0 },
            StreamEvent::ContentStart {
                index: 1,
                kind: PartKind::Text,
            },
            StreamEvent::TextDelta {
                index: 1,
                text: "answer".into(),
            },
            StreamEvent::ContentStop { index: 1 },
            StreamEvent::Done,
        ]);

        // 两个块必须落在不同 output_index 上。
        let indices: Vec<u64> = frames
            .iter()
            .filter(|f| f.event.as_deref() == Some("response.output_item.added"))
            .map(|f| payload(f)["output_index"].as_u64().expect("oi"))
            .collect();
        assert_eq!(indices, vec![0, 1]);

        let completed = payload(frames.last().expect("last"));
        let output = completed["response"]["output"].as_array().expect("output");
        assert_eq!(output.len(), 2);
        assert_eq!(output[0]["type"], json!("reasoning"));
        assert_eq!(output[1]["type"], json!("message"));
        assert_eq!(completed["response"]["output_text"], json!("answer"));
    }

    #[test]
    fn protocol_is_responses() {
        assert_eq!(RESPONSES.protocol(), Protocol::Responses);
    }
}
