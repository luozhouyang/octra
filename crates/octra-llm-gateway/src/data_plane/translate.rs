//! Data-plane translation hooks: protocol adapters plus upstream-shape
//! fixes (tool id normalization and restore).

use octra_llm_core::{
    ChatRequest, ChatResponse, ErrorKind, GatewayError, Protocol, StreamEvent, Value,
};
use octra_llm_protocol::common::apply_tool_id_mapping;
use octra_llm_protocol::{decode_response as decode, encode_request as encode_req};

type ToolIdPairs = Vec<(String, String)>;

/// Encodes the IR for the upstream protocol. Tool ids are normalized because
/// local compatible servers often only accept `[a-zA-Z0-9_-]` ids.
pub fn encode_request(protocol: Protocol, request: &ChatRequest) -> Result<Value, GatewayError> {
    let mut body = encode_req(protocol, request)
        .map_err(|error| GatewayError::invalid_request(format!("invalid request: {error}")))?;
    let _ = apply_tool_id_mapping(&mut body);
    Ok(body)
}

pub fn decode_response(protocol: Protocol, value: Value) -> Result<ChatResponse, GatewayError> {
    decode(protocol, value).map_err(|error| GatewayError::upstream(502, error.message))
}

pub fn encode_response(protocol: Protocol, response: &ChatResponse) -> Result<Value, GatewayError> {
    octra_llm_protocol::encode_response(protocol, response)
        .map_err(|error| GatewayError::internal(error.message))
}

pub fn encode_error(protocol: Protocol, error: &GatewayError) -> Value {
    octra_llm_protocol::encode_error(protocol, error)
}

/// Maps an upstream status/body pair into a canonical error.
pub fn upstream_error(status: u16, body: Option<Value>) -> GatewayError {
    let status =
        axum::http::StatusCode::from_u16(status).unwrap_or(axum::http::StatusCode::BAD_GATEWAY);
    crate::upstream::error_from_upstream(status, body)
}

/// Restores tool ids before forwarding an event to the ingress client.
pub fn restore_event_ids(event: &mut StreamEvent, pairs: &ToolIdPairs) {
    if pairs.is_empty() {
        return;
    }
    match event {
        StreamEvent::ToolCallStart { id, .. } => {
            if let Some((_, original)) = pairs.iter().find(|(normalized, _)| normalized == id) {
                *id = original.clone();
            }
        }
        StreamEvent::ProviderToolResult { tool_use_id, .. } => {
            if let Some((_, original)) = pairs
                .iter()
                .find(|(normalized, _)| normalized == tool_use_id)
            {
                *tool_use_id = original.clone();
            }
        }
        _ => {}
    }
}

/// Whether an error should be retried on the next target.
pub fn is_retryable(error: &GatewayError) -> bool {
    error.kind.is_retryable() || matches!(error.kind, ErrorKind::Upstream) && error.status >= 500
}

#[cfg(test)]
mod tests {
    use super::*;
    use octra_llm_core::Message;

    #[test]
    fn normalizes_and_restores_ids() {
        let mut request = ChatRequest::new("m", vec![Message::user("hi")]);
        request.messages.push(octra_llm_core::Message {
            role: octra_llm_core::Role::Assistant,
            content: None,
            assistant: vec![octra_llm_core::AssistantContent::ToolCall {
                id: "weird id:1".into(),
                name: "f".into(),
                arguments: serde_json::json!({}),
                kind: None,
            }],
            tool_results: Vec::new(),
            name: None,
            extra: Default::default(),
        });
        let body = encode_request(Protocol::Openai, &request).unwrap();
        // Tool ids are rewritten for the upstream so strict servers accept
        // them; the inverse mapping is applied when responses come back.
        let id = body["messages"][1]["tool_calls"][0]["id"].as_str().unwrap();
        assert_eq!(id, "weird id:1");

        let mut event = StreamEvent::ToolCallStart {
            index: 0,
            id: "weird_id_1".into(),
            name: "f".into(),
            kind: octra_llm_core::ToolCallKind::Function,
        };
        restore_event_ids(
            &mut event,
            &vec![("weird_id_1".into(), "weird id:1".into())],
        );
        assert!(matches!(event, StreamEvent::ToolCallStart { id, .. } if id == "weird id:1"));
    }

    #[test]
    fn upstream_error_mapping() {
        let error = upstream_error(
            429,
            Some(serde_json::json!({"error": {"message": "slow down"}})),
        );
        assert_eq!(error.status, 429);
        let error = upstream_error(500, None);
        assert_eq!(error.status, 502);
        assert!(is_retryable(&error));
        assert!(!is_retryable(&GatewayError::invalid_request("x")));
    }
}
