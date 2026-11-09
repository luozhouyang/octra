//! Bedrock Converse responses and error envelopes.
//!
//! `decode_response` always reports the synthetic id `"bedrock"` and an empty
//! model because Converse responses carry neither. The data plane overwrites
//! `response.model` with the client-facing model before returning it.
//!
//! `usage.totalTokens` is not stored: the canonical [`Usage`] derives totals
//! from input and output tokens.

use octra_llm_core::{
    AssistantContent, ChatResponse, FinishReason, GatewayError, Map, Message, ProtocolError,
    ProtocolResult, Reasoning, Role, Usage, Value,
};
use serde_json::json;

use crate::SseFrame;
use crate::common::sse_data_frame;

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
        .get("output")
        .and_then(|output| output.get("message"))
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)
        .ok_or_else(|| ProtocolError::new("response has no output.message.content array"))?;

    let mut assistant = Vec::new();
    let mut reasoning: Option<Reasoning> = None;
    for block in content {
        if let Some(text) = block.get("text").and_then(Value::as_str) {
            assistant.push(AssistantContent::Text {
                text: text.to_string(),
            });
            continue;
        }
        if let Some(tool_use) = block.get("toolUse") {
            assistant.push(AssistantContent::ToolCall {
                id: tool_use
                    .get("toolUseId")
                    .and_then(Value::as_str)
                    .unwrap_or("tooluse_missing")
                    .to_string(),
                name: tool_use
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                arguments: tool_use.get("input").cloned().unwrap_or_else(|| json!({})),
                kind: None,
            });
            continue;
        }
        if let Some(reasoning_text) = block
            .get("reasoningContent")
            .and_then(|content| content.get("reasoningText"))
        {
            let text = reasoning_text
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let signature = reasoning_text
                .get("signature")
                .and_then(Value::as_str)
                .map(str::to_string);
            assistant.push(AssistantContent::Reasoning {
                text: text.clone(),
                signature: signature.clone(),
            });
            let summary = reasoning.get_or_insert_with(|| Reasoning {
                text: String::new(),
                signature: None,
                encrypted: Vec::new(),
            });
            summary.text.push_str(&text);
            if summary.signature.is_none() {
                summary.signature = signature;
            }
            continue;
        }
        assistant.push(AssistantContent::Raw {
            kind: "bedrock_content_block".to_string(),
            raw: block.clone(),
        });
    }

    Ok(ChatResponse {
        id: "bedrock".to_string(),
        model: String::new(),
        message: Message {
            role: Role::Assistant,
            content: None,
            assistant,
            tool_results: Vec::new(),
            name: None,
            extra: Map::new(),
        },
        finish_reason: finish_reason_from(object.get("stopReason").and_then(Value::as_str)),
        usage: decode_usage(object.get("usage")),
        reasoning,
        programmatic: None,
        extra: Map::new(),
    })
}

/// Maps a Converse `stopReason` onto the canonical finish reason.
pub fn finish_reason_from(stop_reason: Option<&str>) -> FinishReason {
    match stop_reason {
        Some("end_turn") | Some("stop_sequence") => FinishReason::Stop,
        Some("max_tokens") => FinishReason::Length,
        Some("tool_use") => FinishReason::ToolCalls,
        Some("content_filtered") | Some("guardrail_intervened") => FinishReason::ContentFilter,
        _ => FinishReason::Stop,
    }
}

/// Reverse mapping used by `encode_response`.
pub fn stop_reason_to(reason: &FinishReason) -> &'static str {
    match reason {
        FinishReason::Length => "max_tokens",
        FinishReason::ToolCalls => "tool_use",
        FinishReason::ContentFilter => "content_filtered",
        FinishReason::Refusal => "guardrail_intervened",
        FinishReason::Stop | FinishReason::Pause | FinishReason::Other => "end_turn",
    }
}

pub fn decode_usage(value: Option<&Value>) -> Usage {
    let Some(value) = value else {
        return Usage::default();
    };
    Usage {
        input_tokens: value
            .get("inputTokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        output_tokens: value
            .get("outputTokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cache_read_tokens: value.get("cacheReadInputTokens").and_then(Value::as_u64),
        cache_write_tokens: value.get("cacheWriteInputTokens").and_then(Value::as_u64),
        reasoning_tokens: None,
        cache_read_included_in_input: false,
    }
}

pub fn encode_usage(usage: &Usage) -> Value {
    let mut object = Map::new();
    object.insert("inputTokens".to_string(), json!(usage.input_tokens));
    object.insert("outputTokens".to_string(), json!(usage.output_tokens));
    object.insert(
        "totalTokens".to_string(),
        json!(usage.input_tokens.saturating_add(usage.output_tokens)),
    );
    if let Some(read) = usage.cache_read_tokens {
        object.insert("cacheReadInputTokens".to_string(), json!(read));
    }
    if let Some(write) = usage.cache_write_tokens {
        object.insert("cacheWriteInputTokens".to_string(), json!(write));
    }
    Value::Object(object)
}

pub fn encode_response(response: &ChatResponse) -> ProtocolResult<Value> {
    let mut content = Vec::new();
    for block in &response.message.assistant {
        match block {
            AssistantContent::Text { text } => content.push(json!({"text": text})),
            AssistantContent::Reasoning { text, signature } => {
                let mut reasoning_text = Map::new();
                reasoning_text.insert("text".to_string(), json!(text));
                if let Some(signature) = signature {
                    reasoning_text.insert("signature".to_string(), json!(signature));
                }
                content.push(json!({
                    "reasoningContent": {
                        "reasoningText": Value::Object(reasoning_text),
                    }
                }));
            }
            AssistantContent::ToolCall {
                id,
                name,
                arguments,
                ..
            } => content.push(json!({
                "toolUse": {
                    "toolUseId": id,
                    "name": name,
                    "input": arguments,
                }
            })),
            AssistantContent::ToolResult {
                tool_use_id,
                content: result,
                is_error,
            } => content.push(json!({
                "toolResult": {
                    "toolUseId": tool_use_id,
                    "content": [{"json": result}],
                    "status": if *is_error { "error" } else { "success" },
                }
            })),
            AssistantContent::Refusal { text } => content.push(json!({"text": text})),
            AssistantContent::Raw { raw, .. } => content.push(raw.clone()),
        }
    }

    Ok(json!({
        "output": {
            "message": {
                "role": "assistant",
                "content": content,
            }
        },
        "stopReason": stop_reason_to(&response.finish_reason),
        "usage": encode_usage(&response.usage),
    }))
}

pub fn error_body(error: &GatewayError) -> Value {
    json!({
        "message": error.message,
        "__type": error.kind_code(),
    })
}

pub fn error_frames(error: &GatewayError) -> Vec<SseFrame> {
    vec![sse_data_frame(&error_body(error))]
}

#[cfg(test)]
mod tests {
    use super::*;
    use octra_llm_core::AssistantContent;
    use serde_json::json;

    #[test]
    fn decodes_text_tool_use_reasoning_and_usage() {
        let body = json!({
            "output": {
                "message": {
                    "role": "assistant",
                    "content": [
                        {"text": "let me check"},
                        {
                            "toolUse": {
                                "toolUseId": "tooluse_1",
                                "name": "lookup",
                                "input": {"q": "x"},
                            }
                        },
                        {
                            "reasoningContent": {
                                "reasoningText": {
                                    "text": "thinking",
                                    "signature": "sig-1",
                                }
                            }
                        }
                    ],
                }
            },
            "stopReason": "tool_use",
            "usage": {
                "inputTokens": 10,
                "outputTokens": 4,
                "totalTokens": 14,
                "cacheReadInputTokens": 2,
                "cacheWriteInputTokens": 1,
            },
        });
        let response = decode_response(body).unwrap();
        assert_eq!(response.id, "bedrock");
        assert_eq!(response.model, "");
        assert_eq!(response.finish_reason, FinishReason::ToolCalls);
        assert_eq!(
            response.message.assistant,
            vec![
                AssistantContent::Text {
                    text: "let me check".to_string(),
                },
                AssistantContent::ToolCall {
                    id: "tooluse_1".to_string(),
                    name: "lookup".to_string(),
                    arguments: json!({"q": "x"}),
                    kind: None,
                },
                AssistantContent::Reasoning {
                    text: "thinking".to_string(),
                    signature: Some("sig-1".to_string()),
                },
            ]
        );
        assert_eq!(response.usage.input_tokens, 10);
        assert_eq!(response.usage.output_tokens, 4);
        assert_eq!(response.usage.cache_read_tokens, Some(2));
        assert_eq!(response.usage.cache_write_tokens, Some(1));
        let reasoning = response.reasoning.expect("reasoning summary");
        assert_eq!(reasoning.text, "thinking");
        assert_eq!(reasoning.signature.as_deref(), Some("sig-1"));
    }

    #[test]
    fn maps_every_documented_stop_reason() {
        for (stop, expected) in [
            ("end_turn", FinishReason::Stop),
            ("stop_sequence", FinishReason::Stop),
            ("max_tokens", FinishReason::Length),
            ("tool_use", FinishReason::ToolCalls),
            ("content_filtered", FinishReason::ContentFilter),
            ("guardrail_intervened", FinishReason::ContentFilter),
            ("something_new", FinishReason::Stop),
        ] {
            assert_eq!(finish_reason_from(Some(stop)), expected, "{stop}");
        }
        assert_eq!(finish_reason_from(None), FinishReason::Stop);
    }

    #[test]
    fn response_round_trips_through_encode() {
        let original = json!({
            "output": {
                "message": {
                    "role": "assistant",
                    "content": [
                        {"text": "hello"},
                        {"toolUse": {"toolUseId": "tooluse_1", "name": "f", "input": {}}}
                    ],
                }
            },
            "stopReason": "tool_use",
            "usage": {"inputTokens": 1, "outputTokens": 2, "totalTokens": 3},
        });
        let decoded = decode_response(original).unwrap();
        let encoded = encode_response(&decoded).unwrap();
        let again = decode_response(encoded).unwrap();
        assert_eq!(again.finish_reason, FinishReason::ToolCalls);
        assert_eq!(again.message.assistant, decoded.message.assistant);
        assert_eq!(again.usage, decoded.usage);
    }
}
