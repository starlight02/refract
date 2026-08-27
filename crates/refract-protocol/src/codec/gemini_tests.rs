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
            id: "get_weather#0".into(),
            name: "get_weather".into(),
            input: json!({ "city": "上海" }),
        },
        "缺 id 时应回退用 name#index"
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
fn parallel_same_name_calls_get_distinct_fallback_ids() {
    let ir = decode_req(json!({
        "contents": [{
            "role": "model",
            "parts": [
                { "functionCall": { "name": "search", "args": { "q": "a" } } },
                { "functionCall": { "name": "search", "args": { "q": "b" } } }
            ]
        }]
    }));
    let ids: Vec<_> = ir.messages[0]
        .content
        .iter()
        .filter_map(|part| match part {
            ContentPart::ToolUse { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(ids, ["search#0", "search#1"]);
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
            id: "get_weather#0".into(),
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
        id: "f#0".into(),
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
