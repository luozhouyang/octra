//! OpenAI Responses API responses and error envelopes.

use octra_llm_core::{
    AssistantContent, ChatResponse, FinishReason, GatewayError, Map, Message, ProtocolError,
    ProtocolResult, Role, Usage, Value,
};
use serde_json::json;

use crate::common::SseFrame;

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
        .unwrap_or("resp_unknown")
        .to_string();
    let model = object
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let output = object
        .get("output")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut assistant = Vec::new();
    let mut has_tool_call = false;
    for item in &output {
        match item.get("type").and_then(Value::as_str).unwrap_or("") {
            "message" => {
                if let Some(content) = item.get("content").and_then(Value::as_array) {
                    for part in content {
                        match part.get("type").and_then(Value::as_str).unwrap_or("") {
                            "output_text" => {
                                let text =
                                    part.get("text").and_then(Value::as_str).unwrap_or_default();
                                if !text.is_empty() {
                                    assistant.push(AssistantContent::Text {
                                        text: text.to_string(),
                                    });
                                }
                            }
                            "refusal" => {
                                let text = part
                                    .get("refusal")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default();
                                assistant.push(AssistantContent::Refusal {
                                    text: text.to_string(),
                                });
                            }
                            _ => {}
                        }
                    }
                }
            }
            "reasoning" => {
                let text = item
                    .get("summary")
                    .and_then(Value::as_array)
                    .map(|summary| {
                        summary
                            .iter()
                            .filter_map(|part| part.get("text").and_then(Value::as_str))
                            .collect::<Vec<_>>()
                            .join("")
                    })
                    .unwrap_or_default();
                if !text.is_empty() {
                    assistant.push(AssistantContent::Reasoning {
                        text,
                        signature: item
                            .get("encrypted_content")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                    });
                }
            }
            "function_call" => {
                has_tool_call = true;
                let arguments = item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .and_then(|text| serde_json::from_str::<Value>(text).ok())
                    .unwrap_or_else(|| json!({}));
                assistant.push(AssistantContent::ToolCall {
                    id: item
                        .get("call_id")
                        .or_else(|| item.get("id"))
                        .and_then(Value::as_str)
                        .unwrap_or("call_missing")
                        .to_string(),
                    name: item
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    arguments,
                    kind: None,
                });
            }
            other => assistant.push(AssistantContent::Raw {
                kind: format!("openai_responses_{other}"),
                raw: item.clone(),
            }),
        }
    }

    let status = object.get("status").and_then(Value::as_str);
    let finish_reason = match status {
        Some("incomplete") => match object
            .get("incomplete_details")
            .and_then(|details| details.get("reason"))
            .and_then(Value::as_str)
        {
            Some("content_filter") => FinishReason::ContentFilter,
            _ => FinishReason::Length,
        },
        Some("failed") => FinishReason::Other,
        _ if has_tool_call => FinishReason::ToolCalls,
        _ => FinishReason::Stop,
    };

    let mut extra = Map::new();
    for key in ["previous_response_id", "status", "metadata"] {
        if let Some(value) = object.get(key) {
            extra.insert(key.to_string(), value.clone());
        }
    }

    Ok(ChatResponse {
        id,
        model,
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
        reasoning: None,
        programmatic: None,
        extra,
    })
}

pub fn decode_usage(value: Option<&Value>) -> Usage {
    let Some(value) = value else {
        return Usage::default();
    };
    let input = value
        .get("input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = value
        .get("output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cached = value
        .get("input_tokens_details")
        .and_then(|details| details.get("cached_tokens"))
        .and_then(Value::as_u64);
    let reasoning = value
        .get("output_tokens_details")
        .and_then(|details| details.get("reasoning_tokens"))
        .and_then(Value::as_u64);
    Usage {
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: cached,
        cache_write_tokens: None,
        reasoning_tokens: reasoning,
        cache_read_included_in_input: true,
    }
}

pub fn encode_usage(usage: &Usage) -> Value {
    json!({
        "input_tokens": usage.input_tokens,
        "output_tokens": usage.output_tokens,
        "total_tokens": usage.input_tokens + usage.output_tokens,
        "input_tokens_details": {"cached_tokens": usage.cache_read_tokens.unwrap_or(0)},
        "output_tokens_details": {"reasoning_tokens": usage.reasoning_tokens.unwrap_or(0)},
    })
}

pub fn encode_response(response: &ChatResponse) -> ProtocolResult<Value> {
    let mut output = Vec::new();
    let mut text = String::new();
    let mut message_content = Vec::new();
    for block in &response.message.assistant {
        match block {
            AssistantContent::Text { text: chunk } => {
                text.push_str(chunk);
                message_content.push(json!({"type": "output_text", "text": chunk}));
            }
            AssistantContent::Refusal { text: refusal } => {
                message_content.push(json!({"type": "refusal", "refusal": refusal}));
            }
            AssistantContent::Reasoning { text, .. } => output.push(json!({
                "type": "reasoning",
                "id": format!("rs_{}", response.id),
                "summary": [{"type": "summary_text", "text": text}],
            })),
            AssistantContent::ToolCall {
                id,
                name,
                arguments,
                ..
            } => output.push(json!({
                "type": "function_call",
                "id": format!("fc_{id}"),
                "call_id": id,
                "name": name,
                "arguments": arguments.to_string(),
                "status": "completed",
            })),
            AssistantContent::Raw { kind, raw } if kind.starts_with("openai_responses") => {
                output.push(raw.clone())
            }
            _ => {}
        }
    }
    if !message_content.is_empty() {
        output.push(json!({
            "type": "message",
            "id": format!("msg_{}", response.id),
            "role": "assistant",
            "status": "completed",
            "content": message_content,
        }));
    }

    let status = match response.finish_reason {
        FinishReason::Length | FinishReason::ContentFilter => "incomplete",
        _ => "completed",
    };
    let mut object = json!({
        "id": response.id,
        "object": "response",
        "created_at": chrono_seconds(),
        "model": response.model,
        "status": status,
        "output": output,
        "usage": encode_usage(&response.usage),
    });
    if let Some(previous) = response.extra.get("previous_response_id") {
        object["previous_response_id"] = previous.clone();
    }
    if response.finish_reason == FinishReason::Length {
        object["incomplete_details"] = json!({"reason": "max_output_tokens"});
    } else if response.finish_reason == FinishReason::ContentFilter {
        object["incomplete_details"] = json!({"reason": "content_filter"});
    }
    object["output_text"] = json!(text);
    Ok(object)
}

fn chrono_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

pub fn error_body(error: &GatewayError) -> Value {
    json!({
        "error": {
            "message": error.message,
            "type": crate::openai::response::error_type(error.kind),
            "code": error.code.clone().unwrap_or_else(|| error.kind_code().to_string()),
        }
    })
}

pub fn error_frames(error: &GatewayError) -> Vec<SseFrame> {
    vec![SseFrame::event("error", error_body(error).to_string())]
}

pub fn response_object(id: &str, model: &str, status: &str) -> Value {
    json!({
        "id": id,
        "object": "response",
        "created_at": chrono_seconds(),
        "model": model,
        "status": status,
        "output": [],
    })
}

pub fn data_event(event: &str, value: Value) -> SseFrame {
    let mut value = value;
    if let Some(object) = value.as_object_mut() {
        object
            .entry("type".to_string())
            .or_insert_with(|| json!(event));
    }
    SseFrame::event(event, value.to_string())
}
