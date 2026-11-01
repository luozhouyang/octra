//! Helpers shared by the protocol adapters: SSE frame formatting, JSON
//! parsing, tool-id normalization and attachment decoding.

use std::collections::BTreeSet;

use octra_llm_core::{Attachment, AttachmentData, ContentPart, GatewayError, ProtocolResult, Role};
use serde_json::{Map, Value};

/// One Server-Sent Events frame ready to be written to the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseFrame {
    /// `event:` name, `None` for OpenAI-style data-only frames.
    pub event: Option<String>,
    pub data: String,
}

impl SseFrame {
    pub fn data(data: impl Into<String>) -> Self {
        Self {
            event: None,
            data: data.into(),
        }
    }

    pub fn event(event: impl Into<String>, data: impl Into<String>) -> Self {
        Self {
            event: Some(event.into()),
            data: data.into(),
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = String::new();
        if let Some(event) = &self.event {
            out.push_str("event: ");
            out.push_str(event);
            out.push('\n');
        }
        for line in self.data.split('\n') {
            out.push_str("data: ");
            out.push_str(line);
            out.push('\n');
        }
        out.push('\n');
        out.into_bytes()
    }

    /// Wire representation, useful in assertions.
    pub fn render(&self) -> String {
        String::from_utf8(self.to_bytes()).unwrap_or_default()
    }
}

pub fn sse_data_frame(value: &Value) -> SseFrame {
    SseFrame::data(value.to_string())
}

pub fn sse_event_frame(event: &str, value: &Value) -> SseFrame {
    SseFrame::event(event, value.to_string())
}

/// Parses SSE data. `[DONE]` (OpenAI stream terminator) yields `None`.
pub fn parse_sse_json(data: &str) -> ProtocolResult<Option<Value>> {
    let trimmed = data.trim();
    if trimmed.is_empty() || trimmed == "[DONE]" {
        return Ok(None);
    }
    Ok(Some(serde_json::from_str(trimmed)?))
}

/// Wire protocol facts needed by the transport layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamProtocol {
    OpenAi,
    Anthropic,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamCodec {
    pub protocol: UpstreamProtocol,
    pub accept: &'static str,
}

impl StreamCodec {
    pub const OPENAI: StreamCodec = StreamCodec {
        protocol: UpstreamProtocol::OpenAi,
        accept: "text/event-stream",
    };

    pub const ANTHROPIC: StreamCodec = StreamCodec {
        protocol: UpstreamProtocol::Anthropic,
        accept: "text/event-stream",
    };
}

/// LM Studio and other local servers historically rejected OpenAI ids with
/// characters outside `[a-zA-Z0-9_-]`; local Anthropic-compatible servers are
/// similarly picky. Tool ids are rewritten deterministically for the upstream
/// and restored (`restore_tool_ids`) before returning to the client.
pub fn normalize_tool_id(id: &str) -> String {
    let mut out = String::with_capacity(id.len());
    for ch in id.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "call".to_string()
    } else {
        out
    }
}

/// Rewrites every tool id in `value` to its normalized form and returns the
/// mapping used to restore originals in the response.
pub fn apply_tool_id_mapping(value: &mut Value) -> Vec<(String, String)> {
    let mut seen = BTreeSet::new();
    let mut pairs = Vec::new();
    visit_tool_ids(value, &mut |id: &str| {
        let normalized = normalize_tool_id(id);
        if normalized != id && seen.insert(id.to_string()) {
            pairs.push((normalized.clone(), id.to_string()));
        }
        normalized
    });
    pairs
}

pub fn restore_tool_ids(value: &mut Value, pairs: &[(String, String)]) {
    if pairs.is_empty() {
        return;
    }
    visit_tool_ids(value, &mut |id: &str| {
        pairs
            .iter()
            .find(|(normalized, _)| normalized == id)
            .map(|(_, original)| original.clone())
            .unwrap_or_else(|| id.to_string())
    });
}

fn visit_tool_ids(value: &mut Value, mapper: &mut dyn FnMut(&str) -> String) {
    match value {
        Value::Object(map) => {
            for key in ["tool_call_id", "tool_use_id"] {
                if let Some(Value::String(id)) = map.get_mut(key) {
                    let mapped = mapper(id);
                    *id = mapped;
                }
            }
            let block_type = map.get("type").and_then(Value::as_str);
            if (matches!(block_type, Some("tool_use") | Some("server_tool_use"))
                || matches!(block_type, Some("tool_call") | Some("tool_result")))
                && let Some(Value::String(id)) = map.get_mut("id")
            {
                let mapped = mapper(id);
                *id = mapped;
            }
            for child in map.values_mut() {
                visit_tool_ids(child, mapper);
            }
        }
        Value::Array(items) => {
            for item in items {
                visit_tool_ids(item, mapper);
            }
        }
        _ => {}
    }
}

/// Shared attachment decoding from vendor content blocks.
pub fn decode_attachment(block: &Value, kind: &str) -> Attachment {
    let mut extra = Map::new();
    for key in ["cache_control", "title", "citations", "annotations"] {
        if let Some(value) = block.get(key) {
            extra.insert(key.to_string(), value.clone());
        }
    }
    let source = block.get("source").cloned().unwrap_or(Value::Null);
    match kind {
        "image" => {
            let media_type = source
                .get("media_type")
                .and_then(Value::as_str)
                .unwrap_or("image/png")
                .to_string();
            match source.get("type").and_then(Value::as_str) {
                Some("base64") => Attachment {
                    media_type,
                    data: source.get("data").and_then(Value::as_str).map(|data| {
                        AttachmentData::Base64 {
                            data: data.to_string(),
                        }
                    }),
                    url: None,
                    name: block
                        .get("image_url")
                        .and_then(|v| v.get("url"))
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    extra,
                },
                Some("url") => {
                    let url = source
                        .get("url")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    Attachment {
                        media_type,
                        data: None,
                        url,
                        name: None,
                        extra,
                    }
                }
                _ => Attachment {
                    media_type,
                    data: None,
                    url: source
                        .get("url")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    name: None,
                    extra,
                },
            }
        }
        _ => Attachment {
            media_type: block
                .get("media_type")
                .and_then(Value::as_str)
                .or_else(|| source.get("media_type").and_then(Value::as_str))
                .unwrap_or("application/octet-stream")
                .to_string(),
            data: source
                .get("data")
                .and_then(Value::as_str)
                .map(|data| AttachmentData::Base64 {
                    data: data.to_string(),
                }),
            url: source
                .get("url")
                .and_then(Value::as_str)
                .map(str::to_string),
            name: block
                .get("filename")
                .or_else(|| block.get("title"))
                .and_then(Value::as_str)
                .map(str::to_string),
            extra,
        },
    }
}

/// Collects text parts from content blocks that can be flattened to text.
pub fn flatten_text_parts(parts: &[ContentPart]) -> Option<String> {
    let mut out = String::new();
    for part in parts {
        if let ContentPart::Text { text, .. } = part {
            out.push_str(text);
        }
    }
    Some(out)
}

/// Canonical role to the protocol-independent string used in usage logs.
pub fn role_name(role: Role) -> &'static str {
    role.as_str()
}

/// The terminal `message_stop` / `[DONE]` frames share this shape check.
pub fn is_done_frame(data: &str) -> bool {
    data.trim() == "[DONE]"
}

/// Builds an upstream error from a parsed error body.
pub fn upstream_error_from_body(status: u16, body: Option<&Value>) -> GatewayError {
    let message = body
        .and_then(|body| body.get("error"))
        .and_then(|error| {
            error
                .get("message")
                .and_then(Value::as_str)
                .or_else(|| error.as_str())
        })
        .or_else(|| {
            body.and_then(|body| body.get("message"))
                .and_then(Value::as_str)
        })
        .map(str::to_string)
        .unwrap_or_else(|| format!("upstream returned HTTP {status}"));
    let mut err = GatewayError::upstream(status.clamp(400, 599), message);
    err.upstream_status = Some(status);
    err.upstream_body = body.cloned();
    err
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn frames_render_with_data_prefix() {
        let frame = SseFrame::event("content_block_delta", r#"{"a":1}"#);
        assert_eq!(
            frame.render(),
            "event: content_block_delta\ndata: {\"a\":1}\n\n"
        );
        assert_eq!(SseFrame::data("x").render(), "data: x\n\n");
    }

    #[test]
    fn parse_sse_json_handles_done() {
        assert!(parse_sse_json("[DONE]").unwrap().is_none());
        assert!(parse_sse_json("  ").unwrap().is_none());
        assert_eq!(
            parse_sse_json(r#"{"x":1}"#).unwrap().unwrap(),
            json!({"x": 1})
        );
        assert!(parse_sse_json("{oops").is_err());
    }

    #[test]
    fn tool_id_normalization_round_trips() {
        let mut value = json!({
            "messages": [
                {"role": "assistant", "tool_calls": [{"id": "call:a/b", "type": "function"}]},
                {"role": "tool", "tool_call_id": "call:a/b", "content": "x"}
            ]
        });
        let pairs = apply_tool_id_mapping(&mut value);
        assert_eq!(value["messages"][1]["tool_call_id"], "call_a_b");
        restore_tool_ids(&mut value, &pairs);
        assert_eq!(value["messages"][1]["tool_call_id"], "call:a/b");
        assert_eq!(value["messages"][0]["tool_calls"][0]["id"], "call:a/b");
    }

    #[test]
    fn opaque_tool_call_blocks_get_ids_rewritten() {
        let mut value = json!([{
            "type": "tool_call",
            "id": "abc.1",
            "name": "x"
        }]);
        let pairs = apply_tool_id_mapping(&mut value);
        assert_eq!(value[0]["id"], "abc_1");
        assert_eq!(pairs, vec![("abc_1".to_string(), "abc.1".to_string())]);
    }

    #[test]
    fn upstream_error_extracts_message() {
        let body = json!({"error": {"message": "bad key", "type": "auth"}});
        let err = upstream_error_from_body(401, Some(&body));
        assert_eq!(err.message, "bad key");
        assert_eq!(err.status, 401);
        let err = upstream_error_from_body(503, None);
        assert_eq!(err.status, 503);
        assert!(err.message.contains("503"));
    }
}
