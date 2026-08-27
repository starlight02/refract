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
fn text_source_document_stays_plain_text() {
    // 回归：source.type == "text" 的 document 必须映射到 MediaSource::Text，
    // 绝不能当 base64 —— 否则跨协议转码会把原文误当 base64 解码，
    // 回给 Anthropic 也要还原成 text 源。
    let raw = json!({
        "model": "m", "max_tokens": 8,
        "messages": [{
            "role": "user",
            "content": [{
                "type": "document",
                "source": { "type": "text", "media_type": "text/plain", "data": "hello world" },
                "title": "note.txt",
            }],
        }],
    });
    let ir = MESSAGES.decode_request(&raw).expect("decode");
    assert_eq!(
        ir.messages[0].content,
        vec![ContentPart::File {
            source: MediaSource::Text("hello world".into()),
            mime: Some("text/plain".into()),
            name: Some("note.txt".into()),
        }]
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
