//! Gemini generateContent responses and error envelopes.

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
    check_blocked(object)?;

    let candidate = object
        .get("candidates")
        .and_then(Value::as_array)
        .and_then(|candidates| candidates.first())
        .ok_or_else(|| ProtocolError::new("response has no candidates"))?;
    let parts = candidate
        .get("content")
        .and_then(|content| content.get("parts"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut assistant = Vec::new();
    let mut has_tool_call = false;
    for (index, part) in parts.iter().enumerate() {
        if let Some(call) = part.get("functionCall") {
            has_tool_call = true;
            let name = call
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            assistant.push(AssistantContent::ToolCall {
                id: format!("call_{name}_{index}"),
                name,
                arguments: call.get("args").cloned().unwrap_or_else(|| json!({})),
                kind: None,
            });
            continue;
        }
        if let Some(text) = part.get("text").and_then(Value::as_str) {
            if part
                .get("thought")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                assistant.push(AssistantContent::Reasoning {
                    text: text.to_string(),
                    signature: None,
                });
            } else {
                assistant.push(AssistantContent::Text {
                    text: text.to_string(),
                });
            }
        }
    }

    let finish_reason = match candidate.get("finishReason").and_then(Value::as_str) {
        Some("MAX_TOKENS") => FinishReason::Length,
        Some("SAFETY") | Some("RECITATION") | Some("BLOCKLIST") | Some("PROHIBITED_CONTENT") => {
            FinishReason::ContentFilter
        }
        Some("STOP") if has_tool_call => FinishReason::ToolCalls,
        Some("STOP") | None if has_tool_call => FinishReason::ToolCalls,
        Some("STOP") | None => FinishReason::Stop,
        Some(_) => FinishReason::Other,
    };

    let mut extra = Map::new();
    if let Some(ratings) = candidate.get("safetyRatings") {
        extra.insert("safetyRatings".to_string(), ratings.clone());
    }
    if let Some(citation) = candidate.get("citationMetadata") {
        extra.insert("citationMetadata".to_string(), citation.clone());
    }

    Ok(ChatResponse {
        id: format!("gemini-{}", chrono_seconds()),
        model: object
            .get("modelVersion")
            .and_then(Value::as_str)
            .unwrap_or("gemini")
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
        usage: decode_usage(object.get("usageMetadata")),
        reasoning: None,
        programmatic: None,
        extra,
    })
}

pub fn check_blocked(object: &Map<String, Value>) -> ProtocolResult<()> {
    if let Some(reason) = object
        .get("promptFeedback")
        .and_then(|feedback| feedback.get("blockReason"))
        .and_then(Value::as_str)
    {
        return Err(ProtocolError::new(format!(
            "prompt blocked by Gemini safety settings: {reason}"
        )));
    }
    Ok(())
}

pub fn decode_usage(value: Option<&Value>) -> Usage {
    let Some(value) = value else {
        return Usage::default();
    };
    Usage {
        input_tokens: value
            .get("promptTokenCount")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        output_tokens: value
            .get("candidatesTokenCount")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cache_read_tokens: value.get("cachedContentTokenCount").and_then(Value::as_u64),
        cache_write_tokens: None,
        reasoning_tokens: value.get("thoughtsTokenCount").and_then(Value::as_u64),
    }
}

pub fn encode_usage(usage: &Usage) -> Value {
    let mut object = json!({
        "promptTokenCount": usage.input_tokens,
        "candidatesTokenCount": usage.output_tokens,
        "totalTokenCount": usage.input_tokens + usage.output_tokens,
    });
    if let Some(cached) = usage.cache_read_tokens {
        object["cachedContentTokenCount"] = json!(cached);
    }
    if let Some(reasoning) = usage.reasoning_tokens {
        object["thoughtsTokenCount"] = json!(reasoning);
    }
    object
}

pub fn encode_response(response: &ChatResponse) -> ProtocolResult<Value> {
    let mut parts = Vec::new();
    for block in &response.message.assistant {
        match block {
            AssistantContent::Text { text } => parts.push(json!({"text": text})),
            AssistantContent::Reasoning { text, .. } => {
                parts.push(json!({"text": text, "thought": true}))
            }
            AssistantContent::ToolCall {
                name, arguments, ..
            } => parts.push(json!({
                "functionCall": {"name": name, "args": arguments},
            })),
            _ => {}
        }
    }
    let finish_reason = match response.finish_reason {
        FinishReason::Length => "MAX_TOKENS",
        FinishReason::ContentFilter | FinishReason::Refusal => "SAFETY",
        FinishReason::Stop
        | FinishReason::ToolCalls
        | FinishReason::Pause
        | FinishReason::Other => "STOP",
    };
    Ok(json!({
        "candidates": [{
            "content": {"role": "model", "parts": parts},
            "finishReason": finish_reason,
            "index": 0,
        }],
        "usageMetadata": encode_usage(&response.usage),
        "modelVersion": response.model,
    }))
}

fn chrono_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

pub fn finish_reason_from(value: Option<&Value>) -> FinishReason {
    match value.and_then(Value::as_str) {
        Some("MAX_TOKENS") => FinishReason::Length,
        Some("SAFETY") | Some("RECITATION") | Some("BLOCKLIST") | Some("PROHIBITED_CONTENT") => {
            FinishReason::ContentFilter
        }
        Some("STOP") => FinishReason::Stop,
        _ => FinishReason::Other,
    }
}

pub fn finish_reason_to(reason: &FinishReason) -> &'static str {
    match reason {
        FinishReason::Length => "MAX_TOKENS",
        FinishReason::ContentFilter | FinishReason::Refusal => "SAFETY",
        _ => "STOP",
    }
}

pub fn error_body(error: &GatewayError) -> Value {
    json!({
        "error": {
            "code": error.status,
            "message": error.message,
            "status": gemini_status(error.status),
        }
    })
}

fn gemini_status(status: u16) -> &'static str {
    match status {
        400 => "INVALID_ARGUMENT",
        401 => "UNAUTHENTICATED",
        403 => "PERMISSION_DENIED",
        404 => "NOT_FOUND",
        429 => "RESOURCE_EXHAUSTED",
        500 => "INTERNAL",
        503 => "UNAVAILABLE",
        504 => "DEADLINE_EXCEEDED",
        _ => "UNKNOWN",
    }
}

pub fn error_frames(error: &GatewayError) -> Vec<SseFrame> {
    vec![SseFrame::data(error_body(error).to_string())]
}
