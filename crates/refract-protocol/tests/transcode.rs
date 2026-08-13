//! 跨协议转换的集成测试。
//!
//! 四个 codec 是各自独立实现的，它们只共享 IR 这一个契约。单元测试能证明
//! 「每个 codec 自己前后一致」，但证明不了**两个不同 codec 之间能对接** ——
//! 而后者才是这个网关的全部意义所在。
//!
//! 这里做的是 12 组（4 × 3）跨协议往返：用协议 A 的格式发请求，经 IR 转成
//! 协议 B 发给上游，再把 B 的响应转回 A。任何一个 codec 对 IR 的理解出现
//! 偏差，都会在这里暴露。

use refract_core::Protocol;
use refract_protocol::codec::CodecSet;
use serde_json::{Value, json};

/// 各协议的一个「典型请求」。字段选择覆盖了会掉信息的关键点：
/// system 提示、多轮对话、工具定义、采样参数。
fn sample_request(protocol: Protocol) -> Value {
    match protocol {
        Protocol::Chat => json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "system", "content": "You are terse."},
                {"role": "user", "content": "What is 2+2?"}
            ],
            "temperature": 0.3,
            "max_tokens": 100,
            "tools": [{
                "type": "function",
                "function": {
                    "name": "calc",
                    "description": "do math",
                    "parameters": {"type": "object", "properties": {"e": {"type": "string"}}}
                }
            }]
        }),
        Protocol::Responses => json!({
            "model": "gpt-5",
            "instructions": "You are terse.",
            "input": [
                {"role": "user", "content": [{"type": "input_text", "text": "What is 2+2?"}]}
            ],
            "temperature": 0.3,
            "max_output_tokens": 100,
            "tools": [{
                "type": "function",
                "name": "calc",
                "description": "do math",
                "parameters": {"type": "object", "properties": {"e": {"type": "string"}}}
            }]
        }),
        Protocol::Messages => json!({
            "model": "claude-sonnet-4-6",
            "system": "You are terse.",
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "What is 2+2?"}]}
            ],
            "temperature": 0.3,
            "max_tokens": 100,
            "tools": [{
                "name": "calc",
                "description": "do math",
                "input_schema": {"type": "object", "properties": {"e": {"type": "string"}}}
            }]
        }),
        Protocol::Gemini => json!({
            "model": "gemini-2.5-pro",
            "systemInstruction": {"parts": [{"text": "You are terse."}]},
            "contents": [
                {"role": "user", "parts": [{"text": "What is 2+2?"}]}
            ],
            "generationConfig": {"temperature": 0.3, "maxOutputTokens": 100},
            "tools": [{
                "functionDeclarations": [{
                    "name": "calc",
                    "description": "do math",
                    "parameters": {"type": "object", "properties": {"e": {"type": "string"}}}
                }]
            }]
        }),
    }
}

/// 各协议的一个「典型响应」，含文本 + 用量。
fn sample_response(protocol: Protocol) -> Value {
    match protocol {
        Protocol::Chat => json!({
            "id": "chatcmpl-1",
            "object": "chat.completion",
            "created": 1_700_000_000_i64,
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "4"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 12, "completion_tokens": 1, "total_tokens": 13}
        }),
        Protocol::Responses => json!({
            "id": "resp_1",
            "object": "response",
            "created_at": 1_700_000_000_i64,
            "model": "gpt-5",
            "status": "completed",
            "output": [{
                "type": "message",
                "id": "msg_1",
                "role": "assistant",
                "status": "completed",
                "content": [{"type": "output_text", "text": "4", "annotations": []}]
            }],
            "usage": {"input_tokens": 12, "output_tokens": 1, "total_tokens": 13}
        }),
        Protocol::Messages => json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-4-6",
            "content": [{"type": "text", "text": "4"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 12, "output_tokens": 1}
        }),
        Protocol::Gemini => json!({
            "candidates": [{
                "content": {"role": "model", "parts": [{"text": "4"}]},
                "finishReason": "STOP",
                "index": 0
            }],
            "usageMetadata": {
                "promptTokenCount": 12,
                "candidatesTokenCount": 1,
                "totalTokenCount": 13
            },
            "modelVersion": "gemini-2.5-pro"
        }),
    }
}

/// 从任意协议的响应里挖出助手回复的纯文本。
fn extract_text(protocol: Protocol, body: &Value) -> String {
    match protocol {
        Protocol::Chat => body["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        Protocol::Responses => body["output"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter(|i| i["type"] == "message")
                    .flat_map(|i| i["content"].as_array().cloned().unwrap_or_default())
                    .filter_map(|c| c["text"].as_str().map(str::to_owned))
                    .collect::<Vec<_>>()
                    .join("")
            })
            .unwrap_or_default(),
        Protocol::Messages => body["content"]
            .as_array()
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(|p| p["text"].as_str().map(str::to_owned))
                    .collect::<Vec<_>>()
                    .join("")
            })
            .unwrap_or_default(),
        Protocol::Gemini => body["candidates"][0]["content"]["parts"]
            .as_array()
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(|p| p["text"].as_str().map(str::to_owned))
                    .collect::<Vec<_>>()
                    .join("")
            })
            .unwrap_or_default(),
    }
}

/// 取用量。四家的字段名各不相同，归一后必须一致。
fn extract_usage(protocol: Protocol, body: &Value) -> (u64, u64) {
    let g = |v: &Value| v.as_u64().unwrap_or_default();
    match protocol {
        Protocol::Chat => (
            g(&body["usage"]["prompt_tokens"]),
            g(&body["usage"]["completion_tokens"]),
        ),
        Protocol::Responses => (
            g(&body["usage"]["input_tokens"]),
            g(&body["usage"]["output_tokens"]),
        ),
        Protocol::Messages => (
            g(&body["usage"]["input_tokens"]),
            g(&body["usage"]["output_tokens"]),
        ),
        Protocol::Gemini => (
            g(&body["usageMetadata"]["promptTokenCount"]),
            g(&body["usageMetadata"]["candidatesTokenCount"]),
        ),
    }
}

#[test]
fn every_protocol_pair_survives_a_request_round_trip() {
    let codecs = CodecSet::builtin();

    for inbound in Protocol::ALL {
        let raw = sample_request(inbound);
        let ir = codecs
            .for_protocol(inbound)
            .decode_request(&raw)
            .unwrap_or_else(|e| panic!("{inbound:?} failed to decode its own request: {e}"));

        // IR 层的不变量：不管从哪个协议进来，这些语义都必须存在。
        assert_eq!(ir.messages.len(), 1, "{inbound:?}: user turn lost");
        assert!(!ir.system.is_empty(), "{inbound:?}: system prompt lost");
        assert_eq!(ir.tools.len(), 1, "{inbound:?}: tool definition lost");
        assert_eq!(ir.tools[0].name, "calc", "{inbound:?}: tool name lost");
        assert_eq!(
            ir.sampling.temperature,
            Some(0.3),
            "{inbound:?}: temperature lost"
        );
        assert_eq!(
            ir.max_output_tokens,
            Some(100),
            "{inbound:?}: max_tokens lost"
        );

        for upstream in Protocol::ALL {
            if upstream == inbound {
                continue;
            }
            let encoded = codecs
                .for_protocol(upstream)
                .encode_request(&ir)
                .unwrap_or_else(|e| panic!("{inbound:?} -> {upstream:?} encode failed: {e}"));

            // 编码出的请求必须能被目标协议自己解回来 —— 这是「上游能看懂」
            // 的最强本地代理判据。
            let reparsed = codecs
                .for_protocol(upstream)
                .decode_request(&encoded)
                .unwrap_or_else(|e| {
                    panic!(
                        "{inbound:?} -> {upstream:?} produced unparseable output: {e}\n{encoded:#}"
                    )
                });

            assert_eq!(
                reparsed.messages.len(),
                ir.messages.len(),
                "{inbound:?} -> {upstream:?}: message count changed"
            );
            assert!(
                !reparsed.system.is_empty(),
                "{inbound:?} -> {upstream:?}: system prompt dropped"
            );
            assert_eq!(
                reparsed.tools.len(),
                1,
                "{inbound:?} -> {upstream:?}: tools dropped"
            );
            assert_eq!(
                reparsed.sampling.temperature, ir.sampling.temperature,
                "{inbound:?} -> {upstream:?}: temperature drifted"
            );
            assert_eq!(
                reparsed.max_output_tokens, ir.max_output_tokens,
                "{inbound:?} -> {upstream:?}: max_tokens drifted"
            );
        }
    }
}

#[test]
fn every_protocol_pair_survives_a_response_round_trip() {
    let codecs = CodecSet::builtin();

    for upstream in Protocol::ALL {
        let raw = sample_response(upstream);
        let ir = codecs
            .for_protocol(upstream)
            .decode_response(&raw)
            .unwrap_or_else(|e| panic!("{upstream:?} failed to decode its own response: {e}"));

        assert_eq!(ir.usage.input_tokens, 12, "{upstream:?}: input usage lost");
        assert_eq!(ir.usage.output_tokens, 1, "{upstream:?}: output usage lost");

        for inbound in Protocol::ALL {
            if inbound == upstream {
                continue;
            }
            let encoded = codecs
                .for_protocol(inbound)
                .encode_response(&ir)
                .unwrap_or_else(|e| panic!("{upstream:?} -> {inbound:?} encode failed: {e}"));

            assert_eq!(
                extract_text(inbound, &encoded),
                "4",
                "{upstream:?} -> {inbound:?}: assistant text lost\n{encoded:#}"
            );
            assert_eq!(
                extract_usage(inbound, &encoded),
                (12, 1),
                "{upstream:?} -> {inbound:?}: usage lost\n{encoded:#}"
            );

            // 客户端会拿这个响应再解析一次，必须是目标协议的合法形状。
            codecs
                .for_protocol(inbound)
                .decode_response(&encoded)
                .unwrap_or_else(|e| {
                    panic!("{upstream:?} -> {inbound:?} produced unparseable output: {e}")
                });
        }
    }
}

#[test]
fn tool_calls_survive_every_pair() {
    let codecs = CodecSet::builtin();

    // 用 Messages 的工具调用响应作为源：Anthropic 的工具入参是结构化对象，
    // 转到 Chat（入参是 JSON 字符串）时最容易出错。
    let raw = json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "claude-sonnet-4-6",
        "content": [
            {"type": "text", "text": "Let me calculate."},
            {
                "type": "tool_use",
                "id": "toolu_1",
                "name": "calc",
                "input": {"e": "2+2"}
            }
        ],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 10, "output_tokens": 5}
    });

    let ir = codecs
        .for_protocol(Protocol::Messages)
        .decode_response(&raw)
        .unwrap();

    for inbound in Protocol::ALL {
        let encoded = codecs
            .for_protocol(inbound)
            .encode_response(&ir)
            .unwrap_or_else(|e| panic!("encode to {inbound:?} failed: {e}"));

        let back = codecs
            .for_protocol(inbound)
            .decode_response(&encoded)
            .unwrap_or_else(|e| panic!("{inbound:?} could not reparse its own output: {e}"));

        let tool_calls: Vec<_> = back
            .content
            .iter()
            .filter_map(|p| match p {
                refract_protocol::ir::ContentPart::ToolUse { name, input, .. } => {
                    Some((name.clone(), input.clone()))
                }
                _ => None,
            })
            .collect();

        assert_eq!(
            tool_calls.len(),
            1,
            "{inbound:?}: tool call lost in round trip\n{encoded:#}"
        );
        assert_eq!(tool_calls[0].0, "calc", "{inbound:?}: tool name changed");
        assert_eq!(
            tool_calls[0].1,
            json!({"e": "2+2"}),
            "{inbound:?}: tool arguments corrupted"
        );
    }
}

#[test]
fn multi_turn_conversation_with_tool_results_survives_every_pair() {
    let codecs = CodecSet::builtin();

    // 带工具结果的多轮对话是最难的场景：工具调用与其结果必须配对，
    // 顺序不能乱，id 不能丢 —— 丢了上游会整个拒绝请求。
    let raw = json!({
        "model": "claude-sonnet-4-6",
        "max_tokens": 200,
        "messages": [
            {"role": "user", "content": [{"type": "text", "text": "What is 2+2?"}]},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_1", "name": "calc", "input": {"e": "2+2"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": "4"}
            ]},
            {"role": "assistant", "content": [{"type": "text", "text": "It is 4."}]}
        ],
        "tools": [{
            "name": "calc",
            "description": "do math",
            "input_schema": {"type": "object", "properties": {"e": {"type": "string"}}}
        }]
    });

    let ir = codecs
        .for_protocol(Protocol::Messages)
        .decode_request(&raw)
        .unwrap();
    assert_eq!(ir.messages.len(), 4);

    for upstream in Protocol::ALL {
        let encoded = codecs
            .for_protocol(upstream)
            .encode_request(&ir)
            .unwrap_or_else(|e| panic!("encode to {upstream:?} failed: {e}"));
        let back = codecs
            .for_protocol(upstream)
            .decode_request(&encoded)
            .unwrap_or_else(|e| {
                panic!("{upstream:?} could not reparse its own output: {e}\n{encoded:#}")
            });

        // Chat 协议把工具结果表达为独立的 `role: "tool"` 消息，条数会变；
        // 但工具调用与结果的**配对关系**必须守恒。
        let call_ids = collect_tool_use_ids(&back);
        let result_ids = collect_tool_result_ids(&back);
        assert_eq!(
            call_ids,
            vec!["toolu_1"],
            "{upstream:?}: tool_use id lost\n{encoded:#}"
        );
        assert_eq!(
            result_ids,
            vec!["toolu_1"],
            "{upstream:?}: tool_result linkage lost\n{encoded:#}"
        );
    }
}

fn collect_tool_use_ids(req: &refract_protocol::ir::UnifiedRequest) -> Vec<String> {
    use refract_protocol::ir::ContentPart;
    req.messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter_map(|p| match p {
            ContentPart::ToolUse { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect()
}

fn collect_tool_result_ids(req: &refract_protocol::ir::UnifiedRequest) -> Vec<String> {
    use refract_protocol::ir::ContentPart;
    req.messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter_map(|p| match p {
            ContentPart::ToolResult { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn thinking_blocks_survive_the_anthropic_round_trip() {
    let codecs = CodecSet::builtin();

    // signature 是 Anthropic 用来校验推理块未被篡改的凭证。转出去再转回来
    // 若丢了它，带推理的多轮对话会被上游整个拒绝 —— 这是最隐蔽的丢信息。
    let raw = json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "claude-sonnet-4-6",
        "content": [
            {"type": "thinking", "thinking": "2+2 is 4", "signature": "sig-abc123"},
            {"type": "text", "text": "4"}
        ],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 10, "output_tokens": 5}
    });

    let ir = codecs
        .for_protocol(Protocol::Messages)
        .decode_response(&raw)
        .unwrap();

    let encoded = codecs
        .for_protocol(Protocol::Messages)
        .encode_response(&ir)
        .unwrap();
    let back = codecs
        .for_protocol(Protocol::Messages)
        .decode_response(&encoded)
        .unwrap();

    let signatures: Vec<_> = back
        .content
        .iter()
        .filter_map(|p| match p {
            refract_protocol::ir::ContentPart::Thinking { signature, .. } => signature.clone(),
            _ => None,
        })
        .collect();
    assert_eq!(
        signatures,
        vec!["sig-abc123"],
        "dropping the thinking signature breaks multi-turn reasoning"
    );
}

#[test]
fn unknown_fields_are_preserved_through_same_protocol_round_trip() {
    let codecs = CodecSet::builtin();

    // 上游随时可能加新参数。同协议往返丢字段 = 用户没法用上游的新功能。
    let raw = json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "hi"}],
        "some_brand_new_knob": {"nested": true}
    });

    let ir = codecs
        .for_protocol(Protocol::Chat)
        .decode_request(&raw)
        .unwrap();
    let back = codecs
        .for_protocol(Protocol::Chat)
        .encode_request(&ir)
        .unwrap();

    assert_eq!(
        back["some_brand_new_knob"],
        json!({"nested": true}),
        "unknown fields must survive a same-protocol round trip"
    );
}

// ---------------------------------------------------------------------------
// wire schema 白名单：跨协议编码不得携带目标协议不认识的顶层字段
// ---------------------------------------------------------------------------

/// 各协议请求体的合法顶层字段。
///
/// 同协议直通有意保留未知字段（上游新参数），不适用白名单；但**跨协议**
/// 转码的输出里出现白名单之外的字段，只可能是源协议字段或自造搬运键
/// 泄漏 —— 严格的上游会直接 400。
fn allowed_request_fields(protocol: Protocol) -> &'static [&'static str] {
    match protocol {
        Protocol::Chat => &[
            "model",
            "messages",
            "temperature",
            "top_p",
            "max_tokens",
            "max_completion_tokens",
            "stop",
            "stream",
            "stream_options",
            "tools",
            "tool_choice",
            "parallel_tool_calls",
            "user",
            "n",
            "frequency_penalty",
            "presence_penalty",
            "logit_bias",
            "logprobs",
            "top_logprobs",
            "response_format",
            "seed",
            "service_tier",
            "store",
            "metadata",
            "reasoning_effort",
            "modalities",
            "audio",
            "prediction",
        ],
        Protocol::Responses => &[
            "model",
            "input",
            "instructions",
            "temperature",
            "top_p",
            "max_output_tokens",
            "stream",
            "tools",
            "tool_choice",
            "parallel_tool_calls",
            "reasoning",
            "text",
            "truncation",
            "metadata",
            "store",
            "previous_response_id",
            "include",
            "background",
            "max_tool_calls",
            "prompt_cache_key",
            "safety_identifier",
            "service_tier",
            "top_logprobs",
            "user",
        ],
        Protocol::Messages => &[
            "model",
            "messages",
            "system",
            "max_tokens",
            "temperature",
            "top_p",
            "top_k",
            "stop_sequences",
            "stream",
            "tools",
            "tool_choice",
            "metadata",
            "thinking",
            "service_tier",
        ],
        Protocol::Gemini => &[
            "contents",
            "systemInstruction",
            "generationConfig",
            "tools",
            "toolConfig",
            "safetySettings",
            "cachedContent",
            "labels",
        ],
    }
}

/// 各协议一个「满配请求」：推理、工具、采样、停止序列全带上，
/// 最大化字段泄漏的机会。
fn rich_request(protocol: Protocol) -> Value {
    match protocol {
        Protocol::Chat => json!({
            "model": "o3",
            "messages": [
                {"role": "system", "content": "Be terse."},
                {"role": "user", "content": "hi"},
                {"role": "assistant", "tool_calls": [{"id": "c1", "type": "function",
                    "function": {"name": "f", "arguments": "{\"x\":1}"}}]},
                {"role": "tool", "tool_call_id": "c1", "content": "ok"}
            ],
            "temperature": 0.5, "top_p": 0.9, "max_tokens": 800, "stop": ["END"],
            "reasoning_effort": "high",
            "tools": [{"type": "function", "function": {"name": "f", "parameters": {"type": "object"}}}],
            "tool_choice": "auto",
            "custom_chat_only_knob": {"a": 1}
        }),
        Protocol::Responses => json!({
            "model": "gpt-5",
            "instructions": "Be terse.",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]},
                {"type": "function_call", "call_id": "c1", "name": "f", "arguments": "{\"x\":1}"},
                {"type": "function_call_output", "call_id": "c1", "output": "ok"},
                {"type": "web_search_call", "id": "ws_1", "status": "completed"}
            ],
            "temperature": 0.5, "top_p": 0.9, "max_output_tokens": 800,
            "reasoning": {"effort": "high", "summary": "auto"},
            "tools": [
                {"type": "function", "name": "f", "parameters": {"type": "object"}},
                {"type": "web_search"}
            ],
            "tool_choice": "auto",
            "custom_responses_only_knob": true
        }),
        Protocol::Messages => json!({
            "model": "claude-sonnet-4-6",
            "system": "Be terse.",
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "hi"}]},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "hmm", "signature": "SIG"},
                    {"type": "tool_use", "id": "c1", "name": "f", "input": {"x": 1}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "c1", "content": "ok"}
                ]}
            ],
            "max_tokens": 800, "temperature": 0.5, "top_k": 40,
            "stop_sequences": ["END"],
            "thinking": {"type": "enabled", "budget_tokens": 2048},
            "tools": [{"name": "f", "input_schema": {"type": "object"}}],
            "custom_messages_only_knob": "x"
        }),
        Protocol::Gemini => json!({
            "contents": [
                {"role": "user", "parts": [{"text": "hi"}]},
                {"role": "model", "parts": [
                    {"functionCall": {"name": "f", "args": {"x": 1}}, "thoughtSignature": "TS"}
                ]},
                {"role": "user", "parts": [
                    {"functionResponse": {"name": "f", "response": {"out": "ok"}}}
                ]}
            ],
            "systemInstruction": {"parts": [{"text": "Be terse."}]},
            "generationConfig": {
                "temperature": 0.5, "topK": 40, "maxOutputTokens": 800,
                "stopSequences": ["END"],
                "thinkingConfig": {"thinkingBudget": 2048, "includeThoughts": true}
            },
            "safetySettings": [{"category": "HARM_CATEGORY_HATE_SPEECH", "threshold": "BLOCK_NONE"}],
            "tools": [{"functionDeclarations": [{"name": "f", "parameters": {"type": "object"}}]}]
        }),
    }
}

#[test]
fn cross_protocol_encoding_stays_within_the_target_schema() {
    let codecs = CodecSet::builtin();

    for inbound in Protocol::ALL {
        let ir = codecs
            .for_protocol(inbound)
            .decode_request(&rich_request(inbound))
            .unwrap_or_else(|e| panic!("{inbound:?} failed to decode rich request: {e}"));

        for upstream in Protocol::ALL {
            if upstream == inbound {
                continue;
            }
            let encoded = codecs
                .for_protocol(upstream)
                .encode_request(&ir)
                .unwrap_or_else(|e| panic!("{inbound:?} -> {upstream:?} encode failed: {e}"));

            let allowed = allowed_request_fields(upstream);
            let obj = encoded.as_object().expect("request body must be an object");
            let leaked: Vec<&String> = obj
                .keys()
                .filter(|k| !allowed.contains(&k.as_str()))
                .collect();
            assert!(
                leaked.is_empty(),
                "{inbound:?} -> {upstream:?} leaked foreign fields {leaked:?}\n{encoded:#}"
            );
        }
    }
}

#[test]
fn same_protocol_round_trip_still_keeps_private_knobs() {
    // 白名单只约束跨协议；直通必须保留自家未知字段，两者不能互相牺牲。
    let codecs = CodecSet::builtin();
    let back = codecs
        .for_protocol(Protocol::Chat)
        .encode_request(
            &codecs
                .for_protocol(Protocol::Chat)
                .decode_request(&rich_request(Protocol::Chat))
                .unwrap(),
        )
        .unwrap();
    assert_eq!(back["custom_chat_only_knob"], json!({"a": 1}));
}

// ---------------------------------------------------------------------------
// usage 数值口径：output 统一含 reasoning，各协议换算不能失真
// ---------------------------------------------------------------------------

/// 带推理消耗的响应样本：input=20，可见输出=30，推理=10。
/// IR 口径（对齐 OpenAI/Anthropic）：output_tokens = 40（含推理）。
fn reasoning_response(protocol: Protocol) -> Value {
    match protocol {
        Protocol::Chat => json!({
            "id": "chatcmpl-1", "object": "chat.completion", "model": "o3",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "4"},
                         "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 20, "completion_tokens": 40, "total_tokens": 60,
                      "completion_tokens_details": {"reasoning_tokens": 10}}
        }),
        Protocol::Responses => json!({
            "id": "resp_1", "object": "response", "model": "gpt-5", "status": "completed",
            "output": [{"type": "message", "id": "m1", "role": "assistant", "status": "completed",
                        "content": [{"type": "output_text", "text": "4", "annotations": []}]}],
            "usage": {"input_tokens": 20, "output_tokens": 40, "total_tokens": 60,
                      "output_tokens_details": {"reasoning_tokens": 10}}
        }),
        // Anthropic 不单独报 reasoning tokens：output_tokens 已含推理。
        Protocol::Messages => json!({
            "id": "msg_1", "type": "message", "role": "assistant", "model": "claude-sonnet-4-6",
            "content": [{"type": "text", "text": "4"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 20, "output_tokens": 40}
        }),
        // Gemini 的 candidatesTokenCount **不含**思考，thoughts 单列。
        Protocol::Gemini => json!({
            "candidates": [{"content": {"role": "model", "parts": [{"text": "4"}]},
                            "finishReason": "STOP", "index": 0}],
            "usageMetadata": {"promptTokenCount": 20, "candidatesTokenCount": 30,
                              "thoughtsTokenCount": 10, "totalTokenCount": 60},
            "modelVersion": "gemini-2.5-pro"
        }),
    }
}

#[test]
fn reasoning_usage_numbers_are_consistent_across_every_pair() {
    let codecs = CodecSet::builtin();

    for upstream in Protocol::ALL {
        let ir = codecs
            .for_protocol(upstream)
            .decode_response(&reasoning_response(upstream))
            .unwrap_or_else(|e| panic!("{upstream:?} decode failed: {e}"));

        // IR 层不变量：output 统一含推理。
        assert_eq!(ir.usage.input_tokens, 20, "{upstream:?}: input drifted");
        assert_eq!(
            ir.usage.output_tokens, 40,
            "{upstream:?}: output must include reasoning tokens"
        );
        // Messages 报不出 reasoning 明细，其余协议必须保住 10。
        if upstream != Protocol::Messages {
            assert_eq!(
                ir.usage.reasoning_tokens, 10,
                "{upstream:?}: reasoning detail lost"
            );
        }

        for inbound in Protocol::ALL {
            let encoded = codecs
                .for_protocol(inbound)
                .encode_response(&ir)
                .unwrap_or_else(|e| panic!("{upstream:?} -> {inbound:?} encode failed: {e}"));

            let g = |v: &Value| v.as_u64().unwrap_or_default();
            match inbound {
                Protocol::Chat => {
                    assert_eq!(g(&encoded["usage"]["prompt_tokens"]), 20);
                    assert_eq!(
                        g(&encoded["usage"]["completion_tokens"]),
                        40,
                        "{upstream:?} -> chat: completion_tokens 口径应含推理"
                    );
                    assert_eq!(g(&encoded["usage"]["total_tokens"]), 60);
                }
                Protocol::Responses => {
                    assert_eq!(g(&encoded["usage"]["input_tokens"]), 20);
                    assert_eq!(g(&encoded["usage"]["output_tokens"]), 40);
                    assert_eq!(g(&encoded["usage"]["total_tokens"]), 60);
                }
                Protocol::Messages => {
                    assert_eq!(g(&encoded["usage"]["input_tokens"]), 20);
                    assert_eq!(g(&encoded["usage"]["output_tokens"]), 40);
                }
                Protocol::Gemini => {
                    assert_eq!(g(&encoded["usageMetadata"]["promptTokenCount"]), 20);
                    assert_eq!(
                        g(&encoded["usageMetadata"]["candidatesTokenCount"])
                            + g(&encoded["usageMetadata"]["thoughtsTokenCount"]),
                        40,
                        "{upstream:?} -> gemini: candidates+thoughts 必须等于全口径 output"
                    );
                    assert_eq!(g(&encoded["usageMetadata"]["totalTokenCount"]), 60);
                }
            }
        }
    }
}

#[test]
fn empty_and_minimal_requests_do_not_panic() {
    let codecs = CodecSet::builtin();

    // 最小合法请求。缺字段应当报错而不是 panic —— 网关不能被畸形请求打挂。
    let minimal = [
        (Protocol::Chat, json!({"model": "m", "messages": []})),
        (Protocol::Responses, json!({"model": "m", "input": []})),
        (
            Protocol::Messages,
            json!({"model": "m", "max_tokens": 1, "messages": []}),
        ),
        (Protocol::Gemini, json!({"contents": []})),
    ];

    for (protocol, raw) in minimal {
        let result = codecs.for_protocol(protocol).decode_request(&raw);
        // 空消息列表可接受或拒绝，但都不能 panic。
        if let Ok(ir) = result {
            for target in Protocol::ALL {
                let _ = codecs.for_protocol(target).encode_request(&ir);
            }
        }
    }

    // 完全垃圾的输入。
    for protocol in Protocol::ALL {
        assert!(
            codecs
                .for_protocol(protocol)
                .decode_request(&json!("not an object"))
                .is_err(),
            "{protocol:?} accepted a non-object request"
        );
    }
}
