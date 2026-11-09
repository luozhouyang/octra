//! Anthropic Messages responses and error envelopes.

use octra_llm_core::{
    AssistantContent, ChatResponse, FinishReason, GatewayError, Map, Message, ProtocolError,
    ProtocolResult, Reasoning, Role, ToolCallKind, Usage, Value,
};
use serde_json::json;

use crate::SseFrame;
use crate::common::sse_event_frame;

pub fn decode_response(value: Value) -> ProtocolResult<ChatResponse> {
    let object = value
        .as_object()
        .ok_or_else(|| ProtocolError::new("response body must be a JSON object"))?;
    if let Some(error) = object.get("error") {
        return Err(ProtocolError::new(format!(
            "upstream returned an error: {}",
            error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
        )));
    }
    let content = object
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| ProtocolError::new("response has no content array"))?;

    let mut assistant = Vec::new();
    let mut reasoning = None;
    let mut encrypted = Vec::new();
    for block in content {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                let text = block
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if let Some(citations) = block.get("citations") {
                    assistant.push(AssistantContent::Raw {
                        kind: "text_with_citations".into(),
                        raw: json!({"type": "text", "text": text, "citations": citations}),
                    });
                } else {
                    assistant.push(AssistantContent::Text { text });
                }
            }
            Some("thinking") => {
                let text = block
                    .get("thinking")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let signature = block
                    .get("signature")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                assistant.push(AssistantContent::Reasoning {
                    text: text.clone(),
                    signature: signature.clone(),
                });
                reasoning.get_or_insert_with(|| Reasoning {
                    text: String::new(),
                    signature: None,
                    encrypted: Vec::new(),
                });
                if let Some(summary) = &mut reasoning {
                    summary.text.push_str(&text);
                    if summary.signature.is_none() {
                        summary.signature = signature;
                    }
                }
            }
            Some("redacted_thinking") => {
                encrypted.push(block.get("data").cloned().unwrap_or(Value::Null));
            }
            Some("tool_use") => assistant.push(AssistantContent::ToolCall {
                id: block
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("toolu_missing")
                    .to_string(),
                name: block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                arguments: block.get("input").cloned().unwrap_or(json!({})),
                kind: None,
            }),
            Some("server_tool_use") => assistant.push(AssistantContent::ToolCall {
                id: block
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("srvtoolu_missing")
                    .to_string(),
                name: block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                arguments: block.get("input").cloned().unwrap_or(json!({})),
                kind: Some(ToolCallKind::Provider),
            }),
            Some("web_search_tool_result")
            | Some("web_fetch_tool_result")
            | Some("code_execution_tool_result")
            | Some("bash_code_execution_tool_result")
            | Some("text_editor_code_execution_tool_result")
            | Some("tool_search_tool_result") => assistant.push(AssistantContent::Raw {
                kind: block
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("tool_result")
                    .to_string(),
                raw: block.clone(),
            }),
            Some(other) => assistant.push(AssistantContent::Raw {
                kind: other.to_string(),
                raw: block.clone(),
            }),
            None => {}
        }
    }
    if let Some(summary) = &mut reasoning {
        summary.encrypted = encrypted;
    }

    let stop_reason = object.get("stop_reason").and_then(Value::as_str);
    let finish_reason = match stop_reason {
        Some("tool_use") => FinishReason::ToolCalls,
        Some("max_tokens") => FinishReason::Length,
        Some("pause_turn") => FinishReason::Pause,
        Some("refusal") => FinishReason::Refusal,
        _ => match object.get("stop_sequence") {
            Some(Value::String(_)) => FinishReason::Stop,
            _ => FinishReason::Stop,
        },
    };

    Ok(ChatResponse {
        id: object
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("msg_unknown")
            .to_string(),
        model: object
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        message: Message {
            role: Role::Assistant,
            content: None,
            assistant,
            tool_results: Vec::new(),
            name: None,
            extra: Map::new(),
        },
        finish_reason,
        usage: decode_usage(object.get("usage")),
        reasoning,
        programmatic: None,
        extra: Map::new(),
    })
}

pub fn decode_usage(value: Option<&Value>) -> Usage {
    let Some(value) = value else {
        return Usage::default();
    };
    Usage {
        input_tokens: value
            .get("input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        output_tokens: value
            .get("output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cache_read_tokens: value.get("cache_read_input_tokens").and_then(Value::as_u64),
        cache_write_tokens: value
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64),
        reasoning_tokens: None,
        cache_read_included_in_input: false,
    }
}

pub fn encode_usage(usage: &Usage) -> Value {
    let mut object = Map::new();
    object.insert("input_tokens".to_string(), json!(usage.input_tokens));
    object.insert("output_tokens".to_string(), json!(usage.output_tokens));
    if let Some(read) = usage.cache_read_tokens {
        object.insert("cache_read_input_tokens".to_string(), json!(read));
    }
    if let Some(write) = usage.cache_write_tokens {
        object.insert("cache_creation_input_tokens".to_string(), json!(write));
    }
    Value::Object(object)
}

pub fn stop_reason_to(reason: &FinishReason) -> &'static str {
    match reason {
        FinishReason::ToolCalls => "tool_use",
        FinishReason::Length => "max_tokens",
        FinishReason::Pause => "pause_turn",
        FinishReason::Refusal => "refusal",
        FinishReason::Stop | FinishReason::ContentFilter | FinishReason::Other => "end_turn",
    }
}

pub fn encode_response(response: &ChatResponse) -> ProtocolResult<Value> {
    let mut content = Vec::new();
    for block in &response.message.assistant {
        match block {
            AssistantContent::Reasoning { text, signature } => {
                let mut thinking = Map::new();
                thinking.insert("type".to_string(), json!("thinking"));
                thinking.insert("thinking".to_string(), json!(text));
                if let Some(signature) = signature {
                    thinking.insert("signature".to_string(), json!(signature));
                } else {
                    thinking.insert("signature".to_string(), json!(""));
                }
                content.push(Value::Object(thinking));
            }
            AssistantContent::Text { text } => {
                content.push(json!({"type": "text", "text": text}));
            }
            AssistantContent::ToolCall {
                id,
                name,
                arguments,
                kind: _,
            } => content.push(json!({
                "type": "tool_use",
                "id": id,
                "name": name,
                "input": arguments,
            })),
            AssistantContent::Raw { kind, raw } => match kind.as_str() {
                "openai_tool_call" => {
                    let function = raw.get("function").cloned().unwrap_or(Value::Null);
                    content.push(json!({
                        "type": "tool_use",
                        "id": raw.get("id").cloned().unwrap_or(json!("toolu_missing")),
                        "name": function.get("name").cloned().unwrap_or(json!("")),
                        "input": parse_input(function.get("arguments")),
                    }));
                }
                "text_with_citations" => content.push(raw.clone()),
                _ => content.push(raw.clone()),
            },
            // Tool results chosen by the model carry no user-facing meaning.
            AssistantContent::ToolResult { .. } | AssistantContent::Refusal { .. } => {}
        }
    }
    if response
        .reasoning
        .as_ref()
        .is_some_and(|r| !r.text.is_empty())
        && !content
            .iter()
            .any(|block| block.get("type").and_then(Value::as_str) == Some("thinking"))
    {
        let reasoning = response.reasoning.as_ref().expect("checked");
        content.insert(
            0,
            json!({
                "type": "thinking",
                "thinking": reasoning.text,
                "signature": reasoning.signature.clone().unwrap_or_default(),
            }),
        );
    }

    Ok(json!({
        "id": response.id,
        "type": "message",
        "role": "assistant",
        "model": response.model,
        "content": content,
        "stop_reason": stop_reason_to(&response.finish_reason),
        "stop_sequence": Value::Null,
        "usage": encode_usage(&response.usage),
    }))
}

fn parse_input(value: Option<&Value>) -> Value {
    match value {
        Some(Value::String(text)) => serde_json::from_str(text).unwrap_or(json!({})),
        Some(other) => other.clone(),
        None => json!({}),
    }
}

pub fn error_type(error: &GatewayError) -> &'static str {
    use octra_llm_core::ErrorKind;
    match error.kind {
        ErrorKind::InvalidRequest => "invalid_request_error",
        ErrorKind::Unauthorized => "authentication_error",
        ErrorKind::Forbidden => "permission_error",
        ErrorKind::NotFound => "not_found_error",
        ErrorKind::PayloadTooLarge => "request_too_large",
        ErrorKind::RateLimited => "rate_limit_error",
        ErrorKind::Upstream | ErrorKind::Config => "api_error",
        ErrorKind::Timeout => "timeout_error",
        ErrorKind::Internal => "api_error",
    }
}

pub fn error_body(error: &GatewayError) -> Value {
    if let Some(body) = &error.upstream_body
        && let Some(provider_error) = body.get("error")
    {
        let mut forwarded = provider_error.clone();
        if let Some(map) = forwarded.as_object_mut() {
            map.entry("type".to_string())
                .or_insert_with(|| json!(error_type(error)));
            map.entry("message".to_string())
                .or_insert_with(|| json!(error.message));
        }
        return json!({"type": "error", "error": forwarded});
    }
    json!({
        "type": "error",
        "error": {
            "type": error_type(error),
            "message": error.message,
        }
    })
}

pub fn error_frames(error: &GatewayError) -> Vec<SseFrame> {
    vec![sse_event_frame("error", &error_body(error))]
}

/// Whether a decoded block is a provider tool result (used by adapters and
/// tests that inspect raw blocks).
pub fn is_provider_tool_result(block: &AssistantContent) -> bool {
    match block {
        AssistantContent::Raw { kind, .. } => kind.ends_with("_tool_result"),
        AssistantContent::ToolCall { kind, .. } => {
            matches!(kind, Some(ToolCallKind::Provider))
        }
        _ => false,
    }
}
