//! 跨协议**流式**转换的集成测试。
//!
//! 非流式往返（见 `transcode.rs`）证明不了流式能用：流式路径是完全独立的
//! 状态机，且失败模式更隐蔽 —— 帧序错乱、块下标冲突、终止帧缺失，这些都
//! 只在流式下出现。真实用户 99% 的请求走流式，这一层不测等于没测。
//!
//! 测试策略：把上游协议的一串真实 SSE 帧喂给它的 decoder，得到统一事件，
//! 再用客户端协议的 encoder 编回去，最后**用客户端协议自己的 decoder 解析
//! 一遍**。能自洽解析 = 客户端 SDK 能看懂。

use refract_core::Protocol;
use refract_protocol::codec::CodecSet;
use refract_protocol::stream::{PartKind, SseFrame, StreamAggregator, StreamEvent};

/// 各协议一段典型的流式响应：文本分两块吐出 + 用量 + 终止。
fn sample_frames(protocol: Protocol) -> Vec<SseFrame> {
    match protocol {
        Protocol::Chat => vec![
            SseFrame::data(
                r#"{"id":"chatcmpl-1","object":"chat.completion.chunk","model":"gpt-4o","choices":[{"index":0,"delta":{"role":"assistant","content":"Hel"}}]}"#,
            ),
            SseFrame::data(
                r#"{"id":"chatcmpl-1","object":"chat.completion.chunk","model":"gpt-4o","choices":[{"index":0,"delta":{"content":"lo"}}]}"#,
            ),
            SseFrame::data(
                r#"{"id":"chatcmpl-1","object":"chat.completion.chunk","model":"gpt-4o","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}}"#,
            ),
            SseFrame::data("[DONE]"),
        ],
        Protocol::Responses => vec![
            SseFrame::named(
                "response.created",
                r#"{"type":"response.created","sequence_number":0,"response":{"id":"resp_1","object":"response","model":"gpt-5","status":"in_progress","output":[]}}"#,
            ),
            SseFrame::named(
                "response.output_item.added",
                r#"{"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"type":"message","id":"msg_1","role":"assistant","status":"in_progress","content":[]}}"#,
            ),
            SseFrame::named(
                "response.content_part.added",
                r#"{"type":"response.content_part.added","sequence_number":2,"item_id":"msg_1","output_index":0,"content_index":0,"part":{"type":"output_text","text":"","annotations":[]}}"#,
            ),
            SseFrame::named(
                "response.output_text.delta",
                r#"{"type":"response.output_text.delta","sequence_number":3,"item_id":"msg_1","output_index":0,"content_index":0,"delta":"Hel"}"#,
            ),
            SseFrame::named(
                "response.output_text.delta",
                r#"{"type":"response.output_text.delta","sequence_number":4,"item_id":"msg_1","output_index":0,"content_index":0,"delta":"lo"}"#,
            ),
            SseFrame::named(
                "response.completed",
                r#"{"type":"response.completed","sequence_number":5,"response":{"id":"resp_1","object":"response","model":"gpt-5","status":"completed","output":[],"usage":{"input_tokens":5,"output_tokens":2,"total_tokens":7}}}"#,
            ),
        ],
        Protocol::Messages => vec![
            SseFrame::named(
                "message_start",
                r#"{"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude-sonnet-4-6","content":[],"usage":{"input_tokens":5,"output_tokens":0}}}"#,
            ),
            SseFrame::named(
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            ),
            SseFrame::named(
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}}"#,
            ),
            SseFrame::named(
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo"}}"#,
            ),
            SseFrame::named(
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            SseFrame::named(
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}"#,
            ),
            SseFrame::named("message_stop", r#"{"type":"message_stop"}"#),
        ],
        Protocol::Gemini => vec![
            SseFrame::data(
                r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"Hel"}]},"index":0}],"modelVersion":"gemini-2.5-pro"}"#,
            ),
            SseFrame::data(
                r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"lo"}]},"index":0}],"modelVersion":"gemini-2.5-pro"}"#,
            ),
            SseFrame::data(
                r#"{"candidates":[{"content":{"role":"model","parts":[]},"finishReason":"STOP","index":0}],"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":2,"totalTokenCount":7},"modelVersion":"gemini-2.5-pro"}"#,
            ),
        ],
    }
}

/// 把一串帧喂给指定协议的解码器，收集出统一事件。
fn decode_all(codecs: &CodecSet, protocol: Protocol, frames: &[SseFrame]) -> Vec<StreamEvent> {
    let mut decoder = codecs.for_protocol(protocol).stream_decoder();
    let mut events = Vec::new();
    for frame in frames {
        events.extend(decoder.decode(frame).unwrap_or_else(|e| {
            panic!("{protocol:?} failed to decode its own frame: {e}\n{frame:?}")
        }));
    }
    events.extend(decoder.finish().unwrap());
    events
}

/// 把统一事件编码成目标协议的帧。
fn encode_all(codecs: &CodecSet, protocol: Protocol, events: &[StreamEvent]) -> Vec<SseFrame> {
    let mut encoder = codecs.for_protocol(protocol).stream_encoder();
    let mut frames = Vec::new();
    for event in events {
        frames.extend(
            encoder
                .encode(event)
                .unwrap_or_else(|e| panic!("{protocol:?} failed to encode {event:?}: {e}")),
        );
    }
    frames.extend(encoder.finish().unwrap());
    frames
}

/// 把事件流聚合成完整文本。
fn aggregate_text(events: &[StreamEvent]) -> String {
    let mut agg = StreamAggregator::new();
    for event in events {
        agg.absorb(event);
    }
    agg.into_content()
        .into_iter()
        .filter_map(|p| match p {
            refract_protocol::ir::ContentPart::Text { text } => Some(text),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

#[test]
fn every_protocol_decodes_its_own_stream_to_the_same_text() {
    let codecs = CodecSet::builtin();
    for protocol in Protocol::ALL {
        let events = decode_all(&codecs, protocol, &sample_frames(protocol));
        assert_eq!(
            aggregate_text(&events),
            "Hello",
            "{protocol:?}: text lost while decoding its own stream"
        );

        let usage = events.iter().find_map(|e| match e {
            StreamEvent::Usage(u) => Some(*u),
            StreamEvent::Start { usage, .. } => *usage,
            _ => None,
        });
        assert!(
            usage.is_some(),
            "{protocol:?}: no usage event emitted; billing would see zero tokens"
        );
    }
}

#[test]
fn every_stream_pair_round_trips_the_text() {
    let codecs = CodecSet::builtin();

    for upstream in Protocol::ALL {
        let events = decode_all(&codecs, upstream, &sample_frames(upstream));

        for inbound in Protocol::ALL {
            let frames = encode_all(&codecs, inbound, &events);
            assert!(
                !frames.is_empty(),
                "{upstream:?} -> {inbound:?}: encoder produced no frames"
            );

            // 关键判据：客户端协议自己的解码器必须能解析这些帧。
            let reparsed = decode_all(&codecs, inbound, &frames);
            assert_eq!(
                aggregate_text(&reparsed),
                "Hello",
                "{upstream:?} -> {inbound:?}: text corrupted in streaming round trip\n{frames:#?}"
            );
        }
    }
}

#[test]
fn every_stream_pair_terminates_the_stream() {
    let codecs = CodecSet::builtin();

    for upstream in Protocol::ALL {
        let events = decode_all(&codecs, upstream, &sample_frames(upstream));
        // 上游解码后必须给出终止信号，否则下游永远不知道流结束了。
        assert!(
            events.iter().any(|e| matches!(e, StreamEvent::Done)),
            "{upstream:?}: decoder never emitted Done"
        );

        for inbound in Protocol::ALL {
            let frames = encode_all(&codecs, inbound, &events);
            let terminated = match inbound {
                // OpenAI 系用 `data: [DONE]` 收尾。
                Protocol::Chat => frames.iter().any(|f| f.data.trim() == "[DONE]"),
                // Responses 用 response.completed / incomplete / failed 收尾。
                Protocol::Responses => frames.iter().any(|f| {
                    f.event.as_deref().is_some_and(|e| {
                        e.starts_with("response.completed")
                            || e.starts_with("response.incomplete")
                            || e.starts_with("response.failed")
                    })
                }),
                // Anthropic 用 message_stop 收尾。
                Protocol::Messages => frames
                    .iter()
                    .any(|f| f.event.as_deref() == Some("message_stop")),
                // Gemini 没有终止帧，靠 finishReason 与连接关闭。
                Protocol::Gemini => frames.iter().any(|f| f.data.contains("finishReason")),
            };
            assert!(
                terminated,
                "{upstream:?} -> {inbound:?}: stream never terminated; clients would hang\n{frames:#?}"
            );
        }
    }
}

#[test]
fn tool_call_streams_survive_every_pair() {
    let codecs = CodecSet::builtin();

    // 工具调用的流式转换最容易坏：入参是分片到达的 JSON 字符串，
    // 任何一段丢失都会让上游收到语法错误的 JSON。
    let frames = vec![
        SseFrame::named(
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude-sonnet-4-6","content":[],"usage":{"input_tokens":5,"output_tokens":0}}}"#,
        ),
        SseFrame::named(
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"calc","input":{}}}"#,
        ),
        SseFrame::named(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"e\":"}}"#,
        ),
        SseFrame::named(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"\"2+2\"}"}}"#,
        ),
        SseFrame::named(
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ),
        SseFrame::named(
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":9}}"#,
        ),
        SseFrame::named("message_stop", r#"{"type":"message_stop"}"#),
    ];

    let events = decode_all(&codecs, Protocol::Messages, &frames);

    for inbound in Protocol::ALL {
        let encoded = encode_all(&codecs, inbound, &events);
        let reparsed = decode_all(&codecs, inbound, &encoded);

        let mut agg = StreamAggregator::new();
        for event in &reparsed {
            agg.absorb(event);
        }
        let tools: Vec<_> = agg
            .into_content()
            .into_iter()
            .filter_map(|p| match p {
                refract_protocol::ir::ContentPart::ToolUse { name, input, .. } => {
                    Some((name, input))
                }
                _ => None,
            })
            .collect();

        assert_eq!(
            tools.len(),
            1,
            "{inbound:?}: tool call lost in streaming round trip\n{encoded:#?}"
        );
        assert_eq!(tools[0].0, "calc", "{inbound:?}: tool name changed");
        assert_eq!(
            tools[0].1,
            serde_json::json!({"e": "2+2"}),
            "{inbound:?}: streamed tool arguments corrupted"
        );
    }
}

#[test]
fn thinking_streams_survive_the_pairs_that_support_them() {
    let codecs = CodecSet::builtin();

    let frames = vec![
        SseFrame::named(
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude-sonnet-4-6","content":[],"usage":{"input_tokens":5,"output_tokens":0}}}"#,
        ),
        SseFrame::named(
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
        ),
        SseFrame::named(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"2+2 is 4"}}"#,
        ),
        SseFrame::named(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig-abc"}}"#,
        ),
        SseFrame::named(
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ),
        SseFrame::named(
            "content_block_start",
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
        ),
        SseFrame::named(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Hello"}}"#,
        ),
        SseFrame::named(
            "content_block_stop",
            r#"{"type":"content_block_stop","index":1}"#,
        ),
        SseFrame::named(
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":8}}"#,
        ),
        SseFrame::named("message_stop", r#"{"type":"message_stop"}"#),
    ];

    let events = decode_all(&codecs, Protocol::Messages, &frames);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, StreamEvent::ThinkingDelta { .. })),
        "thinking delta lost during decode"
    );

    // 正文在所有协议下都不能丢，哪怕推理块被丢弃。
    for inbound in Protocol::ALL {
        let encoded = encode_all(&codecs, inbound, &events);
        let reparsed = decode_all(&codecs, inbound, &encoded);
        assert_eq!(
            aggregate_text(&reparsed),
            "Hello",
            "{inbound:?}: assistant text lost when a thinking block precedes it"
        );
    }

    // Anthropic 往返必须保住 signature。
    let encoded = encode_all(&codecs, Protocol::Messages, &events);
    let reparsed = decode_all(&codecs, Protocol::Messages, &encoded);
    let has_signature = reparsed.iter().any(
        |e| matches!(e, StreamEvent::ThinkingSignature { signature, .. } if signature == "sig-abc"),
    );
    assert!(
        has_signature,
        "thinking signature lost in Messages streaming round trip"
    );
}

#[test]
fn decoders_tolerate_junk_frames_without_failing_the_stream() {
    let codecs = CodecSet::builtin();

    // 中转站会插入心跳、空帧、非 JSON 垃圾。这些必须被忽略而不是终止流 ——
    // 一个心跳把整个回答干掉是不可接受的。
    for protocol in Protocol::ALL {
        let mut frames = vec![
            SseFrame::data(""),
            SseFrame::data("ping"),
            SseFrame::named("ping", "{}"),
        ];
        frames.extend(sample_frames(protocol));

        let mut decoder = codecs.for_protocol(protocol).stream_decoder();
        let mut events = Vec::new();
        for frame in &frames {
            match decoder.decode(frame) {
                Ok(e) => events.extend(e),
                Err(e) => panic!("{protocol:?}: junk frame killed the stream: {e}\n{frame:?}"),
            }
        }
        events.extend(decoder.finish().unwrap());

        assert_eq!(
            aggregate_text(&events),
            "Hello",
            "{protocol:?}: junk frames corrupted the payload"
        );
    }
}

#[test]
fn aggregator_reconstructs_a_full_response_from_any_stream() {
    let codecs = CodecSet::builtin();

    // 流式上游 + 非流式客户端的场景：必须能把流聚合成完整响应。
    for upstream in Protocol::ALL {
        let events = decode_all(&codecs, upstream, &sample_frames(upstream));
        let mut agg = StreamAggregator::new();
        for event in &events {
            agg.absorb(event);
        }

        assert_eq!(agg.usage.input_tokens, 5, "{upstream:?}: input usage lost");
        assert_eq!(
            agg.usage.output_tokens, 2,
            "{upstream:?}: output usage lost"
        );
        assert!(
            agg.stop_reason.is_some(),
            "{upstream:?}: stop reason lost; clients cannot tell why generation ended"
        );

        let content = agg.into_content();
        assert_eq!(content.len(), 1, "{upstream:?}: unexpected content blocks");
    }
}

#[test]
fn same_index_different_kinds_do_not_overwrite_each_other() {
    // DeepSeek 系中转站把 reasoning_content 与 content 都放在 choices[0]，
    // 两者共享下标 0。按下标归档会让后到的那一类静默消失。
    let mut agg = StreamAggregator::new();
    agg.absorb(&StreamEvent::ThinkingDelta {
        index: 0,
        text: "thinking".into(),
    });
    agg.absorb(&StreamEvent::TextDelta {
        index: 0,
        text: "answer".into(),
    });

    let content = agg.into_content();
    assert_eq!(
        content.len(),
        2,
        "same-index blocks of different kinds must coexist, got {content:#?}"
    );
}

#[test]
fn content_start_does_not_reset_already_received_text() {
    // 重复的 ContentStart（中转站重发仪式帧）不该清空已收到的增量。
    let mut agg = StreamAggregator::new();
    agg.absorb(&StreamEvent::ContentStart {
        index: 0,
        kind: PartKind::Text,
    });
    agg.absorb(&StreamEvent::TextDelta {
        index: 0,
        text: "Hello".into(),
    });
    agg.absorb(&StreamEvent::ContentStart {
        index: 0,
        kind: PartKind::Text,
    });
    agg.absorb(&StreamEvent::TextDelta {
        index: 0,
        text: " world".into(),
    });

    let content = agg.into_content();
    assert_eq!(content.len(), 1);
    match &content[0] {
        refract_protocol::ir::ContentPart::Text { text } => assert_eq!(text, "Hello world"),
        other => panic!("unexpected part: {other:?}"),
    }
}
