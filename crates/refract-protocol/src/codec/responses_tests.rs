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
