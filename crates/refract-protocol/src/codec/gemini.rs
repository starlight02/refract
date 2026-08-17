//! Google Gemini `generateContent` 协议编解码。
//!
//! # 与其他三个协议最不一样的地方
//!
//! 1. **模型名不在请求体里，在 URL 里** ——
//!    `POST /v1beta/models/{model}:generateContent`。官方 API 会拒绝请求体中
//!    出现 `model` 字段，所以 [`GeminiCodec::encode_request`] 绝不写它，
//!    路由层用候选自带的模型名拼 URL；[`GeminiCodec::decode_request`] 则容忍
//!    `model` 缺失（留空字符串，由上层从 URL 填充）。
//! 2. **助手角色叫 `model` 而不是 `assistant`**。
//! 3. **`tools` 是数组套数组** ——
//!    `[{functionDeclarations:[...]}]`，一层 `Tool` 包着一堆函数声明。
//! 4. **`functionCall` 可能没有 `id`**。Gemini 的单轮工具调用普遍不带 id，
//!    但 IR 与 Anthropic/OpenAI 都靠 id 关联工具结果。缺 id 时用 `name` 兜底，
//!    这是跨协议转换里真实会踩的坑（见 [`fallback_call_id`]）。
//! 5. **流式每个 chunk 是完整的 `GenerateContentResponse`**，不是增量结构，
//!    而且**没有 `[DONE]` 哨兵** —— 流的终结就是连接关闭。
//!
//! # 有损转换点
//!
//! `safetySettings` / `cachedContent` / `safetyRatings` / `citationMetadata`
//! 在其他三个协议里没有任何对应物，只能走 extensions 原样携带：来自 Gemini 的
//! 请求转到 Gemini 上游时无损，转到别的协议时由目标编码器丢弃。

use std::collections::HashMap;

use refract_core::{ErrorKind, GatewayError, Protocol};
use serde_json::{Map, Value, json};

use crate::codec::{ProtocolCodec, RequestCodec, ResponseCodec, StreamCodec};
use crate::ir::{
    ContentPart, MediaSource, Message, ReasoningConfig, ResponseFormat, Role, Sampling, StopReason,
    ToolChoice, ToolDef, UnifiedRequest, UnifiedResponse, Usage,
};
use crate::stream::{PartKind, SseFrame, StreamDecoder, StreamEncoder, StreamEvent};

/// Google Gemini codec。
pub struct GeminiCodec;

/// 供 `CodecSet` 注册的单例。
pub static GEMINI: GeminiCodec = GeminiCodec;

/// 请求体里出现过的顶层字段，未列出的都会被收进 extensions。
const KNOWN_REQUEST_FIELDS: &[&str] = &[
    "model",
    "contents",
    "systemInstruction",
    "system_instruction",
    "tools",
    "toolConfig",
    "tool_config",
    "generationConfig",
    "generation_config",
    "safetySettings",
    "safety_settings",
    "cachedContent",
    "cached_content",
];

/// `generationConfig` 里我们认得的字段。
const KNOWN_GENERATION_FIELDS: &[&str] = &[
    "temperature",
    "topP",
    "top_p",
    "topK",
    "top_k",
    "maxOutputTokens",
    "max_output_tokens",
    "stopSequences",
    "stop_sequences",
    "candidateCount",
    "candidate_count",
    "seed",
    "responseMimeType",
    "response_mime_type",
    "responseSchema",
    "response_schema",
    "thinkingConfig",
    "thinking_config",
];

/// 读取 camelCase 字段，回退到 snake_case。
///
/// Gemini 官方文档用 camelCase，但 REST API 同时接受 snake_case，
/// 各家 SDK 与中转站两种都会发。
fn field<'a>(obj: &'a Map<String, Value>, camel: &str, snake: &str) -> Option<&'a Value> {
    obj.get(camel).or_else(|| obj.get(snake))
}

/// 把非空字符串取出来，空串视作缺失。
fn non_empty_str(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// `functionCall` / `functionResponse` 缺 `id` 时的兜底 ID。
///
/// Gemini 单轮工具调用通常不带 id，而 IR、Anthropic、OpenAI 都靠 id 把
/// 工具结果关联回调用。用函数名兜底能让「调用 → 回传」在同一轮内正确配对；
/// 同名函数并行调用多次时会撞车，但那种场景 Gemini 自己也会给出 id。
fn fallback_call_id(id: Option<&Value>, name: &str) -> String {
    non_empty_str(id).unwrap_or_else(|| name.to_owned())
}

/// 按 MIME 前缀把内联/远程媒体分流到对应的 [`ContentPart`] 变体。
fn media_part(source: MediaSource, mime: Option<String>) -> ContentPart {
    // 先算完所有依赖 mime 借用的东西，再把 mime 移动进枚举，避免借用/移动交叠。
    let kind = mime.as_deref().unwrap_or("");
    let is_image = kind.starts_with("image/");
    let is_audio = kind.starts_with("audio/");
    // IR 的 Audio 用格式名而非完整 MIME，取子类型（`audio/mp3` → `mp3`）。
    let audio_format = kind.split('/').nth(1).map(str::to_owned);

    if is_image {
        ContentPart::Image {
            source,
            mime,
            detail: None,
        }
    } else if is_audio {
        ContentPart::Audio {
            source,
            format: audio_format,
        }
    } else {
        ContentPart::File {
            source,
            mime,
            name: None,
        }
    }
}

/// 从 [`ContentPart`] 反推 Gemini 需要的 MIME。
fn part_mime(part: &ContentPart) -> Option<String> {
    match part {
        ContentPart::Image { mime, .. } | ContentPart::File { mime, .. } => mime.clone(),
        // Audio 只存了格式名，补回 `audio/` 前缀。
        ContentPart::Audio { format, .. } => format.as_ref().map(|f| format!("audio/{f}")),
        _ => None,
    }
}

/// Gemini `finishReason` → IR [`StopReason`]。
///
/// 五种安全类终止（SAFETY / RECITATION / BLOCKLIST / PROHIBITED_CONTENT /
/// SPII）在 IR 里都归一成 [`StopReason::ContentFilter`] —— 其他协议只有一个
/// 内容过滤概念，细分原因保留在 extensions 的 `finishReason` 里。
fn stop_reason_from_gemini(raw: &str) -> StopReason {
    match raw {
        "STOP" => StopReason::Stop,
        "MAX_TOKENS" => StopReason::MaxTokens,
        "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" => {
            StopReason::ContentFilter
        }
        "MALFORMED_FUNCTION_CALL" | "OTHER" => StopReason::Other,
        _ => StopReason::Other,
    }
}

/// IR [`StopReason`] → Gemini `finishReason`。
///
/// Gemini 没有「因为要调工具而停」这个原因，工具调用回合的 `finishReason`
/// 就是 `STOP`，所以 [`StopReason::ToolUse`] 反向映射回 `STOP`。
fn stop_reason_to_gemini(reason: StopReason) -> &'static str {
    match reason {
        // Gemini 用 STOP 表示「正常说完」，工具调用与命中停止序列都算正常说完。
        StopReason::Stop | StopReason::ToolUse | StopReason::StopSequence => "STOP",
        StopReason::MaxTokens => "MAX_TOKENS",
        StopReason::ContentFilter => "SAFETY",
        // Gemini 无「模型拒答」与「暂停回合」概念，落到 OTHER。
        StopReason::Refusal | StopReason::PauseTurn | StopReason::Other => "OTHER",
    }
}

/// 解析 `usageMetadata`。
///
/// 口径换算：IR 的 `output_tokens` 统一为**含 reasoning**（OpenAI 的
/// `completion_tokens` 与 Anthropic 的 `output_tokens` 都是这个口径），
/// 而 Gemini 的 `candidatesTokenCount` **不含** `thoughtsTokenCount`。
/// 不在这里补齐的话，gemini→chat 的 `completion_tokens` 会漏掉思考消耗，
/// 计费口径失真。
fn parse_usage(raw: Option<&Value>) -> Usage {
    let Some(obj) = raw.and_then(Value::as_object) else {
        return Usage::default();
    };
    let num = |camel: &str, snake: &str| {
        field(obj, camel, snake)
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    let reasoning = num("thoughtsTokenCount", "thoughts_token_count");
    Usage {
        input_tokens: num("promptTokenCount", "prompt_token_count"),
        output_tokens: num("candidatesTokenCount", "candidates_token_count") + reasoning,
        cached_input_tokens: num("cachedContentTokenCount", "cached_content_token_count"),
        // Gemini 没有「写入缓存」的计量（显式缓存单独计费，不在这里报）。
        cache_write_tokens: 0,
        reasoning_tokens: reasoning,
    }
}

/// 编码 `usageMetadata`。
///
/// [`parse_usage`] 的逆向：IR `output_tokens` 含 reasoning，Gemini 的
/// `candidatesTokenCount` 不含，写出时要减回去；`totalTokenCount` 则是
/// 全口径总和（input + output，output 已含 reasoning）。
fn encode_usage(usage: &Usage) -> Value {
    let mut obj = Map::new();
    obj.insert("promptTokenCount".into(), json!(usage.input_tokens));
    obj.insert(
        "candidatesTokenCount".into(),
        json!(usage.output_tokens.saturating_sub(usage.reasoning_tokens)),
    );
    obj.insert(
        "totalTokenCount".into(),
        json!(usage.input_tokens + usage.output_tokens),
    );
    if usage.cached_input_tokens > 0 {
        obj.insert(
            "cachedContentTokenCount".into(),
            json!(usage.cached_input_tokens),
        );
    }
    if usage.reasoning_tokens > 0 {
        obj.insert("thoughtsTokenCount".into(), json!(usage.reasoning_tokens));
    }
    Value::Object(obj)
}

/// 解析单个 `part` 为 [`ContentPart`]。
///
/// 认不出的 part（如 `executableCode`）不返回 `None` 而是包成
/// [`ContentPart::Opaque`] —— 服务端工具类内容块在多轮对话里必须原样
/// 回传，丢弃会损坏 gemini→gemini 直通的对话历史。
fn decode_part(part: &Value) -> Option<ContentPart> {
    let obj = part.as_object()?;

    // text：`thought: true` 时是推理内容而非普通文本。
    if let Some(text) = obj.get("text").and_then(Value::as_str) {
        let is_thought = obj.get("thought").and_then(Value::as_bool).unwrap_or(false);
        if is_thought {
            return Some(ContentPart::Thinking {
                text: text.to_owned(),
                signature: non_empty_str(field(obj, "thoughtSignature", "thought_signature")),
            });
        }
        return Some(ContentPart::Text {
            text: text.to_owned(),
        });
    }

    // inlineData：base64 内联媒体。
    if let Some(inline) = field(obj, "inlineData", "inline_data").and_then(Value::as_object) {
        let mime = non_empty_str(field(inline, "mimeType", "mime_type"));
        let data = inline.get("data").and_then(Value::as_str).unwrap_or("");
        return Some(media_part(MediaSource::Base64(data.to_owned()), mime));
    }

    // fileData：Files API 上传后的 URI 引用。
    if let Some(file) = field(obj, "fileData", "file_data").and_then(Value::as_object) {
        let mime = non_empty_str(field(file, "mimeType", "mime_type"));
        let uri = field(file, "fileUri", "file_uri")
            .and_then(Value::as_str)
            .unwrap_or("");
        return Some(media_part(MediaSource::FileId(uri.to_owned()), mime));
    }

    // functionCall：模型请求调用工具。可能没有 id。
    if let Some(call) = field(obj, "functionCall", "function_call").and_then(Value::as_object) {
        let name = call.get("name").and_then(Value::as_str).unwrap_or("");
        return Some(ContentPart::ToolUse {
            id: fallback_call_id(call.get("id"), name),
            name: name.to_owned(),
            input: call.get("args").cloned().unwrap_or_else(|| json!({})),
            // 签名挂在 part 级（与 functionCall 同级）。Gemini 3 强制要求
            // 多轮回传时带着它，丢失即 400，所以必须走 IR 全程保留。
            signature: non_empty_str(field(obj, "thoughtSignature", "thought_signature")),
        });
    }

    // functionResponse：工具执行结果回传。
    if let Some(resp) =
        field(obj, "functionResponse", "function_response").and_then(Value::as_object)
    {
        let name = resp.get("name").and_then(Value::as_str).unwrap_or("");
        let payload = resp.get("response").cloned().unwrap_or_else(|| json!({}));
        // Gemini 的 response 是结构化 JSON；IR 用 ContentPart 列表表达，
        // 所以序列化成文本承载。字符串直接用原文，避免多一层引号。
        let text = match &payload {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        // Gemini 没有 is_error 标志，约定看 response 里有没有 `error` 键。
        let is_error = payload.as_object().is_some_and(|o| o.contains_key("error"));
        return Some(ContentPart::ToolResult {
            id: fallback_call_id(resp.get("id"), name),
            name: (!name.is_empty()).then(|| name.to_owned()),
            content: vec![ContentPart::Text { text }],
            is_error,
        });
    }

    // 认不出的 part（executableCode、codeExecutionResult……）不丢弃：
    // 包成 Opaque，同协议直通时原样还原，跨协议时由目标编码器丢弃。
    Some(ContentPart::Opaque {
        protocol: "gemini".to_owned(),
        value: part.clone(),
    })
}

/// 把一组 `parts` 解析成 [`ContentPart`] 列表。
fn decode_parts(parts: Option<&Value>) -> Vec<ContentPart> {
    let Some(list) = parts.and_then(Value::as_array) else {
        return Vec::new();
    };
    list.iter().filter_map(decode_part).collect()
}

/// 把 [`ContentPart`] 编码成 Gemini `part`。
///
/// 返回 `None` 表示这个片段 Gemini 无法表达（如 `RedactedThinking`、
/// `Refusal`），静默丢弃并 `tracing::debug!`。
///
/// `tool_names` 是同一请求对话历史里 `ToolUse` 的 `id → name` 映射，用来
/// 给缺 `name` 的 `ToolResult`（OpenAI 系协议转来的）反查函数名 ——
/// Gemini 的 `functionResponse.name` 必须是函数声明名，配错直接 400。
fn encode_part(part: &ContentPart, tool_names: &HashMap<&str, &str>) -> Option<Value> {
    match part {
        ContentPart::Text { text } => Some(json!({ "text": text })),
        ContentPart::Thinking { text, signature } => {
            let mut obj = Map::new();
            obj.insert("text".into(), json!(text));
            obj.insert("thought".into(), json!(true));
            // signature 必须无损保留：多轮工具调用丢了它上游会拒绝整个请求。
            if let Some(sig) = signature {
                obj.insert("thoughtSignature".into(), json!(sig));
            }
            Some(Value::Object(obj))
        }
        ContentPart::Image { source, .. }
        | ContentPart::Audio { source, .. }
        | ContentPart::File { source, .. } => {
            let mime = part_mime(part);
            match source {
                MediaSource::Base64(data) => Some(json!({
                    "inlineData": {
                        "mimeType": mime.unwrap_or_else(|| "application/octet-stream".to_owned()),
                        "data": data,
                    }
                })),
                // 纯文本按 inlineData 表达：inlineData.data 必须是 base64。
                MediaSource::Text(text) => {
                    use base64::Engine as _;
                    Some(json!({
                        "inlineData": {
                            "mimeType": mime.unwrap_or_else(|| "text/plain".to_owned()),
                            "data": base64::engine::general_purpose::STANDARD.encode(text),
                        }
                    }))
                }
                // Gemini 只接受 Files API 的 URI，普通 URL 也只能塞这里 ——
                // 上游会自己判断可达性，网关不做预校验。
                MediaSource::FileId(uri) | MediaSource::Url(uri) => Some(json!({
                    "fileData": {
                        "mimeType": mime.unwrap_or_else(|| "application/octet-stream".to_owned()),
                        "fileUri": uri,
                    }
                })),
            }
        }
        ContentPart::ToolUse {
            id,
            name,
            input,
            signature,
        } => {
            let mut call = Map::new();
            // id 只在非空且与 name 不同时才写回：兜底 id 是我们造的，
            // 回传给 Gemini 反而可能被拒。
            if !id.is_empty() && id != name {
                call.insert("id".into(), json!(id));
            }
            call.insert("name".into(), json!(name));
            call.insert("args".into(), input.clone());
            let mut part = Map::new();
            part.insert("functionCall".into(), Value::Object(call));
            // 签名是 part 级字段。Gemini 3 缺它直接 400，必须原样还原。
            if let Some(sig) = signature {
                part.insert("thoughtSignature".into(), json!(sig));
            }
            Some(Value::Object(part))
        }
        ContentPart::ToolResult {
            id,
            name,
            content,
            is_error,
        } => {
            let text = content
                .iter()
                .filter_map(ContentPart::as_text)
                .collect::<Vec<_>>()
                .join("");
            // 尽量还原成结构化 JSON；不是 JSON 就包一层，Gemini 要求
            // response 必须是 object。
            let payload = serde_json::from_str::<Value>(&text)
                .ok()
                .filter(Value::is_object)
                .unwrap_or_else(|| {
                    if *is_error {
                        json!({ "error": text })
                    } else {
                        json!({ "output": text })
                    }
                });
            let mut resp = Map::new();
            if !id.is_empty() {
                resp.insert("id".into(), json!(id));
            }
            // Gemini 靠 name 匹配声明的函数，回传时 name 必填。优先级：
            // IR 里保留的函数名 → 从对话历史的 ToolUse 反查 → id 兜底
            // （Gemini 自产的无 id 调用，兜底 id 恰好就是函数名）。
            let resolved = name
                .as_deref()
                .or_else(|| tool_names.get(id.as_str()).copied())
                .unwrap_or(id);
            resp.insert("name".into(), json!(resolved));
            resp.insert("response".into(), payload);
            Some(json!({ "functionResponse": Value::Object(resp) }))
        }
        ContentPart::Opaque { protocol, value } => {
            // 只在同协议直通时原样还原，别的协议的私有块塞给 Gemini 必 400。
            if protocol == "gemini" {
                Some(value.clone())
            } else {
                tracing::debug!(%protocol, "非 gemini 的 Opaque 块无法表达，已丢弃");
                None
            }
        }
        other => {
            tracing::debug!(?other, "gemini 无法表达该内容片段，已丢弃");
            None
        }
    }
}

/// IR 角色 → Gemini `role`。
///
/// Gemini 只有 `user` 与 `model` 两种角色。工具结果在 Gemini 里属于 `user`
/// 回合（`functionResponse` part），与 Anthropic 的处理一致。
const fn role_to_gemini(role: Role) -> &'static str {
    match role {
        Role::Assistant => "model",
        // System 理应已被提到 systemInstruction；万一残留就当 user 发出去，
        // 总比丢掉指令好。
        Role::User | Role::Tool | Role::System => "user",
    }
}

impl RequestCodec for GeminiCodec {
    fn decode_request(&self, raw: &Value) -> Result<UnifiedRequest, GatewayError> {
        let obj = raw
            .as_object()
            .ok_or_else(|| GatewayError::invalid_request("request body must be a JSON object"))?;

        // 模型名在 URL 里（`/v1beta/models/{model}:generateContent`），请求体
        // 通常没有。缺失时留空，由上层用 URL 里的模型名填充。
        let model = non_empty_str(obj.get("model")).unwrap_or_default();

        let contents = obj
            .get("contents")
            .ok_or_else(|| GatewayError::invalid_request("missing required field `contents`"))?;
        let contents = contents.as_array().ok_or_else(|| {
            GatewayError::invalid_request("field `contents` must be an array of Content objects")
        })?;

        let mut messages = Vec::with_capacity(contents.len());
        for entry in contents {
            let entry = entry.as_object().ok_or_else(|| {
                GatewayError::invalid_request("each item in `contents` must be an object")
            })?;
            let parts = decode_parts(entry.get("parts"));
            // Gemini 允许省略 role（单轮对话），默认按 user 处理。
            let role = match entry.get("role").and_then(Value::as_str) {
                Some("model") => Role::Assistant,
                Some("function") => Role::Tool,
                _ => Role::User,
            };
            // 只含 functionResponse 的 user 回合，语义上是工具结果，
            // 提升为 Role::Tool 让目标协议能正确还原。
            let role = if role == Role::User
                && !parts.is_empty()
                && parts
                    .iter()
                    .all(|p| matches!(p, ContentPart::ToolResult { .. }))
            {
                Role::Tool
            } else {
                role
            };
            messages.push(Message::new(role, parts));
        }

        let mut ir = UnifiedRequest::new(model, messages);

        // systemInstruction 是独立顶层字段，直接进 IR 的 system。
        if let Some(sys) = field(obj, "systemInstruction", "system_instruction") {
            ir.system = match sys {
                // 官方结构是 {parts:[...]}，但很多客户端直接发字符串。
                Value::String(s) => vec![ContentPart::text(s.clone())],
                Value::Object(o) => decode_parts(o.get("parts")),
                _ => Vec::new(),
            };
        }

        // tools 是数组套数组：[{functionDeclarations:[{...}]}]。
        if let Some(tools) = obj.get("tools").and_then(Value::as_array) {
            let mut extra = Vec::new();
            for tool in tools {
                let Some(tool_obj) = tool.as_object() else {
                    continue;
                };
                let decls = field(tool_obj, "functionDeclarations", "function_declarations")
                    .and_then(Value::as_array);
                if let Some(decls) = decls {
                    for decl in decls {
                        let Some(decl) = decl.as_object() else {
                            continue;
                        };
                        let name = decl.get("name").and_then(Value::as_str).ok_or_else(|| {
                            GatewayError::invalid_request(
                                "each functionDeclaration requires a `name`",
                            )
                        })?;
                        ir.tools.push(ToolDef {
                            name: name.to_owned(),
                            description: non_empty_str(decl.get("description")),
                            parameters: decl
                                .get("parameters")
                                .cloned()
                                .unwrap_or_else(|| json!({ "type": "object" })),
                            strict: None,
                        });
                    }
                }
                // googleSearch / codeExecution / urlContext 这类内置工具在
                // 其他协议里没有对应物，原样留存供 Gemini→Gemini 无损透传。
                let builtins: Map<String, Value> = tool_obj
                    .iter()
                    .filter(|(k, _)| {
                        k.as_str() != "functionDeclarations"
                            && k.as_str() != "function_declarations"
                    })
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                if !builtins.is_empty() {
                    extra.push(Value::Object(builtins));
                }
            }
            if !extra.is_empty() {
                ir.set_extension("gemini.builtinTools", Value::Array(extra));
            }
        }

        // toolConfig.functionCallingConfig.mode：AUTO / ANY / NONE。
        if let Some(cfg) = field(obj, "toolConfig", "tool_config").and_then(Value::as_object) {
            let fc = field(cfg, "functionCallingConfig", "function_calling_config")
                .and_then(Value::as_object);
            if let Some(fc) = fc {
                let allowed = field(fc, "allowedFunctionNames", "allowed_function_names")
                    .and_then(Value::as_array);
                // ANY + 恰好一个 allowedFunctionNames = 强制调用指定工具。
                let single = allowed
                    .filter(|a| a.len() == 1)
                    .and_then(|a| a[0].as_str())
                    .map(str::to_owned);
                ir.tool_choice = match fc.get("mode").and_then(Value::as_str) {
                    Some("AUTO") => ToolChoice::Auto,
                    Some("ANY") => match single {
                        Some(name) => ToolChoice::Tool(name),
                        None => ToolChoice::Required,
                    },
                    Some("NONE") => ToolChoice::None,
                    _ => ToolChoice::Unspecified,
                };
                // mode 为 ANY 且允许多个函数名时，名单本身无处安放，留在 extensions。
                if ir.tool_choice == ToolChoice::Required
                    && let Some(names) = field(fc, "allowedFunctionNames", "allowed_function_names")
                {
                    ir.set_extension("gemini.allowedFunctionNames", names.clone());
                }
            }
        }

        // generationConfig 承载所有采样参数。
        if let Some(gen_cfg) =
            field(obj, "generationConfig", "generation_config").and_then(Value::as_object)
        {
            let f64_of =
                |camel: &str, snake: &str| field(gen_cfg, camel, snake).and_then(Value::as_f64);
            let u32_of = |camel: &str, snake: &str| {
                field(gen_cfg, camel, snake)
                    .and_then(Value::as_u64)
                    .map(|v| v as u32)
            };

            ir.sampling = Sampling {
                temperature: gen_cfg.get("temperature").and_then(Value::as_f64),
                top_p: f64_of("topP", "top_p"),
                top_k: u32_of("topK", "top_k"),
                // Gemini 无频率/存在惩罚（旧版有 frequencyPenalty，非通用，不映射）。
                frequency_penalty: None,
                presence_penalty: None,
                stop: field(gen_cfg, "stopSequences", "stop_sequences")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default(),
                seed: gen_cfg.get("seed").and_then(Value::as_i64),
                candidate_count: u32_of("candidateCount", "candidate_count"),
            };
            ir.max_output_tokens = u32_of("maxOutputTokens", "max_output_tokens");

            // responseMimeType + responseSchema → ResponseFormat。
            let mime =
                field(gen_cfg, "responseMimeType", "response_mime_type").and_then(Value::as_str);
            let schema = field(gen_cfg, "responseSchema", "response_schema");
            ir.response_format = match (mime, schema) {
                (Some("application/json"), Some(schema)) => Some(ResponseFormat::JsonSchema {
                    // Gemini 的 schema 没有名字，IR 需要一个，给个稳定占位。
                    name: "response".to_owned(),
                    schema: schema.clone(),
                    strict: true,
                }),
                (Some("application/json"), None) => Some(ResponseFormat::JsonObject),
                (Some("text/plain"), _) => Some(ResponseFormat::Text),
                // application/xml、text/x.enum 等非 JSON 约束无法归一化。
                (Some(other), _) => {
                    ir.set_extension("gemini.responseMimeType", json!(other));
                    None
                }
                (None, _) => None,
            };

            // thinkingConfig：thinkingBudget 是定量预算，includeThoughts 决定
            // 是否回传推理内容。
            if let Some(tc) =
                field(gen_cfg, "thinkingConfig", "thinking_config").and_then(Value::as_object)
            {
                let budget = field(tc, "thinkingBudget", "thinking_budget")
                    .and_then(Value::as_i64)
                    // -1 是 Gemini 的「动态思考」哨兵，不是真实预算，
                    // 归一化成 None 并留在 extensions 里以便回传时还原。
                    .and_then(|v| u32::try_from(v).ok());
                let include =
                    field(tc, "includeThoughts", "include_thoughts").and_then(Value::as_bool);
                if budget.is_some() || include.is_some() {
                    ir.reasoning = Some(ReasoningConfig {
                        effort: None,
                        budget_tokens: budget,
                        include_thoughts: include,
                    });
                }
                if field(tc, "thinkingBudget", "thinking_budget").and_then(Value::as_i64)
                    == Some(-1)
                {
                    ir.set_extension("gemini.dynamicThinking", json!(true));
                }
            }

            // generationConfig 里的未知字段（responseModalities、speechConfig……）。
            let unknown: Map<String, Value> = gen_cfg
                .iter()
                .filter(|(k, _)| !KNOWN_GENERATION_FIELDS.contains(&k.as_str()))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            if !unknown.is_empty() {
                ir.set_extension("gemini.generationConfig", Value::Object(unknown));
            }
        }

        // safetySettings 与 cachedContent：其他协议完全没有对应概念，
        // 只能原样携带（Gemini→Gemini 无损，转出去时由目标编码器丢弃）。
        if let Some(safety) = field(obj, "safetySettings", "safety_settings") {
            ir.set_extension("gemini.safetySettings", safety.clone());
        }
        if let Some(cached) = field(obj, "cachedContent", "cached_content") {
            ir.set_extension("gemini.cachedContent", cached.clone());
        }

        // 剩下的未知顶层字段整体留存。
        for (key, value) in obj {
            if !KNOWN_REQUEST_FIELDS.contains(&key.as_str()) {
                ir.set_extension(format!("gemini.{key}"), value.clone());
            }
        }

        // Gemini 用 URL 后缀 `:streamGenerateContent?alt=sse` 而非请求体字段
        // 表达流式，所以请求体解码不出 stream —— 由路由层按路径设置。
        // usage 在 Gemini 流式里总是随最后一帧回来，无需客户端开关。
        ir.stream_include_usage = true;

        Ok(ir)
    }

    fn encode_request(&self, ir: &UnifiedRequest) -> Result<Value, GatewayError> {
        let mut out = Map::new();

        // 关键：**不写 `model`**。官方 `generateContent` 端点会拒绝请求体里
        // 出现 model 字段，模型名属于 URL 路径，由路由层的候选信息提供。

        // 对话历史里 ToolUse 的 id → name 映射，给 ToolResult 反查函数名。
        let tool_names: HashMap<&str, &str> = ir
            .messages
            .iter()
            .flat_map(|m| m.content.iter())
            .filter_map(|p| match p {
                ContentPart::ToolUse { id, name, .. } => Some((id.as_str(), name.as_str())),
                _ => None,
            })
            .collect();

        let mut contents: Vec<Value> = Vec::with_capacity(ir.messages.len());
        for msg in &ir.messages {
            // System 消息若残留在 messages 里，合并进 systemInstruction 而不是
            // 当普通回合发出去。
            if msg.role == Role::System {
                continue;
            }
            let parts: Vec<Value> = msg
                .content
                .iter()
                .filter_map(|p| encode_part(p, &tool_names))
                .collect();
            if parts.is_empty() {
                continue;
            }
            let role = role_to_gemini(msg.role);
            // 连续同角色必须合并：Gemini 要求 user/model 严格交替，
            // 连着两个 user 回合会被上游拒绝。
            match contents.last_mut() {
                Some(Value::Object(prev))
                    if prev.get("role").and_then(Value::as_str) == Some(role) =>
                {
                    if let Some(Value::Array(existing)) = prev.get_mut("parts") {
                        existing.extend(parts);
                    }
                }
                _ => contents.push(json!({ "role": role, "parts": parts })),
            }
        }
        out.insert("contents".into(), Value::Array(contents));

        // systemInstruction：IR 的 system 字段 + messages 里残留的 System 消息。
        let mut system_parts: Vec<Value> = ir
            .system
            .iter()
            .filter_map(|p| encode_part(p, &tool_names))
            .collect();
        for msg in &ir.messages {
            if msg.role == Role::System {
                system_parts.extend(
                    msg.content
                        .iter()
                        .filter_map(|p| encode_part(p, &tool_names)),
                );
            }
        }
        if !system_parts.is_empty() {
            out.insert("systemInstruction".into(), json!({ "parts": system_parts }));
        }

        // tools：所有函数声明塞进**一个** Tool 对象的 functionDeclarations 里。
        let mut tools: Vec<Value> = Vec::new();
        if !ir.tools.is_empty() {
            let decls: Vec<Value> = ir
                .tools
                .iter()
                .map(|t| {
                    let mut decl = Map::new();
                    decl.insert("name".into(), json!(t.name));
                    if let Some(desc) = &t.description {
                        decl.insert("description".into(), json!(desc));
                    }
                    decl.insert("parameters".into(), t.parameters.clone());
                    Value::Object(decl)
                })
                .collect();
            tools.push(json!({ "functionDeclarations": decls }));
        }
        // 还原内置工具（googleSearch 等）。
        if let Some(Value::Array(builtins)) = ir.extension("gemini.builtinTools") {
            tools.extend(builtins.iter().cloned());
        }
        if !tools.is_empty() {
            out.insert("tools".into(), Value::Array(tools));
        }

        // toolConfig。
        let fc_mode = match &ir.tool_choice {
            ToolChoice::Unspecified => None,
            ToolChoice::Auto => Some(json!({ "mode": "AUTO" })),
            ToolChoice::None => Some(json!({ "mode": "NONE" })),
            ToolChoice::Required => {
                let mut cfg = Map::new();
                cfg.insert("mode".into(), json!("ANY"));
                if let Some(names) = ir.extension("gemini.allowedFunctionNames") {
                    cfg.insert("allowedFunctionNames".into(), names.clone());
                }
                Some(Value::Object(cfg))
            }
            ToolChoice::Tool(name) => Some(json!({
                "mode": "ANY",
                "allowedFunctionNames": [name],
            })),
        };
        if let Some(fc) = fc_mode {
            out.insert("toolConfig".into(), json!({ "functionCallingConfig": fc }));
        }

        // generationConfig。
        let mut gen_cfg = Map::new();
        if let Some(Value::Object(extra)) = ir.extension("gemini.generationConfig") {
            for (k, v) in extra {
                gen_cfg.insert(k.clone(), v.clone());
            }
        }
        let s = &ir.sampling;
        if let Some(t) = s.temperature {
            gen_cfg.insert("temperature".into(), json!(t));
        }
        if let Some(p) = s.top_p {
            gen_cfg.insert("topP".into(), json!(p));
        }
        if let Some(k) = s.top_k {
            gen_cfg.insert("topK".into(), json!(k));
        }
        if let Some(seed) = s.seed {
            gen_cfg.insert("seed".into(), json!(seed));
        }
        if let Some(n) = s.candidate_count {
            gen_cfg.insert("candidateCount".into(), json!(n));
        }
        if !s.stop.is_empty() {
            gen_cfg.insert("stopSequences".into(), json!(s.stop));
        }
        if s.frequency_penalty.is_some() || s.presence_penalty.is_some() {
            tracing::debug!("gemini 不支持 frequency/presence penalty，已丢弃");
        }
        if let Some(max) = ir.max_output_tokens {
            gen_cfg.insert("maxOutputTokens".into(), json!(max));
        }
        match &ir.response_format {
            Some(ResponseFormat::JsonObject) => {
                gen_cfg.insert("responseMimeType".into(), json!("application/json"));
            }
            Some(ResponseFormat::JsonSchema { schema, .. }) => {
                gen_cfg.insert("responseMimeType".into(), json!("application/json"));
                gen_cfg.insert("responseSchema".into(), schema.clone());
            }
            Some(ResponseFormat::Text) => {
                gen_cfg.insert("responseMimeType".into(), json!("text/plain"));
            }
            None => {
                if let Some(mime) = ir.extension("gemini.responseMimeType") {
                    gen_cfg.insert("responseMimeType".into(), mime.clone());
                }
            }
        }
        // thinkingConfig：IR 只有档位（来自 OpenAI）时折算成预算，
        // 否则思考功能会被静默关闭。
        if let Some(r) = &ir.reasoning {
            let mut tc = Map::new();
            if ir.extension("gemini.dynamicThinking") == Some(&json!(true)) {
                tc.insert("thinkingBudget".into(), json!(-1));
            } else if let Some(budget) = r.budget_or_from_effort(ir.max_output_tokens) {
                tc.insert("thinkingBudget".into(), json!(budget));
            }
            if let Some(include) = r.include_thoughts {
                tc.insert("includeThoughts".into(), json!(include));
            }
            if !tc.is_empty() {
                gen_cfg.insert("thinkingConfig".into(), Value::Object(tc));
            }
        }
        if !gen_cfg.is_empty() {
            out.insert("generationConfig".into(), Value::Object(gen_cfg));
        }

        // 还原 Gemini 专属顶层字段。
        if let Some(safety) = ir.extension("gemini.safetySettings") {
            out.insert("safetySettings".into(), safety.clone());
        }
        if let Some(cached) = ir.extension("gemini.cachedContent") {
            out.insert("cachedContent".into(), cached.clone());
        }

        // 模型名**不进请求体** —— 官方 `generateContent` 端点对未知顶层
        // 字段直接 400，模型名属于 URL 路径。路由层用自己的候选信息
        // （`upstream_model`）拼 URL，编码产物必须是可以原样发给上游的
        // 合法 wire 格式，不携带任何带外数据。

        Ok(Value::Object(out))
    }
}

/// 解析 Gemini 错误体 `{error:{code, message, status}}`。
///
/// 返回 `None` 表示这不是错误体。`status` 是 Google 的 canonical error code，
/// 比 HTTP 状态码语义更细，所以优先用它判种类。
fn parse_error_body(raw: &Value) -> Option<GatewayError> {
    let err = raw.get("error")?.as_object()?;
    let message = err
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("upstream returned an error without a message");
    let status = err.get("status").and_then(Value::as_str).unwrap_or("");
    let code = err.get("code").and_then(Value::as_u64);

    let kind = match status {
        "INVALID_ARGUMENT" | "FAILED_PRECONDITION" | "OUT_OF_RANGE" => ErrorKind::InvalidRequest,
        "UNAUTHENTICATED" => ErrorKind::Unauthenticated,
        "PERMISSION_DENIED" => ErrorKind::PermissionDenied,
        "NOT_FOUND" => ErrorKind::NotFound,
        "RESOURCE_EXHAUSTED" => ErrorKind::RateLimited,
        "UNAVAILABLE" => ErrorKind::NoAvailableChannel,
        "DEADLINE_EXCEEDED" => ErrorKind::Timeout,
        // status 缺失的中转站很多，退回按 HTTP code 判。
        _ => match code {
            Some(400) => ErrorKind::InvalidRequest,
            Some(401) => ErrorKind::Unauthenticated,
            Some(403) => ErrorKind::PermissionDenied,
            Some(404) => ErrorKind::NotFound,
            Some(429) => ErrorKind::RateLimited,
            Some(503) => ErrorKind::NoAvailableChannel,
            Some(504) => ErrorKind::Timeout,
            _ => ErrorKind::UpstreamError,
        },
    };

    let mut error = GatewayError::new(kind, message).with_protocol(Protocol::Gemini);
    if let Some(code) = code {
        error = error.with_upstream(
            u16::try_from(code).unwrap_or(502),
            serde_json::to_string(raw).unwrap_or_default(),
        );
    }
    Some(error)
}

/// 从一份 `GenerateContentResponse` 里取出第一个候选。
///
/// `candidateCount > 1` 时其余候选无处安放（IR 只有一份 content），
/// 由调用方存进 extensions。
fn first_candidate(obj: &Map<String, Value>) -> Option<&Map<String, Value>> {
    obj.get("candidates")?.as_array()?.first()?.as_object()
}

/// 生成一个响应 ID：优先用上游的 `responseId`。
fn response_id(obj: &Map<String, Value>) -> String {
    non_empty_str(field(obj, "responseId", "response_id"))
        .unwrap_or_else(|| format!("gemini-{}", uuid::Uuid::new_v4()))
}

impl ResponseCodec for GeminiCodec {
    fn decode_response(&self, raw: &Value) -> Result<UnifiedResponse, GatewayError> {
        let obj = raw
            .as_object()
            .ok_or_else(|| GatewayError::invalid_request("response body must be a JSON object"))?;

        // 错误体优先：Gemini 的错误与正常响应共用 200 以外的通道，
        // 但中转站经常用 200 包错误体回来。
        if let Some(err) = parse_error_body(raw) {
            return Err(err);
        }

        let model = non_empty_str(field(obj, "modelVersion", "model_version")).unwrap_or_default();
        let mut out = UnifiedResponse::new(response_id(obj), model);

        let candidate = first_candidate(obj);
        let content = candidate
            .and_then(|c| c.get("content"))
            .and_then(Value::as_object);
        out.content = decode_parts(content.and_then(|c| c.get("parts")));

        let raw_finish = candidate
            .and_then(|c| field(c, "finishReason", "finish_reason"))
            .and_then(Value::as_str);
        let has_tool_use = out
            .content
            .iter()
            .any(|p| matches!(p, ContentPart::ToolUse { .. }));
        out.stop_reason = raw_finish.map(|r| {
            let mapped = stop_reason_from_gemini(r);
            // Gemini 工具调用回合的 finishReason 也是 STOP —— 但下游协议
            // （OpenAI `tool_calls`、Anthropic `tool_use`）需要区分，
            // 所以看到 functionCall 就改判 ToolUse。
            if mapped == StopReason::Stop && has_tool_use {
                StopReason::ToolUse
            } else {
                mapped
            }
        });
        // 细分的安全终止原因（RECITATION / SPII……）在 IR 里都塌成
        // ContentFilter，原值留在 extensions 供 Gemini→Gemini 无损还原。
        if let Some(finish) = raw_finish {
            out.extensions
                .insert("gemini.finishReason".into(), json!(finish));
        }

        out.usage = parse_usage(field(obj, "usageMetadata", "usage_metadata"));

        // 其他协议没有对应物的候选级元数据。
        if let Some(c) = candidate {
            if let Some(ratings) = field(c, "safetyRatings", "safety_ratings") {
                out.extensions
                    .insert("gemini.safetyRatings".into(), ratings.clone());
            }
            if let Some(citations) = field(c, "citationMetadata", "citation_metadata") {
                out.extensions
                    .insert("gemini.citationMetadata".into(), citations.clone());
            }
        }
        // 多候选：IR 只装得下一个，其余原样留存。
        if let Some(all) = obj.get("candidates").and_then(Value::as_array)
            && all.len() > 1
        {
            out.extensions.insert(
                "gemini.extraCandidates".into(),
                Value::Array(all[1..].to_vec()),
            );
        }
        if let Some(feedback) = field(obj, "promptFeedback", "prompt_feedback") {
            out.extensions
                .insert("gemini.promptFeedback".into(), feedback.clone());
            // prompt 被安全策略整体拦截时（blockReason 存在），Gemini 返回
            // 空 candidates + 无 finishReason。不映射的话，下游客户端收到
            // 的是「空内容 + finish_reason: null」，完全没有被拦截的信号。
            if out.stop_reason.is_none()
                && feedback
                    .as_object()
                    .and_then(|f| field(f, "blockReason", "block_reason"))
                    .is_some()
            {
                out.stop_reason = Some(StopReason::ContentFilter);
            }
        }

        Ok(out)
    }

    fn encode_response(&self, ir: &UnifiedResponse) -> Result<Value, GatewayError> {
        // 响应内容里只有 ToolUse 没有 ToolResult，不需要反查映射。
        let parts: Vec<Value> = ir
            .content
            .iter()
            .filter_map(|p| encode_part(p, &HashMap::new()))
            .collect();

        let mut candidate = Map::new();
        candidate.insert("content".into(), json!({ "parts": parts, "role": "model" }));
        // 优先还原上游原始 finishReason（保住 RECITATION 之类的细分原因），
        // 否则从 IR 语义反推。
        let finish = ir
            .extensions
            .get("gemini.finishReason")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| ir.stop_reason.map(|r| stop_reason_to_gemini(r).to_owned()));
        if let Some(finish) = finish {
            candidate.insert("finishReason".into(), json!(finish));
        }
        candidate.insert("index".into(), json!(0));
        if let Some(ratings) = ir.extensions.get("gemini.safetyRatings") {
            candidate.insert("safetyRatings".into(), ratings.clone());
        }
        if let Some(citations) = ir.extensions.get("gemini.citationMetadata") {
            candidate.insert("citationMetadata".into(), citations.clone());
        }

        let mut candidates = vec![Value::Object(candidate)];
        if let Some(Value::Array(extra)) = ir.extensions.get("gemini.extraCandidates") {
            candidates.extend(extra.iter().cloned());
        }

        let mut out = Map::new();
        out.insert("candidates".into(), Value::Array(candidates));
        out.insert("usageMetadata".into(), encode_usage(&ir.usage));
        if !ir.model.is_empty() {
            out.insert("modelVersion".into(), json!(ir.model));
        }
        out.insert("responseId".into(), json!(ir.id));
        if let Some(feedback) = ir.extensions.get("gemini.promptFeedback") {
            out.insert("promptFeedback".into(), feedback.clone());
        }
        // Gemini 没有 stop_sequence 字段；命中的停止序列只能丢。
        if ir.stop_sequence.is_some() {
            tracing::debug!("gemini 响应体无 stop_sequence 字段，已丢弃");
        }

        Ok(Value::Object(out))
    }
}

impl StreamCodec for GeminiCodec {
    fn stream_decoder(&self) -> Box<dyn StreamDecoder> {
        Box::new(GeminiStreamDecoder::default())
    }

    fn stream_encoder(&self) -> Box<dyn StreamEncoder> {
        Box::new(GeminiStreamEncoder::default())
    }
}

impl ProtocolCodec for GeminiCodec {
    fn protocol(&self) -> Protocol {
        Protocol::Gemini
    }
}

/// Gemini 流式解码器（有状态）。
///
/// Gemini 的 SSE（`:streamGenerateContent?alt=sse`）**无事件名**，每个 `data:`
/// 都是一份完整的 `GenerateContentResponse`，且**没有 `[DONE]` 哨兵** ——
/// 流的终结就是连接关闭。所以：
///
/// - 首帧要自己合成 [`StreamEvent::Start`]（Gemini 没有开场事件）。
/// - [`StreamDecoder::finish`] 必须补 [`StreamEvent::Done`]，否则下游编码器
///   永远等不到终结信号。
/// - 块下标要自己分配：Gemini 的 parts 没有跨帧的 index 语义，同一个文本流
///   在每帧里都是 `parts[0]`。我们按「种类连续性」维护下标 —— 连续的文本
///   delta 属于同一个块，出现 functionCall 或切换种类才开新块。
#[derive(Default)]
struct GeminiStreamDecoder {
    /// 是否已发过 Start。
    started: bool,
    /// 当前块下标。
    index: u32,
    /// 当前块的种类，`None` 表示还没开块。
    open: Option<PartKind>,
    /// 是否已发过 Stop（`finishReason` 只该产出一次）。
    stopped: bool,
    /// 是否已发过 Done。
    done: bool,
    /// 本轮是否出现过工具调用，决定 STOP 要不要改判为 ToolUse。
    saw_tool_call: bool,
}

impl GeminiStreamDecoder {
    /// 切换到指定种类的块，必要时关闭上一个块并开新块。
    ///
    /// 返回当前块下标。工具调用每次都强制开新块 —— 两个相邻的 functionCall
    /// 是两次独立调用，不能并进同一个块。
    fn switch(&mut self, kind: PartKind, out: &mut Vec<StreamEvent>) -> u32 {
        let force_new = kind == PartKind::ToolUse;
        match self.open {
            Some(current) if current == kind && !force_new => self.index,
            Some(_) => {
                out.push(StreamEvent::ContentStop { index: self.index });
                self.index += 1;
                self.open = Some(kind);
                out.push(StreamEvent::ContentStart {
                    index: self.index,
                    kind,
                });
                self.index
            }
            None => {
                self.open = Some(kind);
                out.push(StreamEvent::ContentStart {
                    index: self.index,
                    kind,
                });
                self.index
            }
        }
    }
}

impl StreamDecoder for GeminiStreamDecoder {
    fn decode(&mut self, frame: &SseFrame) -> Result<Vec<StreamEvent>, GatewayError> {
        let data = frame.data.trim();
        // Gemini 没有哨兵，但中转站有时会画蛇添足加一个 `[DONE]`。
        if data.is_empty() || data == "[DONE]" {
            return Ok(Vec::new());
        }

        // 非 JSON 帧忽略而不是终止流：中转站会插入裸文本心跳，为一个心跳
        // 丢掉整个回答是不可接受的失败模式。
        let Ok(value) = serde_json::from_str::<Value>(data) else {
            tracing::debug!(bytes = data.len(), "gemini: ignoring non-JSON SSE frame");
            return Ok(Vec::new());
        };

        // 流中错误：Gemini 把错误体直接当成一帧发出来。
        if let Some(err) = parse_error_body(&value) {
            return Ok(vec![StreamEvent::Error {
                message: err.message,
                kind: err.kind.openai_type().to_owned(),
            }]);
        }

        let Some(obj) = value.as_object() else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();

        if !self.started {
            self.started = true;
            out.push(StreamEvent::Start {
                id: response_id(obj),
                model: non_empty_str(field(obj, "modelVersion", "model_version"))
                    .unwrap_or_default(),
                usage: None,
            });
        }

        let candidate = first_candidate(obj);
        let parts = candidate
            .and_then(|c| c.get("content"))
            .and_then(Value::as_object)
            .and_then(|c| c.get("parts"))
            .and_then(Value::as_array);

        for part in parts.into_iter().flatten() {
            match decode_part(part) {
                Some(ContentPart::Text { text }) => {
                    let index = self.switch(PartKind::Text, &mut out);
                    out.push(StreamEvent::TextDelta { index, text });
                }
                Some(ContentPart::Thinking { text, signature }) => {
                    let index = self.switch(PartKind::Thinking, &mut out);
                    out.push(StreamEvent::ThinkingDelta { index, text });
                    // signature 必须无损带走，否则回传给 Anthropic 会被拒。
                    if let Some(signature) = signature {
                        out.push(StreamEvent::ThinkingSignature { index, signature });
                    }
                }
                Some(ContentPart::ToolUse {
                    id,
                    name,
                    input,
                    signature,
                }) => {
                    let index = self.switch(PartKind::ToolUse, &mut out);
                    out.push(StreamEvent::ToolCallStart {
                        index,
                        id,
                        name,
                        signature,
                    });
                    // Gemini 的 functionCall 一次给全 args，没有分片语义，
                    // 所以整份 JSON 一次性发完。
                    out.push(StreamEvent::ToolCallArgsDelta {
                        index,
                        fragment: input.to_string(),
                    });
                    out.push(StreamEvent::ContentStop { index });
                    // 工具调用块已关闭，下一个 part 必须开新块。
                    self.open = None;
                    self.index += 1;
                    self.saw_tool_call = true;
                }
                // 流式帧里出现媒体/工具结果不合语义，忽略。
                Some(_) | None => {}
            }
        }

        if let Some(usage) = field(obj, "usageMetadata", "usage_metadata") {
            out.push(StreamEvent::Usage(parse_usage(Some(usage))));
        }

        let finish = candidate
            .and_then(|c| field(c, "finishReason", "finish_reason"))
            .and_then(Value::as_str);
        if let Some(finish) = finish
            && !self.stopped
        {
            self.stopped = true;
            if let Some(index) = self.open.map(|_| self.index) {
                out.push(StreamEvent::ContentStop { index });
                self.open = None;
            }
            let mapped = stop_reason_from_gemini(finish);
            // 与非流式一致：Gemini 工具调用回合的 finishReason 也是 STOP，
            // 但下游协议要区分，所以本轮发过工具调用就改判 ToolUse。
            let reason = if mapped == StopReason::Stop && self.saw_tool_call {
                StopReason::ToolUse
            } else {
                mapped
            };
            out.push(StreamEvent::Stop {
                reason,
                stop_sequence: None,
            });
        }

        Ok(out)
    }

    fn finish(&mut self) -> Result<Vec<StreamEvent>, GatewayError> {
        let mut out = Vec::new();
        // 关掉还开着的块 —— 上游可能连 finishReason 都没发就断了。
        if self.open.take().is_some() {
            out.push(StreamEvent::ContentStop { index: self.index });
        }
        // Gemini 无哨兵，Done 必须由解码器补，否则下游编码器收不到终结信号。
        if !self.done {
            self.done = true;
            out.push(StreamEvent::Done);
        }
        Ok(out)
    }
}

/// Gemini 流式编码器（有状态）。
///
/// 把统一事件重新组装成 Gemini 的 chunk 序列。Gemini 的流很「朴素」：
/// 没有开场事件、没有块生命周期事件、没有 `[DONE]` 哨兵，每一帧就是一份
/// 完整的 `GenerateContentResponse`。所以编码器的工作主要是**丢弃**上游
/// 协议多出来的仪式性事件，并把 usage 攒到最后一帧。
///
/// 唯一必须补齐的「仪式」是收尾帧：源协议若只发了 `Done` 而没发 `Stop`
/// （中转站常见），仍要产出一个带 `finishReason` 的帧，否则 Gemini 客户端
/// 会认为响应被截断。
#[derive(Default)]
struct GeminiStreamEncoder {
    /// 待写入最后一帧的 usage。
    pending_usage: Option<Usage>,
    /// 已经发过带 finishReason 的收尾帧。
    finished: bool,
    /// 工具调用块的入参缓冲：`ToolCallArgsDelta` 是分片的，而 Gemini 的
    /// `functionCall.args` 必须是完整 JSON 对象，只能攒到块结束再发。
    tool_calls: Vec<Option<ToolCallBuf>>,
}

/// 一个进行中的工具调用块。
struct ToolCallBuf {
    id: String,
    name: String,
    args: String,
    signature: Option<String>,
}

impl GeminiStreamEncoder {
    /// 包装一个 part 成完整的 Gemini chunk。
    fn chunk(parts: Vec<Value>) -> Value {
        json!({
            "candidates": [{
                "content": { "parts": parts, "role": "model" },
                "index": 0,
            }]
        })
    }

    /// 产出一个只含单个 part 的数据帧。
    fn part_frame(part: Value) -> SseFrame {
        // Gemini 的 SSE 无事件名，只有 `data:`。
        SseFrame::data(Self::chunk(vec![part]).to_string())
    }

    /// 合并 usage：逐字段取最大值。
    ///
    /// 与 [`StreamAggregator`](crate::stream::StreamAggregator) 同一策略：
    /// Anthropic 分两次给（Start 给 input、message_delta 给累积 output），
    /// OpenAI 最后一次性给全量，取 max 对两种语义都正确。
    fn merge_usage(&mut self, incoming: Usage) {
        self.pending_usage
            .get_or_insert_with(Usage::default)
            .merge_max(&incoming);
    }

    /// 取出某个下标上的工具调用缓冲。
    fn tool_slot(&mut self, index: u32) -> &mut Option<ToolCallBuf> {
        let idx = index as usize;
        while self.tool_calls.len() <= idx {
            self.tool_calls.push(None);
        }
        &mut self.tool_calls[idx]
    }

    /// 块结束时把攒好的工具调用刷成一帧。
    fn flush_tool_call(&mut self, index: u32) -> Option<SseFrame> {
        let buf = self.tool_slot(index).take()?;
        // 入参可能是被截断的 JSON（流中断），解析失败就退回空对象并保留
        // 原始片段，让下游至少知道调了哪个函数。
        let args = if buf.args.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str::<Value>(&buf.args).unwrap_or_else(|_| {
                tracing::debug!(fragment = %buf.args, "工具入参 JSON 不完整，降级为空对象");
                json!({})
            })
        };
        let mut call = Map::new();
        // 兜底 id（等于函数名）不回传，避免上游拒绝。
        if !buf.id.is_empty() && buf.id != buf.name {
            call.insert("id".into(), json!(buf.id));
        }
        call.insert("name".into(), json!(buf.name));
        call.insert("args".into(), args);
        let mut part = Map::new();
        part.insert("functionCall".into(), Value::Object(call));
        // 思维签名挂 part 级，直通场景必须原样带回。
        if let Some(sig) = buf.signature {
            part.insert("thoughtSignature".into(), json!(sig));
        }
        Some(Self::part_frame(Value::Object(part)))
    }

    /// 构造收尾帧：带 `finishReason`，并把攒下的 usage 一并写入。
    fn stop_frame(&mut self, reason: StopReason) -> SseFrame {
        let mut chunk = json!({
            "candidates": [{
                "content": { "parts": [], "role": "model" },
                "finishReason": stop_reason_to_gemini(reason),
                "index": 0,
            }]
        });
        // usage 只在最后一帧出现，与 Gemini 上游行为一致。
        if let Some(usage) = self.pending_usage.take()
            && let Some(obj) = chunk.as_object_mut()
        {
            obj.insert("usageMetadata".into(), encode_usage(&usage));
        }
        SseFrame::data(chunk.to_string())
    }
}

impl StreamEncoder for GeminiStreamEncoder {
    fn encode(&mut self, event: &StreamEvent) -> Result<Vec<SseFrame>, GatewayError> {
        let frames = match event {
            StreamEvent::TextDelta { text, .. } => {
                vec![Self::part_frame(json!({ "text": text }))]
            }
            StreamEvent::ThinkingDelta { text, .. } => {
                vec![Self::part_frame(json!({ "text": text, "thought": true }))]
            }
            StreamEvent::ThinkingSignature { signature, .. } => {
                // signature 必须无损传下去：单独发一帧空文本推理 part 承载它，
                // 因为它到达时对应的文本帧早已发走。
                vec![Self::part_frame(json!({
                    "text": "",
                    "thought": true,
                    "thoughtSignature": signature,
                }))]
            }
            StreamEvent::RefusalDelta { text, .. } => {
                // Gemini 没有 refusal 概念，降级成普通文本而不是丢弃 ——
                // 拒答理由对用户有意义。
                vec![Self::part_frame(json!({ "text": text }))]
            }
            StreamEvent::ToolCallStart {
                index,
                id,
                name,
                signature,
            } => {
                *self.tool_slot(*index) = Some(ToolCallBuf {
                    id: id.clone(),
                    name: name.clone(),
                    args: String::new(),
                    signature: signature.clone(),
                });
                Vec::new()
            }
            StreamEvent::ToolCallArgsDelta { index, fragment } => {
                // 缺 ToolCallStart 也要宽容（中转站常省仪式性事件）：
                // 没有槽位就开一个匿名的攒着，ContentStop 时至少能发出 args。
                let slot = self.tool_slot(*index).get_or_insert_with(|| ToolCallBuf {
                    id: String::new(),
                    name: String::new(),
                    args: String::new(),
                    signature: None,
                });
                slot.args.push_str(fragment);
                Vec::new()
            }
            StreamEvent::ContentStop { index } => {
                self.flush_tool_call(*index).into_iter().collect()
            }
            StreamEvent::Usage(usage) => {
                // 攒着，等收尾帧一起发 —— Gemini 的 usageMetadata 挂在
                // chunk 顶层，单独发一帧空 candidates 会被客户端当成空回复。
                // 合并而非覆盖：Anthropic 的 message_delta usage 只带 output
                // （input 在 Start 里），覆盖会把 input 归零。
                self.merge_usage(*usage);
                Vec::new()
            }
            StreamEvent::Stop { reason, .. } => {
                self.finished = true;
                vec![self.stop_frame(*reason)]
            }
            StreamEvent::Error { message, kind } => {
                vec![SseFrame::data(
                    json!({
                        "error": {
                            "code": 500,
                            "message": message,
                            "status": kind,
                        }
                    })
                    .to_string(),
                )]
            }
            StreamEvent::Start { usage, .. } => {
                // Anthropic 在 message_start 就给 input_tokens，攒进收尾帧，
                // 否则 anthropic→gemini 的流式 promptTokenCount 永远是 0。
                if let Some(usage) = usage {
                    self.merge_usage(*usage);
                }
                Vec::new()
            }
            // Gemini 无哨兵：Done 不产出任何帧，流的终结就是连接关闭。
            // ContentStart / Ping 是别的协议的仪式，Gemini 不需要。
            StreamEvent::Done | StreamEvent::ContentStart { .. } | StreamEvent::Ping => Vec::new(),
        };
        Ok(frames)
    }

    fn finish(&mut self) -> Result<Vec<SseFrame>, GatewayError> {
        let mut out = Vec::new();
        // 刷掉没等到 ContentStop 的工具调用（流被截断或上游省了事件）。
        for index in 0..self.tool_calls.len() as u32 {
            if let Some(frame) = self.flush_tool_call(index) {
                out.push(frame);
            }
        }
        // 源协议只发了 Done 没发 Stop 时，仍要补一个带 finishReason 的收尾帧，
        // 否则 Gemini 客户端会把响应当成被截断。usage 也在这一帧里。
        if !self.finished {
            self.finished = true;
            out.push(self.stop_frame(StopReason::Stop));
        } else if self.pending_usage.is_some() {
            // Stop 之后才到的 usage（OpenAI 的 `stream_options` 就是这个顺序）
            // 单独补一帧。
            let usage = self.pending_usage.take().unwrap_or_default();
            out.push(SseFrame::data(
                json!({
                    "candidates": [{
                        "content": { "parts": [], "role": "model" },
                        "index": 0,
                    }],
                    "usageMetadata": encode_usage(&usage),
                })
                .to_string(),
            ));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    /// 跑一遍解码，失败直接 panic —— 测试里不关心错误包装。
    fn decode_req(raw: Value) -> UnifiedRequest {
        GEMINI.decode_request(&raw).expect("decode_request failed")
    }

    fn encode_req(ir: &UnifiedRequest) -> Value {
        GEMINI.encode_request(ir).expect("encode_request failed")
    }

    fn decode_resp(raw: Value) -> UnifiedResponse {
        GEMINI
            .decode_response(&raw)
            .expect("decode_response failed")
    }

    /// 把一串 SSE data 喂给解码器，收集全部事件（含 finish 补的尾巴）。
    fn drive_decoder(chunks: &[Value]) -> Vec<StreamEvent> {
        let mut dec = GEMINI.stream_decoder();
        let mut events = Vec::new();
        for chunk in chunks {
            let frame = SseFrame::data(chunk.to_string());
            events.extend(dec.decode(&frame).expect("decode frame failed"));
        }
        events.extend(dec.finish().expect("decoder finish failed"));
        events
    }

    /// 把一串事件喂给编码器，收集全部帧（含 finish 补的尾巴）。
    fn drive_encoder(events: &[StreamEvent]) -> Vec<Value> {
        let mut enc = GEMINI.stream_encoder();
        let mut frames = Vec::new();
        for event in events {
            frames.extend(enc.encode(event).expect("encode event failed"));
        }
        frames.extend(enc.finish().expect("encoder finish failed"));
        frames
            .into_iter()
            .map(|f| {
                assert_eq!(f.event, None, "gemini SSE 不该有事件名");
                serde_json::from_str::<Value>(&f.data).expect("frame data 不是合法 JSON")
            })
            .collect()
    }

    #[test]
    fn protocol_is_gemini() {
        assert_eq!(GEMINI.protocol(), Protocol::Gemini);
    }

    #[test]
    fn decode_basic_request_without_model_field() {
        // 模型名在 URL 里，请求体没有 model —— 这是 Gemini 的常态，不能报错。
        let ir = decode_req(json!({
            "contents": [{ "role": "user", "parts": [{ "text": "你好" }] }]
        }));
        assert_eq!(ir.model, "");
        assert_eq!(ir.messages.len(), 1);
        assert_eq!(ir.messages[0].role, Role::User);
        assert_eq!(ir.messages[0].text_content(), "你好");
    }

    #[test]
    fn decode_request_reads_model_when_present() {
        // 中转站会把 model 塞进请求体，有就读。
        let ir = decode_req(json!({
            "model": "gemini-2.5-pro",
            "contents": [{ "role": "user", "parts": [{ "text": "hi" }] }]
        }));
        assert_eq!(ir.model, "gemini-2.5-pro");
    }

    #[test]
    fn encode_request_never_puts_model_in_body() {
        // 官方 generateContent 端点会拒绝请求体里的 model 字段；
        // model 只用于上层拼 URL，不进请求体。
        let ir = UnifiedRequest::new("gemini-2.5-flash", vec![Message::text(Role::User, "hi")]);
        let out = encode_req(&ir);
        assert_eq!(out.get("model"), None, "model 绝不能出现在请求体里");
    }

    #[test]
    fn assistant_role_is_named_model() {
        let ir = UnifiedRequest::new(
            "m",
            vec![
                Message::text(Role::User, "问"),
                Message::text(Role::Assistant, "答"),
            ],
        );
        let out = encode_req(&ir);
        assert_eq!(out["contents"][0]["role"], json!("user"));
        assert_eq!(
            out["contents"][1]["role"],
            json!("model"),
            "Gemini 用 model 而非 assistant"
        );

        // 反向：role:"model" 要解回 Assistant。
        let back = decode_req(out.clone());
        assert_eq!(back.messages[1].role, Role::Assistant);
    }

    #[test]
    fn consecutive_same_role_messages_are_merged() {
        // Gemini 要求 user/model 交替，连着两个 user 会被上游拒绝。
        let ir = UnifiedRequest::new(
            "m",
            vec![
                Message::text(Role::User, "第一句"),
                Message::text(Role::User, "第二句"),
                Message::text(Role::Assistant, "回复"),
            ],
        );
        let out = encode_req(&ir);
        let contents = out["contents"].as_array().expect("contents 应是数组");
        assert_eq!(contents.len(), 2, "两条 user 应合并成一个回合");
        assert_eq!(contents[0]["parts"].as_array().map(Vec::len), Some(2));
        assert_eq!(contents[0]["parts"][1]["text"], json!("第二句"));
        assert_eq!(contents[1]["role"], json!("model"));
    }

    #[test]
    fn system_instruction_round_trip() {
        let ir = decode_req(json!({
            "systemInstruction": { "parts": [{ "text": "你是助手" }] },
            "contents": [{ "role": "user", "parts": [{ "text": "hi" }] }]
        }));
        assert_eq!(ir.system_text(), "你是助手");
        // system 不该混进 messages。
        assert_eq!(ir.messages.len(), 1);

        let out = encode_req(&ir);
        assert_eq!(
            out["systemInstruction"]["parts"][0]["text"],
            json!("你是助手")
        );
    }

    #[test]
    fn multimodal_parts_split_by_mime_prefix() {
        let ir = decode_req(json!({
            "contents": [{
                "role": "user",
                "parts": [
                    { "inlineData": { "mimeType": "image/png", "data": "AAAA" } },
                    { "inlineData": { "mimeType": "audio/mp3", "data": "BBBB" } },
                    { "fileData": { "mimeType": "application/pdf", "fileUri": "files/x1" } }
                ]
            }]
        }));
        let parts = &ir.messages[0].content;
        assert_eq!(
            parts[0],
            ContentPart::Image {
                source: MediaSource::Base64("AAAA".into()),
                mime: Some("image/png".into()),
                detail: None,
            }
        );
        assert_eq!(
            parts[1],
            ContentPart::Audio {
                source: MediaSource::Base64("BBBB".into()),
                format: Some("mp3".into()),
            }
        );
        assert_eq!(
            parts[2],
            ContentPart::File {
                source: MediaSource::FileId("files/x1".into()),
                mime: Some("application/pdf".into()),
                name: None,
            }
        );

        // 编码回去：base64 走 inlineData，fileId 走 fileData。
        let out = encode_req(&ir);
        let encoded = &out["contents"][0]["parts"];
        assert_eq!(encoded[0]["inlineData"]["mimeType"], json!("image/png"));
        assert_eq!(encoded[1]["inlineData"]["mimeType"], json!("audio/mp3"));
        assert_eq!(encoded[2]["fileData"]["fileUri"], json!("files/x1"));
    }

    #[test]
    fn tools_are_nested_arrays() {
        // Gemini 的 tools 是数组套数组：[{functionDeclarations:[...]}]。
        let ir = decode_req(json!({
            "contents": [{ "role": "user", "parts": [{ "text": "天气" }] }],
            "tools": [{
                "functionDeclarations": [
                    { "name": "get_weather", "description": "查天气",
                      "parameters": { "type": "object", "properties": { "city": { "type": "string" } } } },
                    { "name": "get_time", "parameters": { "type": "object" } }
                ]
            }]
        }));
        assert_eq!(ir.tools.len(), 2, "两个函数声明应展平成两个 ToolDef");
        assert_eq!(ir.tools[0].name, "get_weather");
        assert_eq!(ir.tools[0].description.as_deref(), Some("查天气"));
        assert_eq!(ir.tools[1].name, "get_time");
        assert_eq!(ir.tools[1].description, None);

        // 编码回去要重新套上外层数组。
        let out = encode_req(&ir);
        let tools = out["tools"].as_array().expect("tools 应是数组");
        assert_eq!(tools.len(), 1, "所有声明装进一个 Tool 对象");
        let decls = tools[0]["functionDeclarations"]
            .as_array()
            .expect("functionDeclarations 应是数组");
        assert_eq!(decls.len(), 2);
        assert_eq!(decls[0]["name"], json!("get_weather"));
    }

    #[test]
    fn function_call_without_id_falls_back_to_name() {
        // Gemini 的 functionCall 常常不带 id，但 IR 靠 id 关联工具结果。
        let ir = decode_req(json!({
            "contents": [{
                "role": "model",
                "parts": [{ "functionCall": { "name": "get_weather", "args": { "city": "上海" } } }]
            }]
        }));
        assert_eq!(
            ir.messages[0].content[0],
            ContentPart::ToolUse {
                signature: None,
                id: "get_weather".into(),
                name: "get_weather".into(),
                input: json!({ "city": "上海" }),
            },
            "缺 id 时应回退用函数名"
        );

        // 兜底 id 不该被回传给上游（它是我们造的）。
        let out = encode_req(&ir);
        let call = &out["contents"][0]["parts"][0]["functionCall"];
        assert_eq!(call.get("id"), None, "兜底 id 不回传");
        assert_eq!(call["name"], json!("get_weather"));
    }

    #[test]
    fn function_call_keeps_real_id() {
        let ir = decode_req(json!({
            "contents": [{
                "role": "model",
                "parts": [{ "functionCall": { "id": "call_7", "name": "f", "args": {} } }]
            }]
        }));
        let ContentPart::ToolUse { id, .. } = &ir.messages[0].content[0] else {
            panic!("应解析为 ToolUse");
        };
        assert_eq!(id, "call_7");
        // 真实 id 必须原样回传。
        let out = encode_req(&ir);
        assert_eq!(
            out["contents"][0]["parts"][0]["functionCall"]["id"],
            json!("call_7")
        );
    }

    #[test]
    fn tool_call_three_phase_round_trip() {
        // 声明 → 调用 → 回传，三段式完整走一遍。
        let raw = json!({
            "contents": [
                { "role": "user", "parts": [{ "text": "上海天气" }] },
                { "role": "model", "parts": [{ "functionCall": { "name": "get_weather", "args": { "city": "上海" } } }] },
                { "role": "user", "parts": [{ "functionResponse": { "name": "get_weather", "response": { "temp": 25 } } }] }
            ],
            "tools": [{ "functionDeclarations": [{ "name": "get_weather", "parameters": { "type": "object" } }] }]
        });
        let ir = decode_req(raw);
        assert_eq!(ir.messages[1].role, Role::Assistant);
        // 只含 functionResponse 的 user 回合应提升为 Tool 角色。
        assert_eq!(ir.messages[2].role, Role::Tool, "工具结果回合应识别为 Tool");
        assert_eq!(
            ir.messages[2].content[0],
            ContentPart::ToolResult {
                // functionResponse.name 就是函数名，必须保留 —— 编码回
                // Gemini 时 name 不匹配声明会被上游拒绝。
                name: Some("get_weather".into()),
                id: "get_weather".into(),
                content: vec![ContentPart::text(r#"{"temp":25}"#)],
                is_error: false,
            }
        );

        // 编码回去：Tool 角色要变成 role:"user" + functionResponse。
        let out = encode_req(&ir);
        let third = &out["contents"][2];
        assert_eq!(
            third["role"],
            json!("user"),
            "工具结果在 Gemini 里属于 user 回合"
        );
        assert_eq!(
            third["parts"][0]["functionResponse"]["name"],
            json!("get_weather")
        );
        assert_eq!(
            third["parts"][0]["functionResponse"]["response"],
            json!({ "temp": 25 }),
            "结构化 response 应还原成对象而非字符串"
        );
    }

    #[test]
    fn thinking_signature_survives_round_trip() {
        // signature 丢失会让 Anthropic 上游拒绝整个多轮请求。
        let ir = decode_req(json!({
            "contents": [{
                "role": "model",
                "parts": [{ "text": "推理中", "thought": true, "thoughtSignature": "SIG-abc" }]
            }]
        }));
        assert_eq!(
            ir.messages[0].content[0],
            ContentPart::Thinking {
                text: "推理中".into(),
                signature: Some("SIG-abc".into()),
            }
        );
        let out = encode_req(&ir);
        let part = &out["contents"][0]["parts"][0];
        assert_eq!(part["thought"], json!(true));
        assert_eq!(part["thoughtSignature"], json!("SIG-abc"));
    }

    #[test]
    fn generation_config_and_reasoning_round_trip() {
        let ir = decode_req(json!({
            "contents": [{ "role": "user", "parts": [{ "text": "hi" }] }],
            "generationConfig": {
                "temperature": 0.7,
                "topP": 0.9,
                "topK": 40,
                "maxOutputTokens": 2048,
                "stopSequences": ["END", "STOP"],
                "candidateCount": 1,
                "seed": 42,
                "thinkingConfig": { "thinkingBudget": 1024, "includeThoughts": true }
            }
        }));
        assert_eq!(ir.sampling.temperature, Some(0.7));
        assert_eq!(ir.sampling.top_p, Some(0.9));
        assert_eq!(ir.sampling.top_k, Some(40));
        assert_eq!(ir.sampling.stop, vec!["END".to_owned(), "STOP".to_owned()]);
        assert_eq!(ir.sampling.seed, Some(42));
        assert_eq!(ir.sampling.candidate_count, Some(1));
        assert_eq!(ir.max_output_tokens, Some(2048));
        let reasoning = ir.reasoning.as_ref().expect("应解析出 thinkingConfig");
        assert_eq!(reasoning.budget_tokens, Some(1024));
        assert_eq!(reasoning.include_thoughts, Some(true));

        let out = encode_req(&ir);
        let gen_cfg = &out["generationConfig"];
        assert_eq!(gen_cfg["temperature"], json!(0.7));
        assert_eq!(gen_cfg["topK"], json!(40));
        assert_eq!(gen_cfg["maxOutputTokens"], json!(2048));
        assert_eq!(gen_cfg["stopSequences"], json!(["END", "STOP"]));
        assert_eq!(gen_cfg["thinkingConfig"]["thinkingBudget"], json!(1024));
        assert_eq!(gen_cfg["thinkingConfig"]["includeThoughts"], json!(true));
    }

    #[test]
    fn reasoning_effort_converts_to_thinking_budget() {
        // 从 OpenAI 转过来只有档位，不折算成预算会静默关掉思考功能。
        let mut ir = UnifiedRequest::new("m", vec![Message::text(Role::User, "hi")]);
        ir.max_output_tokens = Some(10_000);
        ir.reasoning = Some(ReasoningConfig {
            effort: Some("high".into()),
            budget_tokens: None,
            include_thoughts: None,
        });
        let out = encode_req(&ir);
        assert_eq!(
            out["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            json!(8_000),
            "high 档应折算成 max_output 的 4/5"
        );
    }

    #[test]
    fn response_format_json_schema_and_object() {
        let with_schema = decode_req(json!({
            "contents": [{ "role": "user", "parts": [{ "text": "hi" }] }],
            "generationConfig": {
                "responseMimeType": "application/json",
                "responseSchema": { "type": "object", "properties": { "a": { "type": "string" } } }
            }
        }));
        assert_eq!(
            with_schema.response_format,
            Some(ResponseFormat::JsonSchema {
                name: "response".into(),
                schema: json!({ "type": "object", "properties": { "a": { "type": "string" } } }),
                strict: true,
            })
        );

        // 只有 mimeType 没有 schema → JsonObject。
        let without_schema = decode_req(json!({
            "contents": [{ "role": "user", "parts": [{ "text": "hi" }] }],
            "generationConfig": { "responseMimeType": "application/json" }
        }));
        assert_eq!(
            without_schema.response_format,
            Some(ResponseFormat::JsonObject)
        );

        let out = encode_req(&with_schema);
        assert_eq!(
            out["generationConfig"]["responseMimeType"],
            json!("application/json")
        );
        assert_eq!(
            out["generationConfig"]["responseSchema"]["type"],
            json!("object")
        );
    }

    #[test]
    fn tool_config_modes_map_to_tool_choice() {
        let mode = |cfg: Value| {
            decode_req(json!({
                "contents": [{ "role": "user", "parts": [{ "text": "hi" }] }],
                "toolConfig": { "functionCallingConfig": cfg }
            }))
            .tool_choice
        };
        assert_eq!(mode(json!({ "mode": "AUTO" })), ToolChoice::Auto);
        assert_eq!(mode(json!({ "mode": "NONE" })), ToolChoice::None);
        assert_eq!(mode(json!({ "mode": "ANY" })), ToolChoice::Required);
        // ANY + 恰好一个允许函数名 = 强制调用该工具。
        assert_eq!(
            mode(json!({ "mode": "ANY", "allowedFunctionNames": ["f"] })),
            ToolChoice::Tool("f".into())
        );

        // 反向编码。
        let mut ir = UnifiedRequest::new("m", vec![Message::text(Role::User, "hi")]);
        ir.tool_choice = ToolChoice::Tool("f".into());
        let out = encode_req(&ir);
        let fc = &out["toolConfig"]["functionCallingConfig"];
        assert_eq!(fc["mode"], json!("ANY"));
        assert_eq!(fc["allowedFunctionNames"], json!(["f"]));
    }

    #[test]
    fn safety_settings_and_unknown_fields_go_to_extensions() {
        let ir = decode_req(json!({
            "contents": [{ "role": "user", "parts": [{ "text": "hi" }] }],
            "safetySettings": [{ "category": "HARM_CATEGORY_HARASSMENT", "threshold": "BLOCK_NONE" }],
            "cachedContent": "cachedContents/abc",
            "labels": { "team": "search" }
        }));
        assert_eq!(
            ir.extension("gemini.safetySettings"),
            Some(&json!([{ "category": "HARM_CATEGORY_HARASSMENT", "threshold": "BLOCK_NONE" }]))
        );
        assert_eq!(
            ir.extension("gemini.cachedContent"),
            Some(&json!("cachedContents/abc"))
        );
        // 完全未知的字段也要进 extensions 而不是报错。
        assert_eq!(
            ir.extension("gemini.labels"),
            Some(&json!({ "team": "search" }))
        );

        // 都要能还原回去。
        let out = encode_req(&ir);
        assert_eq!(out["safetySettings"][0]["threshold"], json!("BLOCK_NONE"));
        assert_eq!(out["cachedContent"], json!("cachedContents/abc"));
    }

    #[test]
    fn missing_contents_is_invalid_request() {
        let err = GEMINI
            .decode_request(&json!({ "generationConfig": {} }))
            .expect_err("缺 contents 必须报错");
        assert_eq!(err.kind, ErrorKind::InvalidRequest);
        assert!(
            err.message.contains("contents"),
            "错误消息要指明缺哪个字段，实际: {}",
            err.message
        );

        // contents 类型不对也要报错，而不是静默当空。
        let err = GEMINI
            .decode_request(&json!({ "contents": "oops" }))
            .expect_err("contents 非数组必须报错");
        assert_eq!(err.kind, ErrorKind::InvalidRequest);
    }

    #[test]
    fn decode_response_maps_usage_and_id() {
        let resp = decode_resp(json!({
            "candidates": [{
                "content": { "parts": [{ "text": "答案" }], "role": "model" },
                "finishReason": "STOP",
                "index": 0
            }],
            "usageMetadata": {
                "promptTokenCount": 12,
                "candidatesTokenCount": 34,
                "cachedContentTokenCount": 5,
                "thoughtsTokenCount": 7,
                "totalTokenCount": 53
            },
            "modelVersion": "gemini-2.5-pro",
            "responseId": "resp-123"
        }));
        assert_eq!(resp.id, "resp-123");
        assert_eq!(resp.model, "gemini-2.5-pro");
        assert_eq!(resp.text(), "答案");
        assert_eq!(resp.stop_reason, Some(StopReason::Stop));
        assert_eq!(
            resp.usage,
            Usage {
                input_tokens: 12,
                // IR 口径：output 含 reasoning（34 + 7），与 OpenAI/Anthropic
                // 对齐，转出去时计费才不失真。
                output_tokens: 41,
                cached_input_tokens: 5,
                cache_write_tokens: 0,
                reasoning_tokens: 7,
            }
        );
    }

    #[test]
    fn response_id_is_generated_when_absent() {
        let resp = decode_resp(json!({
            "candidates": [{ "content": { "parts": [{ "text": "x" }], "role": "model" } }]
        }));
        assert!(
            resp.id.starts_with("gemini-"),
            "缺 responseId 时要自造，实际: {}",
            resp.id
        );
        assert!(resp.id.len() > "gemini-".len(), "自造 ID 要带 uuid");
    }

    #[test]
    fn finish_reason_maps_all_variants() {
        let cases = [
            ("STOP", StopReason::Stop),
            ("MAX_TOKENS", StopReason::MaxTokens),
            ("SAFETY", StopReason::ContentFilter),
            ("RECITATION", StopReason::ContentFilter),
            ("BLOCKLIST", StopReason::ContentFilter),
            ("PROHIBITED_CONTENT", StopReason::ContentFilter),
            ("SPII", StopReason::ContentFilter),
            ("MALFORMED_FUNCTION_CALL", StopReason::Other),
            ("OTHER", StopReason::Other),
            ("FUTURE_UNKNOWN_REASON", StopReason::Other),
        ];
        for (raw, expected) in cases {
            let resp = decode_resp(json!({
                "candidates": [{
                    "content": { "parts": [{ "text": "x" }], "role": "model" },
                    "finishReason": raw
                }]
            }));
            assert_eq!(
                resp.stop_reason,
                Some(expected),
                "finishReason {raw} 映射错误"
            );
        }
    }

    #[test]
    fn stop_with_function_call_becomes_tool_use() {
        // Gemini 工具调用回合的 finishReason 也是 STOP，但下游协议要区分。
        let resp = decode_resp(json!({
            "candidates": [{
                "content": { "parts": [{ "functionCall": { "name": "f", "args": {} } }], "role": "model" },
                "finishReason": "STOP"
            }]
        }));
        assert_eq!(resp.stop_reason, Some(StopReason::ToolUse));

        // 反向：ToolUse 要编码回 STOP，因为 Gemini 没有这个原因。
        let mut ir = UnifiedResponse::new("id", "m");
        ir.stop_reason = Some(StopReason::ToolUse);
        ir.content = vec![ContentPart::ToolUse {
            signature: None,
            id: "f".into(),
            name: "f".into(),
            input: json!({}),
        }];
        let out = GEMINI.encode_response(&ir).expect("encode_response failed");
        assert_eq!(out["candidates"][0]["finishReason"], json!("STOP"));
    }

    #[test]
    fn encode_response_shape_is_gemini_native() {
        let mut ir = UnifiedResponse::new("resp-9", "gemini-2.5-flash");
        ir.content = vec![ContentPart::text("你好")];
        ir.stop_reason = Some(StopReason::MaxTokens);
        ir.usage = Usage {
            input_tokens: 3,
            output_tokens: 4,
            cached_input_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: 2,
        };
        let out = GEMINI.encode_response(&ir).expect("encode_response failed");
        assert_eq!(out["candidates"][0]["content"]["role"], json!("model"));
        assert_eq!(
            out["candidates"][0]["content"]["parts"][0]["text"],
            json!("你好")
        );
        assert_eq!(out["candidates"][0]["finishReason"], json!("MAX_TOKENS"));
        assert_eq!(out["candidates"][0]["index"], json!(0));
        assert_eq!(out["usageMetadata"]["promptTokenCount"], json!(3));
        // IR output(4) 含 reasoning(2)，Gemini 的 candidates 口径要减回去。
        assert_eq!(out["usageMetadata"]["candidatesTokenCount"], json!(2));
        assert_eq!(out["usageMetadata"]["thoughtsTokenCount"], json!(2));
        assert_eq!(out["usageMetadata"]["totalTokenCount"], json!(7));
        assert_eq!(out["modelVersion"], json!("gemini-2.5-flash"));
        assert_eq!(out["responseId"], json!("resp-9"));
    }

    #[test]
    fn error_body_is_parsed_by_status() {
        let cases = [
            ("INVALID_ARGUMENT", 400, ErrorKind::InvalidRequest),
            ("PERMISSION_DENIED", 403, ErrorKind::PermissionDenied),
            ("RESOURCE_EXHAUSTED", 429, ErrorKind::RateLimited),
            ("UNAVAILABLE", 503, ErrorKind::NoAvailableChannel),
        ];
        for (status, code, expected) in cases {
            let err = GEMINI
                .decode_response(&json!({
                    "error": { "code": code, "message": "boom", "status": status }
                }))
                .expect_err("错误体必须解析成 Err");
            assert_eq!(err.kind, expected, "status {status} 映射错误");
            assert_eq!(err.message, "boom");
            assert_eq!(err.protocol, Some(Protocol::Gemini));
            assert_eq!(err.upstream_status, Some(code));
        }
    }

    #[test]
    fn stream_decodes_text_chunks_and_emits_done_without_sentinel() {
        // Gemini 没有 [DONE] 哨兵，Done 必须由 finish() 补出来。
        let events = drive_decoder(&[
            json!({
                "candidates": [{ "content": { "parts": [{ "text": "你" }], "role": "model" } }],
                "modelVersion": "gemini-2.5-pro",
                "responseId": "r1"
            }),
            json!({
                "candidates": [{ "content": { "parts": [{ "text": "好" }], "role": "model" } }]
            }),
            json!({
                "candidates": [{ "content": { "parts": [] , "role": "model" }, "finishReason": "STOP" }],
                "usageMetadata": { "promptTokenCount": 2, "candidatesTokenCount": 2 }
            }),
        ]);

        assert_eq!(
            events[0],
            StreamEvent::Start {
                id: "r1".into(),
                model: "gemini-2.5-pro".into(),
                usage: None,
            },
            "首帧要自己合成 Start"
        );
        assert_eq!(
            events[1],
            StreamEvent::ContentStart {
                index: 0,
                kind: PartKind::Text
            }
        );
        assert_eq!(
            events[2],
            StreamEvent::TextDelta {
                index: 0,
                text: "你".into()
            }
        );
        // 第二帧是同一个文本块的延续，不该再开新块。
        assert_eq!(
            events[3],
            StreamEvent::TextDelta {
                index: 0,
                text: "好".into()
            }
        );
        assert!(
            events.contains(&StreamEvent::Usage(Usage {
                input_tokens: 2,
                output_tokens: 2,
                ..Usage::default()
            })),
            "usageMetadata 要产出 Usage 事件"
        );
        assert!(events.contains(&StreamEvent::Stop {
            reason: StopReason::Stop,
            stop_sequence: None,
        }));
        assert_eq!(
            events.last(),
            Some(&StreamEvent::Done),
            "Gemini 无哨兵，finish() 必须补 Done"
        );
    }

    #[test]
    fn stream_decodes_thinking_and_function_call() {
        let events = drive_decoder(&[
            json!({
                "candidates": [{ "content": {
                    "parts": [{ "text": "想一下", "thought": true, "thoughtSignature": "S1" }],
                    "role": "model"
                } }]
            }),
            json!({
                "candidates": [{
                    "content": { "parts": [{ "functionCall": { "name": "f", "args": { "a": 1 } } }], "role": "model" },
                    "finishReason": "STOP"
                }]
            }),
        ]);

        assert!(events.contains(&StreamEvent::ThinkingDelta {
            index: 0,
            text: "想一下".into()
        }));
        assert!(
            events.contains(&StreamEvent::ThinkingSignature {
                index: 0,
                signature: "S1".into()
            }),
            "thoughtSignature 必须无损传出"
        );
        // 工具调用要开新块，且 args 一次性发完整 JSON。
        assert!(events.contains(&StreamEvent::ToolCallStart {
            signature: None,
            index: 1,
            id: "f".into(),
            name: "f".into()
        }));
        assert!(events.contains(&StreamEvent::ToolCallArgsDelta {
            index: 1,
            fragment: r#"{"a":1}"#.into()
        }));
        // 有工具调用时 STOP 要改判 ToolUse。
        assert!(events.contains(&StreamEvent::Stop {
            reason: StopReason::ToolUse,
            stop_sequence: None,
        }));
    }

    #[test]
    fn stream_decoder_tolerates_junk_and_stray_sentinel() {
        let mut dec = GEMINI.stream_decoder();
        // 空帧与画蛇添足的 [DONE]（某些中转站会发）都不该报错。
        assert_eq!(
            dec.decode(&SseFrame::data("")).expect("空帧不该报错"),
            vec![]
        );
        assert_eq!(
            dec.decode(&SseFrame::data("[DONE]"))
                .expect("[DONE] 不该报错"),
            vec![]
        );
        // 流中错误体要变成 Error 事件而不是 Err。
        let err_events = dec
            .decode(&SseFrame::data(
                json!({ "error": { "code": 429, "message": "quota", "status": "RESOURCE_EXHAUSTED" } })
                    .to_string(),
            ))
            .expect("错误帧应产出 Error 事件而非 Err");
        assert_eq!(
            err_events,
            vec![StreamEvent::Error {
                message: "quota".into(),
                kind: "rate_limit_error".into(),
            }]
        );
        // 坏 JSON 也只是跳过：中转站的裸文本心跳不该让整个回答消失。
        assert_eq!(
            dec.decode(&SseFrame::data("{not json"))
                .expect("坏 JSON 不该终止流"),
            vec![]
        );
        // 且跳过之后仍能解析真实内容。
        let after = dec
            .decode(&SseFrame::data(
                json!({ "candidates": [ { "content": { "parts": [ { "text": "hi" } ] } } ] })
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
    fn stream_encoder_emits_complete_chunks_without_sentinel() {
        let frames = drive_encoder(&[
            StreamEvent::Start {
                id: "r".into(),
                model: "m".into(),
                usage: None,
            },
            StreamEvent::ContentStart {
                index: 0,
                kind: PartKind::Text,
            },
            StreamEvent::TextDelta {
                index: 0,
                text: "你好".into(),
            },
            StreamEvent::ContentStop { index: 0 },
            StreamEvent::Usage(Usage {
                input_tokens: 5,
                output_tokens: 6,
                ..Usage::default()
            }),
            StreamEvent::Stop {
                reason: StopReason::Stop,
                stop_sequence: None,
            },
            StreamEvent::Done,
        ]);

        // Start / ContentStart / ContentStop / Done 都不产出帧。
        assert_eq!(frames.len(), 2, "只该有一个 text 帧和一个收尾帧");
        assert_eq!(
            frames[0]["candidates"][0]["content"]["parts"][0]["text"],
            json!("你好")
        );
        assert_eq!(
            frames[0]["candidates"][0]["content"]["role"],
            json!("model")
        );
        assert_eq!(frames[0]["candidates"][0]["index"], json!(0));
        // usage 攒到收尾帧一起发。
        assert_eq!(frames[1]["candidates"][0]["finishReason"], json!("STOP"));
        assert_eq!(frames[1]["usageMetadata"]["promptTokenCount"], json!(5));
        assert_eq!(frames[1]["usageMetadata"]["candidatesTokenCount"], json!(6));
    }

    #[test]
    fn stream_encoder_buffers_tool_call_args() {
        // 分片的 args 必须攒成完整 JSON 才能发 —— Gemini 的 args 是对象不是字符串。
        let frames = drive_encoder(&[
            StreamEvent::ToolCallStart {
                signature: None,
                index: 0,
                id: "call_1".into(),
                name: "get_weather".into(),
            },
            StreamEvent::ToolCallArgsDelta {
                index: 0,
                fragment: r#"{"city":"#.into(),
            },
            StreamEvent::ToolCallArgsDelta {
                index: 0,
                fragment: r#""上海"}"#.into(),
            },
            StreamEvent::ContentStop { index: 0 },
            StreamEvent::Stop {
                reason: StopReason::ToolUse,
                stop_sequence: None,
            },
        ]);

        let call = &frames[0]["candidates"][0]["content"]["parts"][0]["functionCall"];
        assert_eq!(call["name"], json!("get_weather"));
        assert_eq!(call["id"], json!("call_1"));
        assert_eq!(
            call["args"],
            json!({ "city": "上海" }),
            "分片入参应拼成对象"
        );
        // ToolUse 在 Gemini 里就是 STOP。
        assert_eq!(frames[1]["candidates"][0]["finishReason"], json!("STOP"));
    }

    #[test]
    fn stream_encoder_handles_truncated_tool_args_and_missing_stop() {
        // 上游流被截断：args 不完整、没有 ContentStop、没有 Stop。
        let frames = drive_encoder(&[
            StreamEvent::ToolCallStart {
                signature: None,
                index: 0,
                id: "x".into(),
                name: "f".into(),
            },
            StreamEvent::ToolCallArgsDelta {
                index: 0,
                fragment: r#"{"a":"#.into(),
            },
            StreamEvent::Done,
        ]);

        // finish() 要刷出未闭合的工具调用，入参降级为空对象而非丢帧。
        let call = &frames[0]["candidates"][0]["content"]["parts"][0]["functionCall"];
        assert_eq!(call["name"], json!("f"));
        assert_eq!(call["args"], json!({}), "截断的入参降级为空对象");
        // 没收到 Stop 也要补收尾帧，否则客户端认为响应被截断。
        assert_eq!(
            frames.last().expect("应有收尾帧")["candidates"][0]["finishReason"],
            json!("STOP"),
            "缺 Stop 事件时编码器要自己补 finishReason"
        );
    }

    #[test]
    fn stream_encoder_tolerates_missing_ceremony_events() {
        // 中转站直接发 delta，没有 Start / ContentStart —— 不能崩也不能丢内容。
        let frames = drive_encoder(&[
            StreamEvent::TextDelta {
                index: 0,
                text: "裸".into(),
            },
            StreamEvent::ToolCallArgsDelta {
                index: 1,
                fragment: r#"{"k":1}"#.into(),
            },
            StreamEvent::ContentStop { index: 1 },
        ]);
        assert_eq!(
            frames[0]["candidates"][0]["content"]["parts"][0]["text"],
            json!("裸")
        );
        // 缺 ToolCallStart 也要把攒到的 args 发出去。
        let call = &frames[1]["candidates"][0]["content"]["parts"][0]["functionCall"];
        assert_eq!(call["args"], json!({ "k": 1 }));
    }

    #[test]
    fn empty_content_message_is_dropped_not_emitted_empty() {
        // 空 parts 的消息不该产出 `{"role":"user","parts":[]}` —— Gemini 会拒。
        let ir = UnifiedRequest::new(
            "m",
            vec![
                Message::new(Role::User, vec![]),
                Message::text(Role::User, "有内容"),
            ],
        );
        let out = encode_req(&ir);
        let contents = out["contents"].as_array().expect("contents 应是数组");
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0]["parts"][0]["text"], json!("有内容"));
    }

    #[test]
    fn unknown_part_kinds_become_opaque_and_round_trip() {
        // executableCode 之类我们不认识的 part 包成 Opaque：解析不失败，
        // gemini→gemini 直通时原样还原（多轮历史里这些块必须保留）。
        let raw_part = json!({ "executableCode": { "language": "PYTHON", "code": "print(1)" } });
        let ir = decode_req(json!({
            "contents": [{
                "role": "model",
                "parts": [raw_part.clone(), { "text": "结果" }]
            }]
        }));
        assert_eq!(
            ir.messages[0].content.len(),
            2,
            "未知 part 包成 Opaque 保留"
        );
        assert_eq!(
            ir.messages[0].content[0],
            ContentPart::Opaque {
                protocol: "gemini".into(),
                value: raw_part.clone(),
            }
        );
        assert_eq!(ir.messages[0].content[1], ContentPart::text("结果"));

        // 直通还原：编码回 Gemini 时 Opaque 原样回写。
        let out = encode_req(&ir);
        assert_eq!(out["contents"][0]["parts"][0], raw_part);
        assert_eq!(out["contents"][0]["parts"][1]["text"], json!("结果"));
    }

    #[test]
    fn snake_case_field_names_are_accepted() {
        // REST API 同时接受 snake_case，中转站两种都会发。
        let ir = decode_req(json!({
            "contents": [{ "role": "user", "parts": [{ "text": "hi" }] }],
            "system_instruction": { "parts": [{ "text": "sys" }] },
            "generation_config": { "max_output_tokens": 512, "top_p": 0.5 }
        }));
        assert_eq!(ir.system_text(), "sys");
        assert_eq!(ir.max_output_tokens, Some(512));
        assert_eq!(ir.sampling.top_p, Some(0.5));
    }
}
