//! Cross-protocol conversion tests driven by the fixtures under
//! `tests/protocol-compat/`.

use octra_llm_core::{
    AssistantContent, ChatRequest, ChatResponse, ContentPart, FinishReason, Message, Role,
    StreamEvent, SystemPrompt, Tool, ToolCallKind, Usage, Value,
};
use octra_llm_protocol::{
    anthropic, decode_request, decode_response, encode_request, encode_response, openai,
    parse_sse_json, stream_decoder, stream_encoder,
};

/// Minimal mirror of `octra_data_plane::sse::SseDecoder` so this crate keeps
/// no dependency on the transport crate.
#[derive(Default)]
struct SseDecoder {
    buffer: Vec<u8>,
}

struct DecodedEvent {
    event: Option<String>,
    data: String,
}

impl SseDecoder {
    fn new() -> Self {
        Self::default()
    }

    fn push(&mut self, chunk: &[u8]) -> Vec<DecodedEvent> {
        self.buffer.extend_from_slice(chunk);
        let mut events = Vec::new();
        while let Some(position) = self.buffer.windows(2).position(|window| window == b"\n\n") {
            let block: Vec<u8> = self.buffer.drain(..position + 2).collect();
            if let Some(event) = parse_block(&block[..block.len() - 2]) {
                events.push(event);
            }
        }
        events
    }

    fn finish(&mut self) -> Option<DecodedEvent> {
        parse_block(&std::mem::take(&mut self.buffer))
    }
}

fn parse_block(block: &[u8]) -> Option<DecodedEvent> {
    let text = String::from_utf8_lossy(block);
    let mut event = None;
    let mut data = Vec::new();
    for line in text.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some(value) = line.strip_prefix("event: ") {
            event = Some(value.to_string());
        } else if let Some(value) = line.strip_prefix("data: ") {
            data.push(value);
        }
    }
    (!data.is_empty()).then(|| DecodedEvent {
        event,
        data: data.join("\n"),
    })
}

fn fixture(relative: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/protocol-compat")
        .join(relative);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()))
}

fn fixture_json(relative: &str) -> Value {
    serde_json::from_str(&fixture(relative)).unwrap()
}

fn sse_data_frames(sse: &str) -> Vec<String> {
    let mut decoder = SseDecoder::new();
    let mut frames: Vec<(Option<String>, String)> = Vec::new();
    for event in decoder.push(sse.as_bytes()) {
        frames.push((event.event, event.data));
    }
    if let Some(event) = decoder.finish() {
        frames.push((event.event, event.data));
    }
    frames
        .into_iter()
        .map(|(event, data)| match event {
            Some(event) => format!("{event}\n{data}"),
            None => data,
        })
        .collect()
}

#[test]
fn openai_request_fixture_decodes_to_ir() {
    let request = decode_request(
        octra_llm_core::Protocol::Openai,
        fixture_json("openai/chat-request.json"),
    )
    .unwrap();
    assert_eq!(request.model, "gpt-4o-mini");
    assert_eq!(
        request.system,
        SystemPrompt {
            blocks: vec![
                "You are a concise assistant.".to_string(),
                "Reply in English.".to_string()
            ]
        }
    );
    assert_eq!(request.temperature, Some(0.2));
    assert_eq!(request.top_p, Some(0.9));
    assert_eq!(request.max_tokens, Some(512));
    assert_eq!(request.stop, vec!["END".to_string()]);
    assert_eq!(request.stream, Some(false));
    // Unknown fields are preserved for same-protocol forwarding.
    assert_eq!(
        request.extra.get("metadata"),
        Some(&serde_json::json!({"trace": "abc"}))
    );

    assert_eq!(request.messages.len(), 3);
    let user = &request.messages[0];
    assert_eq!(user.role, Role::User);
    let parts = &user.content.as_ref().unwrap().parts;
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0].as_text(), Some("What is in this image?"));
    assert!(matches!(&parts[1], ContentPart::Image(_)));

    let assistant = &request.messages[1];
    assert_eq!(assistant.role, Role::Assistant);
    match &assistant.assistant[1] {
        AssistantContent::ToolCall {
            id,
            name,
            arguments,
            ..
        } => {
            assert_eq!(id, "call_abc123");
            assert_eq!(name, "get_weather");
            assert_eq!(arguments, &serde_json::json!({"city": "Paris"}));
        }
        other => panic!("unexpected block {other:?}"),
    }

    let tool = &request.messages[2];
    assert_eq!(tool.role, Role::Tool);
    assert_eq!(tool.tool_results[0].tool_call_id, "call_abc123");

    assert_eq!(request.tools.len(), 1);
    let function = request.tools[0].function.as_ref().unwrap();
    assert_eq!(function.name, "get_weather");
    assert_eq!(request.tool_choice, Some(serde_json::json!("auto")));
}

#[test]
fn openai_request_round_trips() {
    let original = fixture_json("openai/chat-request.json");
    let request = decode_request(octra_llm_core::Protocol::Openai, original.clone()).unwrap();
    let encoded = encode_request(octra_llm_core::Protocol::Openai, &request).unwrap();

    assert_eq!(encoded["model"], original["model"]);
    assert_eq!(encoded["messages"][0]["role"], "system");
    assert_eq!(
        encoded["messages"][0]["content"],
        "You are a concise assistant.\n\nReply in English."
    );
    assert_eq!(encoded["messages"][2]["tool_calls"][0]["id"], "call_abc123");
    assert_eq!(
        encoded["messages"][2]["tool_calls"][0]["function"]["arguments"],
        "{\"city\":\"Paris\"}"
    );
    assert_eq!(encoded["messages"][3]["tool_call_id"], "call_abc123");
    assert_eq!(encoded["stop"], "END");
    assert_eq!(encoded["tools"][0]["function"]["name"], "get_weather");
    assert_eq!(encoded["metadata"], original["metadata"]);

    // Decoding the re-encoded request yields the same IR. The system prompt
    // collapses to one block because that is all OpenAI can express.
    let again = decode_request(octra_llm_core::Protocol::Openai, encoded).unwrap();
    assert_eq!(again.messages, request.messages);
    assert_eq!(again.tools, request.tools);
    assert_eq!(
        again.system,
        SystemPrompt::text("You are a concise assistant.\n\nReply in English.")
    );
}

#[test]
fn openai_response_fixture_decodes_to_ir() {
    let response = decode_response(
        octra_llm_core::Protocol::Openai,
        fixture_json("openai/chat-response.json"),
    )
    .unwrap();
    assert_eq!(response.id, "chatcmpl-9f2c");
    assert_eq!(response.text(), "It is sunny in Paris.");
    assert_eq!(response.finish_reason, FinishReason::ToolCalls);
    assert_eq!(
        response.usage,
        Usage {
            input_tokens: 42,
            output_tokens: 17,
            cache_read_tokens: Some(8),
            cache_write_tokens: None,
            reasoning_tokens: None,
            cache_read_included_in_input: true,
        }
    );
    assert!(matches!(
        &response.message.assistant[1],
        AssistantContent::ToolCall { id, name, .. } if id == "call_abc123" && name == "get_weather"
    ));
}

#[test]
fn openai_response_round_trips() {
    let original = fixture_json("openai/chat-response.json");
    let response = decode_response(octra_llm_core::Protocol::Openai, original.clone()).unwrap();
    let encoded = encode_response(octra_llm_core::Protocol::Openai, &response).unwrap();
    assert_eq!(encoded["object"], "chat.completion");
    assert_eq!(
        encoded["choices"][0]["message"]["content"],
        "It is sunny in Paris."
    );
    assert_eq!(encoded["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(
        encoded["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
        "{\"city\":\"Paris\"}"
    );
    assert_eq!(encoded["usage"]["prompt_tokens"], 42);
    assert_eq!(encoded["usage"]["completion_tokens"], 17);
}

#[test]
fn anthropic_request_fixture_decodes_to_ir() {
    let request = decode_request(
        octra_llm_core::Protocol::Anthropic,
        fixture_json("anthropic/messages-request.json"),
    )
    .unwrap();
    assert_eq!(request.system.blocks.len(), 2);
    assert_eq!(request.max_tokens, Some(1024));
    assert_eq!(request.top_k, Some(40));
    assert_eq!(request.reasoning_effort.as_deref(), Some("low"));
    assert_eq!(request.parallel_tool_calls, Some(true));

    let user = &request.messages[0];
    let parts = &user.content.as_ref().unwrap().parts;
    assert!(matches!(&parts[1], ContentPart::Image(_)));
    assert!(matches!(&parts[2], ContentPart::File(_)));

    let assistant = &request.messages[1];
    assert!(matches!(
        &assistant.assistant[0],
        AssistantContent::Reasoning { signature: Some(sig), .. } if sig == "sig-1"
    ));
    assert!(matches!(
        &assistant.assistant[2],
        AssistantContent::ToolCall { id, arguments, .. }
            if id == "toolu_01abc" && arguments == &serde_json::json!({"city": "Paris"})
    ));

    let last = &request.messages[2];
    assert_eq!(last.tool_results.len(), 1);
    assert_eq!(
        last.tool_results[0].content.as_text(),
        Some("{\"temperature\":21}")
    );

    assert_eq!(request.tools.len(), 2);
    assert_eq!(request.tools[0].kind, ToolCallKind::Function);
    assert_eq!(request.tools[1].kind, ToolCallKind::Provider);
    assert_eq!(
        request.tools[1].provider_type.as_deref(),
        Some("web_search_20250305")
    );
}

#[test]
fn anthropic_request_round_trips() {
    let original = fixture_json("anthropic/messages-request.json");
    let request = decode_request(octra_llm_core::Protocol::Anthropic, original.clone()).unwrap();
    let encoded = encode_request(octra_llm_core::Protocol::Anthropic, &request).unwrap();
    assert_eq!(encoded["max_tokens"], 1024);
    assert_eq!(encoded["system"][0]["text"], "You are a concise assistant.");
    assert_eq!(encoded["messages"][1]["content"][0]["type"], "thinking");
    assert_eq!(encoded["messages"][1]["content"][0]["signature"], "sig-1");
    assert_eq!(encoded["messages"][1]["content"][2]["type"], "tool_use");
    assert_eq!(encoded["messages"][2]["content"][0]["type"], "tool_result");
    assert_eq!(encoded["tools"][0]["input_schema"]["type"], "object");
    assert_eq!(encoded["tools"][1]["type"], "web_search_20250305");
    assert_eq!(encoded["thinking"]["budget_tokens"], 2048);

    let again = decode_request(octra_llm_core::Protocol::Anthropic, encoded).unwrap();
    assert_eq!(again.messages, request.messages);
    assert_eq!(again.system, request.system);
    assert_eq!(again.tools, request.tools);
}

#[test]
fn anthropic_response_fixture_decodes_to_ir() {
    let response = decode_response(
        octra_llm_core::Protocol::Anthropic,
        fixture_json("anthropic/messages-response.json"),
    )
    .unwrap();
    assert_eq!(response.id, "msg_01xyz");
    assert_eq!(response.finish_reason, FinishReason::ToolCalls);
    assert_eq!(response.text(), "It is sunny in Paris.");
    assert_eq!(response.usage.cache_read_tokens, Some(8));
    let reasoning = response.reasoning.as_ref().expect("reasoning summary");
    assert_eq!(reasoning.text, "I should check the weather.");
    assert_eq!(reasoning.signature.as_deref(), Some("sig-1"));
}

#[test]
fn anthropic_response_round_trips() {
    let response = decode_response(
        octra_llm_core::Protocol::Anthropic,
        fixture_json("anthropic/messages-response.json"),
    )
    .unwrap();
    let encoded = encode_response(octra_llm_core::Protocol::Anthropic, &response).unwrap();
    assert_eq!(encoded["type"], "message");
    assert_eq!(encoded["stop_reason"], "tool_use");
    assert_eq!(encoded["content"][0]["type"], "thinking");
    assert_eq!(encoded["content"][1]["text"], "It is sunny in Paris.");
    assert_eq!(encoded["content"][2]["type"], "tool_use");
    assert_eq!(encoded["content"][2]["input"]["city"], "Paris");
    assert_eq!(encoded["usage"]["input_tokens"], 42);
    assert_eq!(encoded["usage"]["output_tokens"], 17);
}

#[test]
fn openai_stream_fixture_decodes() {
    let frames = sse_data_frames(&fixture("openai/chat-stream.sse"));
    let mut decoder = stream_decoder(octra_llm_core::Protocol::Openai);
    let mut events = Vec::new();
    for frame in &frames {
        events.extend(decoder.decode_chunk(frame).unwrap());
    }
    events.extend(decoder.finish());

    assert_eq!(
        events[0],
        StreamEvent::MessageStart {
            id: "chatcmpl-9f2c".into(),
            model: "gpt-4o-mini".into()
        }
    );
    let text: String = events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "It is sunny.");
    assert!(events.iter().any(|event| matches!(
        event,
        StreamEvent::ToolCallStart { id, name, .. } if id == "call_abc123" && name == "get_weather"
    )));
    let arguments: String = events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::ToolCallDelta { arguments, .. } => Some(arguments.as_str()),
            _ => None,
        })
        .collect();
    let parsed: Value = serde_json::from_str(&arguments).unwrap();
    assert_eq!(parsed, serde_json::json!({"city": "Paris"}));
    assert!(events.contains(&StreamEvent::Finish {
        reason: FinishReason::ToolCalls
    }));
    assert!(events.iter().any(|event| matches!(
        event,
        StreamEvent::Usage(usage) if usage.input_tokens == 42 && usage.output_tokens == 17
    )));
}

#[test]
fn openai_stream_reencodes_to_openai_sse() {
    let response = stream_roundtrip(octra_llm_core::Protocol::Openai, "openai/chat-stream.sse");
    let text = render(&response);
    assert!(text.contains(r#""object":"chat.completion.chunk""#));
    assert!(text.contains(r#""content":"It is ""#));
    assert!(text.contains(r#""finish_reason":"tool_calls""#));
    assert!(text.trim_end().ends_with("data: [DONE]"));
}

#[test]
fn anthropic_stream_fixture_decodes() {
    let frames = sse_data_frames(&fixture("anthropic/messages-stream.sse"));
    let mut decoder = stream_decoder(octra_llm_core::Protocol::Anthropic);
    let mut events = Vec::new();
    for frame in &frames {
        events.extend(decoder.decode_chunk(frame).unwrap());
    }
    events.extend(decoder.finish());

    assert_eq!(
        events[0],
        StreamEvent::MessageStart {
            id: "msg_01xyz".into(),
            model: "claude-sonnet-4-5".into()
        }
    );
    let reasoning: String = events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::ReasoningDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(reasoning, "I should check the weather.");
    assert!(events.contains(&StreamEvent::ReasoningSignatureDelta {
        signature: "sig-1".into()
    }));
    let text: String = events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "It is sunny in Paris.");
    assert!(events.contains(&StreamEvent::ToolCallStart {
        index: 2,
        id: "toolu_01abc".into(),
        name: "get_weather".into(),
        kind: ToolCallKind::Function,
    }));
    assert!(events.contains(&StreamEvent::Usage(Usage {
        input_tokens: 42,
        output_tokens: 17,
        cache_read_tokens: None,
        cache_write_tokens: None,
        reasoning_tokens: None,
        cache_read_included_in_input: false,
    })));
    assert!(events.contains(&StreamEvent::Finish {
        reason: FinishReason::ToolCalls
    }));
}

#[test]
fn anthropic_stream_reencodes_to_anthropic_sse() {
    let response = stream_roundtrip(
        octra_llm_core::Protocol::Anthropic,
        "anthropic/messages-stream.sse",
    );
    let text = render(&response);
    assert!(text.contains("event: message_start"));
    assert!(text.contains("event: content_block_start"));
    assert!(text.contains(r#""type":"thinking_delta""#));
    assert!(text.contains(r#""type":"text_delta""#));
    assert!(text.contains(r#""type":"input_json_delta""#));
    assert!(text.contains(r#""stop_reason":"tool_use""#));
    assert!(
        text.trim_end()
            .ends_with("event: message_stop\ndata: {\"type\":\"message_stop\"}")
    );
}

#[test]
fn cross_protocol_requests() {
    // OpenAI request into an Anthropic upstream.
    let request = decode_request(
        octra_llm_core::Protocol::Openai,
        fixture_json("openai/chat-request.json"),
    )
    .unwrap();
    let anthropic_body = encode_request(octra_llm_core::Protocol::Anthropic, &request).unwrap();
    assert_eq!(
        anthropic_body["system"][0]["text"],
        "You are a concise assistant."
    );
    assert_eq!(anthropic_body["messages"][0]["content"][1]["type"], "image");
    assert_eq!(
        anthropic_body["messages"][1]["content"][1]["type"],
        "tool_use"
    );
    assert_eq!(
        anthropic_body["messages"][1]["content"][1]["id"],
        "call_abc123"
    );
    assert_eq!(
        anthropic_body["messages"][2]["content"][0]["tool_use_id"],
        "call_abc123"
    );
    assert_eq!(anthropic_body["stop_sequences"][0], "END");

    // Anthropic request into an OpenAI upstream.
    let request = decode_request(
        octra_llm_core::Protocol::Anthropic,
        fixture_json("anthropic/messages-request.json"),
    )
    .unwrap();
    let openai_body = encode_request(octra_llm_core::Protocol::Openai, &request).unwrap();
    assert_eq!(openai_body["messages"][0]["role"], "system");
    assert!(
        openai_body["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("You are a concise assistant.")
    );
    assert_eq!(
        openai_body["messages"][2]["tool_calls"][0]["id"],
        "toolu_01abc"
    );
    // The provider tool has no OpenAI equivalent: only the function remains.
    assert_eq!(openai_body["tools"].as_array().unwrap().len(), 1);
    assert_eq!(openai_body["max_tokens"], 1024);
    // top_k has no OpenAI equivalent.
    assert!(openai_body.get("top_k").is_none());
    assert_eq!(openai_body["stop"], "END");
}

#[test]
fn cross_protocol_responses() {
    let response = decode_response(
        octra_llm_core::Protocol::Anthropic,
        fixture_json("anthropic/messages-response.json"),
    )
    .unwrap();
    let openai_body = encode_response(octra_llm_core::Protocol::Openai, &response).unwrap();
    assert_eq!(
        openai_body["choices"][0]["message"]["content"],
        "It is sunny in Paris."
    );
    assert_eq!(openai_body["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(
        openai_body["choices"][0]["message"]["tool_calls"][0]["id"],
        "toolu_01abc"
    );
    assert_eq!(
        openai_body["choices"][0]["message"]["reasoning_content"],
        "I should check the weather."
    );

    let response = decode_response(
        octra_llm_core::Protocol::Openai,
        fixture_json("openai/chat-response.json"),
    )
    .unwrap();
    let anthropic_body = encode_response(octra_llm_core::Protocol::Anthropic, &response).unwrap();
    assert_eq!(anthropic_body["stop_reason"], "tool_use");
    assert_eq!(anthropic_body["content"][1]["type"], "tool_use");
    assert_eq!(anthropic_body["content"][1]["input"]["city"], "Paris");
    assert_eq!(anthropic_body["usage"]["input_tokens"], 42);
}

#[test]
fn openai_stream_to_anthropic_ingress() {
    let response = stream_roundtrip_cross(
        octra_llm_core::Protocol::Openai,
        octra_llm_core::Protocol::Anthropic,
        "openai/chat-stream.sse",
    );
    let text = render(&response);
    assert!(text.contains("event: message_start"));
    assert!(text.contains(r#"{"delta":{"text":"It is ","type":"text_delta"}"#));
    assert!(text.contains(r#""content_block":{"id":"call_abc123""#));
    assert!(text.contains(r#""stop_reason":"tool_use""#));
}

#[test]
fn anthropic_stream_to_openai_ingress() {
    let response = stream_roundtrip_cross(
        octra_llm_core::Protocol::Anthropic,
        octra_llm_core::Protocol::Openai,
        "anthropic/messages-stream.sse",
    );
    let text = render(&response);
    assert!(text.contains(r#""reasoning_content":"I should check""#));
    assert!(text.contains(r#""content":"It is sunny ""#));
    assert!(text.contains(r#"{"arguments":"{\"city\":"}"#));
    assert!(text.contains(r#""finish_reason":"tool_calls""#));
    assert!(text.trim_end().ends_with("data: [DONE]"));
}

#[test]
fn system_prompt_only_openai_request_is_rejected() {
    let value = serde_json::json!({
        "model": "gpt",
        "messages": [{"role": "system", "content": "only"}]
    });
    assert!(decode_request(octra_llm_core::Protocol::Openai, value).is_err());
}

#[test]
fn anthropic_missing_max_tokens_defaults() {
    let value = serde_json::json!({
        "model": "claude",
        "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]
    });
    let request = decode_request(octra_llm_core::Protocol::Anthropic, value).unwrap();
    assert_eq!(request.max_tokens, Some(4096));
}

#[test]
fn error_envelopes_match_ingress_protocol() {
    use octra_llm_core::GatewayError;
    let error = GatewayError::invalid_request("bad input").with_code("bad_input");

    let openai_error = octra_llm_protocol::encode_error(octra_llm_core::Protocol::Openai, &error);
    assert_eq!(openai_error["error"]["type"], "invalid_request_error");

    let anthropic_error =
        octra_llm_protocol::encode_error(octra_llm_core::Protocol::Anthropic, &error);
    assert_eq!(anthropic_error["type"], "error");
    assert_eq!(anthropic_error["error"]["type"], "invalid_request_error");
    assert_eq!(anthropic_error["error"]["message"], "bad input");

    let frames = octra_llm_protocol::encode_stream_error(octra_llm_core::Protocol::Openai, &error);
    assert_eq!(frames.len(), 1);
    let parsed = parse_sse_json(&frames[0].data).unwrap().unwrap();
    assert_eq!(parsed["error"]["message"], "bad input");
}

#[test]
fn tool_call_with_invalid_json_arguments_round_trips_raw() {
    let value = serde_json::json!({
        "id": "chatcmpl-x",
        "model": "m",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "f", "arguments": "{broken"}
                }]
            },
            "finish_reason": "tool_calls"
        }]
    });
    let response = decode_response(octra_llm_core::Protocol::Openai, value).unwrap();
    let encoded = encode_response(octra_llm_core::Protocol::Openai, &response).unwrap();
    assert_eq!(
        encoded["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
        "{broken"
    );
}

#[test]
fn openai_reasoning_content_is_preserved() {
    let value = serde_json::json!({
        "id": "chatcmpl-x",
        "model": "deepseek-reasoner",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "answer",
                "reasoning_content": "thought process"
            },
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3}
    });
    let response = decode_response(octra_llm_core::Protocol::Openai, value).unwrap();
    assert!(matches!(
        &response.message.assistant[0],
        AssistantContent::Reasoning { text, .. } if text == "thought process"
    ));
    let anthropic = encode_response(octra_llm_core::Protocol::Anthropic, &response).unwrap();
    assert_eq!(anthropic["content"][0]["type"], "thinking");
    assert_eq!(anthropic["content"][0]["thinking"], "thought process");
    assert_eq!(anthropic["stop_reason"], "end_turn");
}

#[test]
fn invalid_payloads_are_rejected() {
    assert!(
        decode_request(
            octra_llm_core::Protocol::Openai,
            serde_json::json!({"messages": []})
        )
        .is_err()
    );
    assert!(decode_request(octra_llm_core::Protocol::Openai, serde_json::json!([])).is_err());
    assert!(
        decode_response(
            octra_llm_core::Protocol::Anthropic,
            serde_json::json!({"error": {"message": "x"}})
        )
        .is_err()
    );
    assert!(
        decode_response(
            octra_llm_core::Protocol::Openai,
            serde_json::json!({"choices": []})
        )
        .is_err()
    );
}

#[test]
fn builtin_tool_encoding_for_openai_is_dropped() {
    let tools = vec![Tool {
        kind: ToolCallKind::Provider,
        name: Some("web_search".into()),
        function: None,
        provider_type: Some("web_search_20250305".into()),
        provider_config: Some(serde_json::json!({
            "type": "web_search_20250305",
            "name": "web_search"
        })),
        extra: Default::default(),
    }];
    let mut request = ChatRequest::new("m", vec![Message::user("hi")]);
    request.tools = tools;
    let openai = encode_request(octra_llm_core::Protocol::Openai, &request).unwrap();
    assert!(openai.get("tools").is_none());
    let anthropic = encode_request(octra_llm_core::Protocol::Anthropic, &request).unwrap();
    assert_eq!(anthropic["tools"][0]["type"], "web_search_20250305");
}

fn stream_roundtrip(protocol: octra_llm_core::Protocol, fixture_path: &str) -> Vec<u8> {
    stream_roundtrip_cross(protocol, protocol, fixture_path)
}

fn stream_roundtrip_cross(
    upstream: octra_llm_core::Protocol,
    ingress: octra_llm_core::Protocol,
    fixture_path: &str,
) -> Vec<u8> {
    let mut decoder = stream_decoder(upstream);
    let mut encoder = stream_encoder(ingress);
    let mut out = Vec::new();
    for frame in sse_data_frames(&fixture(fixture_path)) {
        for event in decoder.decode_chunk(&frame).unwrap() {
            for sse in encoder.encode(&event).unwrap() {
                out.extend_from_sse(sse);
            }
        }
    }
    for event in decoder.finish() {
        for sse in encoder.encode(&event).unwrap() {
            out.extend_from_sse(sse);
        }
    }
    for sse in encoder.finish().unwrap() {
        out.extend_from_sse(sse);
    }
    out
}

trait ExtendSse {
    fn extend_from_sse(&mut self, frame: octra_llm_protocol::SseFrame);
}

impl ExtendSse for Vec<u8> {
    fn extend_from_sse(&mut self, frame: octra_llm_protocol::SseFrame) {
        self.extend(frame.to_bytes());
    }
}

fn render(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).unwrap()
}

// Keep the module imports exercised even when assertions are trimmed.
#[allow(dead_code)]
fn _type_check(_: ChatResponse, _: ChatRequest) {}
#[allow(dead_code)]
fn _adapter_helpers_exist() {
    let _ = openai::request::encode_arguments;
    let _ = anthropic::response::stop_reason_to;
}

#[test]
fn openai_responses_round_trip() {
    use octra_llm_core::Protocol;
    let request = serde_json::json!({
        "model": "gpt-5",
        "instructions": "be concise",
        "input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]},
            {"type": "function_call", "call_id": "call_1", "name": "lookup", "arguments": "{\"q\":1}"},
            {"type": "function_call_output", "call_id": "call_1", "output": "ok"}
        ],
        "tools": [{"type": "function", "name": "lookup", "parameters": {"type": "object"}}],
        "max_output_tokens": 64,
        "store": true
    });
    let ir = decode_request(Protocol::OpenaiResponses, request).unwrap();
    assert_eq!(ir.model, "gpt-5");
    assert_eq!(ir.system.blocks, vec!["be concise"]);
    assert_eq!(ir.messages.len(), 3);
    assert_eq!(ir.max_tokens, Some(64));
    assert_eq!(ir.tools.len(), 1);
    assert_eq!(ir.extra["store"], true);

    let encoded = encode_request(Protocol::OpenaiResponses, &ir).unwrap();
    let back = decode_request(Protocol::OpenaiResponses, encoded).unwrap();
    assert_eq!(back.system.blocks, ir.system.blocks);
    assert_eq!(back.messages.len(), ir.messages.len());

    let response = serde_json::json!({
        "id": "resp_1",
        "model": "gpt-5",
        "status": "completed",
        "output": [
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "hello"}]},
            {"type": "function_call", "call_id": "call_2", "name": "lookup", "arguments": "{\"q\":2}"}
        ],
        "usage": {"input_tokens": 3, "output_tokens": 2, "output_tokens_details": {"reasoning_tokens": 1}}
    });
    let decoded = decode_response(Protocol::OpenaiResponses, response).unwrap();
    assert_eq!(decoded.text(), "hello");
    assert_eq!(decoded.finish_reason, FinishReason::ToolCalls);
    assert_eq!(decoded.usage.input_tokens, 3);
    assert_eq!(decoded.usage.reasoning_tokens, Some(1));
    let encoded = encode_response(Protocol::OpenaiResponses, &decoded).unwrap();
    assert_eq!(encoded["object"], "response");
    assert_eq!(encoded["output_text"], "hello");
}

#[test]
fn gemini_round_trip() {
    use octra_llm_core::Protocol;
    let request = serde_json::json!({
        "contents": [
            {"role": "user", "parts": [{"text": "hi"}]},
            {"role": "model", "parts": [{"functionCall": {"name": "lookup", "args": {"q": 1}}}]},
            {"role": "user", "parts": [{"functionResponse": {"name": "lookup", "response": {"content": "ok"}}}]}
        ],
        "systemInstruction": {"parts": [{"text": "sys"}]},
        "tools": [{"functionDeclarations": [{"name": "lookup", "parameters": {"type": "object"}}]}],
        "generationConfig": {"temperature": 0.2, "maxOutputTokens": 50}
    });
    let ir = decode_request(Protocol::Gemini, request).unwrap();
    assert_eq!(ir.system.blocks, vec!["sys"]);
    assert_eq!(ir.messages.len(), 3);
    assert_eq!(ir.max_tokens, Some(50));
    assert_eq!(ir.tools.len(), 1);

    let encoded = encode_request(Protocol::Gemini, &ir).unwrap();
    let back = decode_request(Protocol::Gemini, encoded).unwrap();
    assert_eq!(back.system.blocks, ir.system.blocks);
    assert_eq!(back.messages.len(), ir.messages.len());

    let response = serde_json::json!({
        "candidates": [{
            "content": {"role": "model", "parts": [
                {"text": "thinking", "thought": true},
                {"text": "hello"}
            ]},
            "finishReason": "STOP"
        }],
        "usageMetadata": {"promptTokenCount": 4, "candidatesTokenCount": 5, "thoughtsTokenCount": 2}
    });
    let decoded = decode_response(Protocol::Gemini, response).unwrap();
    assert_eq!(decoded.text(), "hello");
    assert_eq!(decoded.finish_reason, FinishReason::Stop);
    assert_eq!(decoded.usage.input_tokens, 4);
    assert_eq!(decoded.usage.reasoning_tokens, Some(2));
    let encoded = encode_response(Protocol::Gemini, &decoded).unwrap();
    assert_eq!(encoded["candidates"][0]["finishReason"], "STOP");
    assert_eq!(encoded["usageMetadata"]["promptTokenCount"], 4);
}

#[test]
fn openai_responses_stream_round_trip() {
    use octra_llm_core::Protocol;
    let upstream = concat!(
        "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-5\"}}\n\n",
        "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"Hello\"}\n\n",
        "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}}\n\n"
    );
    let mut decoder = stream_decoder(Protocol::OpenaiResponses);
    let mut events = Vec::new();
    for frame in sse_data_frames(upstream) {
        let data = frame
            .split_once('\n')
            .map(|(_, data)| data)
            .unwrap_or(&frame);
        events.extend(decoder.decode_chunk(data).unwrap());
    }
    events.extend(decoder.finish());
    assert!(
        events
            .iter()
            .any(|event| matches!(event, StreamEvent::TextDelta { text } if text == "Hello"))
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, StreamEvent::Usage(usage) if usage.output_tokens == 2))
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, StreamEvent::Finish { .. }))
    );

    let mut encoder = stream_encoder(Protocol::OpenaiResponses);
    let mut out = Vec::new();
    for event in &events {
        for frame in encoder.encode(event).unwrap() {
            out.extend_from_sse(frame);
        }
    }
    for frame in encoder.finish().unwrap() {
        out.extend_from_sse(frame);
    }
    let rendered = render(&out);
    assert!(rendered.contains("response.output_text.delta"));
    assert!(rendered.contains("response.completed"));
}

#[test]
fn gemini_stream_round_trip() {
    use octra_llm_core::Protocol;
    let upstream = concat!(
        "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"Hello\"}]}}],\"modelVersion\":\"gemini-test\"}\n\n",
        "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"functionCall\":{\"name\":\"lookup\",\"args\":{\"q\":1}}}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":1,\"candidatesTokenCount\":2}}\n\n"
    );
    let mut decoder = stream_decoder(Protocol::Gemini);
    let mut events = Vec::new();
    for frame in sse_data_frames(upstream) {
        events.extend(decoder.decode_chunk(&frame).unwrap());
    }
    events.extend(decoder.finish());
    assert!(
        events
            .iter()
            .any(|event| matches!(event, StreamEvent::TextDelta { text } if text == "Hello"))
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, StreamEvent::ToolCallStart { .. }))
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, StreamEvent::Usage(_)))
    );

    let mut encoder = stream_encoder(Protocol::Gemini);
    let mut out = Vec::new();
    for event in &events {
        for frame in encoder.encode(event).unwrap() {
            out.extend_from_sse(frame);
        }
    }
    for frame in encoder.finish().unwrap() {
        out.extend_from_sse(frame);
    }
    let rendered = render(&out);
    assert!(rendered.contains("functionCall"));
    assert!(rendered.contains("finishReason"));
}

#[test]
fn openai_max_completion_tokens_round_trips() {
    let request = decode_request(
        octra_llm_core::Protocol::Openai,
        serde_json::json!({
            "model": "gpt-5",
            "messages": [{"role": "user", "content": "hi"}],
            "max_completion_tokens": 64
        }),
    )
    .unwrap();
    let encoded = encode_request(octra_llm_core::Protocol::Openai, &request).unwrap();
    assert_eq!(encoded["max_completion_tokens"], 64);
    assert!(encoded.get("max_tokens").is_none());
}

#[test]
fn anthropic_accepts_string_content_and_reports_max_tokens() {
    let request = decode_request(
        octra_llm_core::Protocol::Anthropic,
        serde_json::json!({
            "model": "claude",
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "hello"}]
        }),
    )
    .unwrap();
    assert_eq!(request.messages.len(), 1);

    let response = decode_response(
        octra_llm_core::Protocol::Anthropic,
        serde_json::json!({
            "id": "msg-1",
            "model": "claude",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "hi"}],
            "stop_reason": "max_tokens",
            "usage": {"input_tokens": 1, "output_tokens": 2}
        }),
    )
    .unwrap();
    assert_eq!(response.finish_reason, octra_llm_core::FinishReason::Length);
}

#[test]
fn gemini_thought_signatures_survive_ir_round_trips() {
    let request = decode_request(
        octra_llm_core::Protocol::Gemini,
        serde_json::json!({
            "contents": [{
                "role": "model",
                "parts": [{
                    "functionCall": {"name": "lookup", "args": {"q": "x"}},
                    "thoughtSignature": "sig-call"
                }]
            }]
        }),
    )
    .unwrap();
    let encoded = encode_request(octra_llm_core::Protocol::Gemini, &request).unwrap();
    assert_eq!(
        encoded["contents"][0]["parts"][0]["thoughtSignature"],
        "sig-call"
    );

    let response = decode_response(
        octra_llm_core::Protocol::Gemini,
        serde_json::json!({
            "candidates": [{
                "content": {"parts": [{
                    "functionCall": {"name": "lookup", "args": {"q": "x"}},
                    "thoughtSignature": "sig-response"
                }]},
                "finishReason": "STOP"
            }],
            "modelVersion": "gemini-2.5"
        }),
    )
    .unwrap();
    let encoded = encode_response(octra_llm_core::Protocol::Gemini, &response).unwrap();
    assert_eq!(
        encoded["candidates"][0]["content"]["parts"][0]["thoughtSignature"],
        "sig-response"
    );
}
