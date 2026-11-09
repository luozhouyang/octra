//! OpenAI Chat Completions responses and error envelopes.

use octra_llm_core::{
    AssistantContent, ChatResponse, FinishReason, GatewayError, Map, Message, ProtocolError,
    ProtocolResult, Role, Usage, Value,
};
use serde_json::json;

use super::request::{decode_arguments, encode_arguments};
use crate::common::{SseFrame, restore_tool_ids, sse_data_frame};

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

    let id = object
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("chatcmpl-unknown")
        .to_string();
    let model = object
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let choice = object
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .ok_or_else(|| ProtocolError::new("response has no choices"))?;

    let message = decode_message(choice)?;
    let finish_reason = finish_reason_from(choice.get("finish_reason"));
    let usage = decode_usage(object.get("usage"));

    Ok(ChatResponse {
        id,
        model,
        message,
        finish_reason,
        usage,
        reasoning: None,
        programmatic: None,
        extra: Map::new(),
    })
}

fn decode_message(choice: &Value) -> ProtocolResult<Message> {
    let message = choice
        .get("message")
        .and_then(Value::as_object)
        .ok_or_else(|| ProtocolError::new("choice has no message"))?;
    let mut assistant = Vec::new();

    if let Some(reasoning) = message
        .get("reasoning_content")
        .or_else(|| message.get("reasoning"))
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
    {
        assistant.push(AssistantContent::Reasoning {
            text: reasoning.to_string(),
            signature: None,
        });
    }

    let text = match message.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    };
    if !text.is_empty() {
        assistant.push(AssistantContent::Text { text });
    }
    if let Some(refusal) = message.get("refusal").and_then(Value::as_str) {
        assistant.push(AssistantContent::Refusal {
            text: refusal.to_string(),
        });
    }

    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        for call in calls {
            let function = call.get("function").cloned().unwrap_or(Value::Null);
            let (arguments, _) = decode_arguments(function.get("arguments"));
            assistant.push(AssistantContent::ToolCall {
                id: call
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("call_missing")
                    .to_string(),
                name: function
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                arguments,
                kind: None,
            });
        }
    }

    let mut annotations = Vec::new();
    if let Some(list) = message.get("annotations") {
        annotations.push(list.clone());
    }

    Ok(Message {
        role: Role::Assistant,
        content: None,
        assistant,
        tool_results: Vec::new(),
        name: None,
        extra: if annotations.is_empty() {
            Map::new()
        } else {
            let mut extra = Map::new();
            extra.insert("annotations".to_string(), annotations.remove(0));
            extra
        },
    })
}

pub fn finish_reason_from(value: Option<&Value>) -> FinishReason {
    match value.and_then(Value::as_str) {
        Some("stop") => FinishReason::Stop,
        Some("length") => FinishReason::Length,
        Some("tool_calls") | Some("function_call") => FinishReason::ToolCalls,
        Some("content_filter") => FinishReason::ContentFilter,
        Some("refusal") => FinishReason::Refusal,
        _ => FinishReason::Other,
    }
}

pub fn finish_reason_to(reason: &FinishReason) -> &'static str {
    match reason {
        FinishReason::Stop => "stop",
        FinishReason::Length => "length",
        FinishReason::ToolCalls => "tool_calls",
        FinishReason::ContentFilter => "content_filter",
        FinishReason::Refusal => "refusal",
        FinishReason::Pause | FinishReason::Other => "stop",
    }
}

pub fn decode_usage(value: Option<&Value>) -> Usage {
    let Some(value) = value else {
        return Usage::default();
    };
    let prompt = value
        .get("prompt_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let completion = value
        .get("completion_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cached = value
        .get("prompt_tokens_details")
        .and_then(|details| details.get("cached_tokens"))
        .and_then(Value::as_u64);
    let reasoning = value
        .get("completion_tokens_details")
        .and_then(|details| details.get("reasoning_tokens"))
        .and_then(Value::as_u64);
    Usage {
        input_tokens: prompt,
        output_tokens: completion,
        cache_read_tokens: cached,
        cache_write_tokens: None,
        reasoning_tokens: reasoning,
        cache_read_included_in_input: cached.is_some(),
    }
}

pub fn encode_usage(usage: &Usage) -> Value {
    json!({
        "prompt_tokens": usage.input_tokens,
        "completion_tokens": usage.output_tokens,
        "total_tokens": usage.input_tokens + usage.output_tokens,
    })
}

pub fn encode_response(response: &ChatResponse) -> ProtocolResult<Value> {
    let mut message = Map::new();
    message.insert("role".to_string(), json!("assistant"));

    let text = response.text();
    message.insert(
        "content".to_string(),
        if text.is_empty() {
            Value::Null
        } else {
            Value::String(text)
        },
    );

    let reasoning = response
        .reasoning
        .as_ref()
        .map(|reasoning| reasoning.text.clone())
        .or_else(|| {
            let text = response
                .message
                .assistant
                .iter()
                .filter_map(|block| match block {
                    AssistantContent::Reasoning { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<String>();
            (!text.is_empty()).then_some(text)
        });
    if let Some(reasoning) = reasoning {
        message.insert("reasoning_content".to_string(), json!(reasoning));
    }

    if let Some(refusal) = response
        .message
        .assistant
        .iter()
        .find_map(|block| match block {
            AssistantContent::Refusal { text } => Some(text),
            _ => None,
        })
    {
        message.insert("refusal".to_string(), json!(refusal));
    }

    let mut calls = Vec::new();
    for block in &response.message.assistant {
        match block {
            AssistantContent::ToolCall {
                id,
                name,
                arguments,
                kind: None,
            } => calls.push(json!({
                "id": id,
                "type": "function",
                "function": {"name": name, "arguments": encode_arguments(arguments)},
            })),
            AssistantContent::Raw { kind, raw } if kind == "openai_tool_call" => {
                calls.push(raw.clone())
            }
            _ => {}
        }
    }
    if !calls.is_empty() {
        message.insert("tool_calls".to_string(), Value::Array(calls));
    }
    if let Some(annotations) = response.message.extra.get("annotations") {
        message.insert("annotations".to_string(), annotations.clone());
    }

    let mut choice = Map::new();
    choice.insert("index".to_string(), json!(0));
    choice.insert("message".to_string(), Value::Object(message));
    choice.insert(
        "finish_reason".to_string(),
        json!(finish_reason_to(&response.finish_reason)),
    );
    choice.insert("logprobs".to_string(), Value::Null);

    Ok(json!({
        "id": response.id,
        "object": "chat.completion",
        "created": chrono_seconds(),
        "model": response.model,
        "choices": [Value::Object(choice)],
        "usage": encode_usage(&response.usage),
    }))
}

fn chrono_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

pub fn error_type(kind: octra_llm_core::ErrorKind) -> &'static str {
    use octra_llm_core::ErrorKind;
    match kind {
        ErrorKind::InvalidRequest => "invalid_request_error",
        ErrorKind::Unauthorized => "authentication_error",
        ErrorKind::Forbidden => "permission_error",
        ErrorKind::NotFound => "invalid_request_error",
        ErrorKind::PayloadTooLarge => "invalid_request_error",
        ErrorKind::RateLimited => "rate_limit_error",
        ErrorKind::Upstream => "upstream_error",
        ErrorKind::Config => "upstream_error",
        ErrorKind::Timeout => "timeout_error",
        ErrorKind::Internal => "server_error",
    }
}

pub fn error_body(error: &GatewayError) -> Value {
    // Forward the provider's own error envelope when it was already in this
    // shape, so clients keep seeing the vendor error type.
    if let Some(body) = &error.upstream_body
        && body.get("error").is_some()
    {
        return json!({
            "error": {
                "message": body
                    .get("error")
                    .and_then(|error| error.get("message"))
                    .cloned()
                    .unwrap_or_else(|| json!(error.message)),
                "type": body
                    .get("error")
                    .and_then(|error| error.get("type"))
                    .cloned()
                    .unwrap_or_else(|| json!(error_type(error.kind))),
                "code": body
                    .get("error")
                    .and_then(|error| error.get("code"))
                    .cloned()
                    .unwrap_or(Value::Null),
            }
        });
    }
    json!({
        "error": {
            "message": error.message,
            "type": error_type(error.kind),
            "code": error.code.clone().unwrap_or_else(|| error.kind_code().to_string()),
        }
    })
}

pub fn error_frames(error: &GatewayError) -> Vec<SseFrame> {
    vec![sse_data_frame(&error_body(error))]
}

/// Restores original tool ids on a response body produced upstream.
pub fn restore_response_ids(value: &mut Value, pairs: &[(String, String)]) {
    restore_tool_ids(value, pairs);
}
