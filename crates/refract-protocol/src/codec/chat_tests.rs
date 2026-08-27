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
fn stream_emits_single_start_when_name_split_across_chunks() {
    // 回归：name 被中转站拆到后续帧时，必须累积拼回后只补发一条 Start，
    // 不能每帧都补 —— 重复 Start 会让下游编码器开两个块。
    let events = decode_stream(&[
        r#"{"id":"c","model":"m","choices":[{"index":0,"delta":{"tool_calls":[
            {"index":0,"id":"call_x","type":"function",
             "function":{"name":"get_","arguments":""}}]}}]}"#,
        r#"{"id":"c","model":"m","choices":[{"index":0,"delta":{"tool_calls":[
            {"index":0,"function":{"name":"weather","arguments":"{\"ci"}}]}}]}"#,
        r#"{"id":"c","model":"m","choices":[{"index":0,"delta":{"tool_calls":[
            {"index":0,"function":{"arguments":"ty\":1}"}}]}}]}"#,
        r#"{"id":"c","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        "[DONE]",
    ]);

    // 只允许一条 Start，且 name 是拼接后的完整名。
    let starts: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, StreamEvent::ToolCallStart { .. }))
        .collect();
    assert_eq!(starts.len(), 1, "每个工具只应产出一条 Start：{events:?}");
    assert_eq!(
        events,
        vec![
            StreamEvent::Start {
                id: "c".into(),
                model: "m".into(),
                usage: None
            },
            StreamEvent::ToolCallStart {
                signature: None,
                index: 1,
                id: "call_x".into(),
                name: "get_weather".into(),
            },
            StreamEvent::ToolCallArgsDelta {
                index: 1,
                fragment: "{\"ci".into()
            },
            StreamEvent::ToolCallArgsDelta {
                index: 1,
                fragment: "ty\":1}".into()
            },
            StreamEvent::Stop {
                reason: StopReason::ToolUse,
                stop_sequence: None
            },
            StreamEvent::Done,
        ]
    );

    // name 完全迟到（首帧只有 id + arguments）时也一样：参数先暂存，
    // 名字到达后连同 Start 一起补发，全程只有一条 Start。
    let late = decode_stream(&[
        r#"{"id":"c","model":"m","choices":[{"index":0,"delta":{"tool_calls":[
            {"index":0,"id":"call_y","type":"function",
             "function":{"arguments":"{"}}]}}]}"#,
        r#"{"id":"c","model":"m","choices":[{"index":0,"delta":{"tool_calls":[
            {"index":0,"function":{"name":"fn","arguments":"}"}}]}}]}"#,
        r#"{"id":"c","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        "[DONE]",
    ]);
    assert_eq!(
        late,
        vec![
            StreamEvent::Start {
                id: "c".into(),
                model: "m".into(),
                usage: None
            },
            StreamEvent::ToolCallStart {
                signature: None,
                index: 1,
                id: "call_y".into(),
                name: "fn".into(),
            },
            // 先到的 "{" 随声明补发，顺序不乱。
            StreamEvent::ToolCallArgsDelta {
                index: 1,
                fragment: "{".into()
            },
            StreamEvent::ToolCallArgsDelta {
                index: 1,
                fragment: "}".into()
            },
            StreamEvent::Stop {
                reason: StopReason::ToolUse,
                stop_sequence: None
            },
            StreamEvent::Done,
        ]
    );

    // 无参调用（arguments 始终为空）：declare 永远等不到触发，
    // 必须由流终结前的 flush 补发 Start，否则下游看不到这条调用。
    let no_args = decode_stream(&[
        r#"{"id":"c","model":"m","choices":[{"index":0,"delta":{"tool_calls":[
            {"index":0,"id":"call_z","type":"function",
             "function":{"name":"ping","arguments":""}}]}}]}"#,
        r#"{"id":"c","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        "[DONE]",
    ]);
    assert!(
        no_args.contains(
            &(StreamEvent::ToolCallStart {
                signature: None,
                index: 1,
                id: "call_z".into(),
                name: "ping".into(),
            })
        ),
        "无参工具也要声明：{no_args:?}"
    );
    // flush 必须先于 Stop，否则下游编码器来不及把块合进响应。
    let start_pos = no_args
        .iter()
        .position(|e| matches!(e, StreamEvent::ToolCallStart { .. }))
        .unwrap();
    let stop_pos = no_args
        .iter()
        .position(|e| matches!(e, StreamEvent::Stop { .. }))
        .unwrap();
    assert!(start_pos < stop_pos, "Start 必须先于 Stop：{no_args:?}");
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
