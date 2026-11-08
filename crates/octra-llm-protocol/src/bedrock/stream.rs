//! Bedrock `converse-stream` event decoding.
//!
//! The gateway feeds one AWS event-stream frame at a time as
//! `"event-type\njson-payload"`, matching the convention shared by the other
//! protocol decoders. The encoder is a no-op because Bedrock is not an ingress
//! protocol; it exists only to satisfy the dispatcher.

use octra_llm_core::{
    FinishReason, GatewayError, ProtocolResult, StreamEvent, ToolCallKind, Value,
};

use super::response::{decode_usage, finish_reason_from};
use crate::common::parse_sse_json;
use crate::{SseFrame, StreamDecoder, StreamEncoder};

#[derive(Debug, Default)]
pub struct Decoder {
    started: bool,
    finished: bool,
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    fn ensure_start(&mut self, out: &mut Vec<StreamEvent>) {
        if !self.started {
            self.started = true;
            out.push(StreamEvent::MessageStart {
                id: "bedrock".to_string(),
                model: String::new(),
            });
        }
    }
}

impl StreamDecoder for Decoder {
    fn decode_chunk(&mut self, data: &str) -> ProtocolResult<Vec<StreamEvent>> {
        let mut out = Vec::new();
        let (event, payload) = match data.split_once('\n') {
            Some((event, payload)) if !event.trim_start().starts_with('{') => {
                (event.trim().to_string(), payload.to_string())
            }
            _ => (String::new(), data.to_string()),
        };
        let Some(value) = parse_sse_json(&payload)? else {
            return Ok(out);
        };
        let body = unwrap_event(&value, &event);

        match event.as_str() {
            "messageStart" => {
                self.ensure_start(&mut out);
            }
            "contentBlockStart" => {
                self.ensure_start(&mut out);
                let index = content_block_index(body);
                if let Some(tool_use) = body.get("start").and_then(|start| start.get("toolUse")) {
                    out.push(StreamEvent::ToolCallStart {
                        index,
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
                        kind: ToolCallKind::Function,
                    });
                }
            }
            "contentBlockDelta" => {
                self.ensure_start(&mut out);
                let index = content_block_index(body);
                let delta = body.get("delta").cloned().unwrap_or(Value::Null);
                if let Some(text) = delta.get("text").and_then(Value::as_str)
                    && !text.is_empty()
                {
                    out.push(StreamEvent::TextDelta {
                        text: text.to_string(),
                    });
                }
                if let Some(text) = delta
                    .get("reasoningContent")
                    .and_then(|content| content.get("text"))
                    .and_then(Value::as_str)
                    && !text.is_empty()
                {
                    out.push(StreamEvent::ReasoningDelta {
                        text: text.to_string(),
                    });
                }
                if let Some(input) = delta
                    .get("toolUse")
                    .and_then(|tool_use| tool_use.get("input"))
                    .and_then(Value::as_str)
                    && !input.is_empty()
                {
                    out.push(StreamEvent::ToolCallDelta {
                        index,
                        arguments: input.to_string(),
                    });
                }
            }
            "contentBlockStop" => {}
            "messageStop" => {
                self.ensure_start(&mut out);
                if !self.finished {
                    self.finished = true;
                    out.push(StreamEvent::Finish {
                        reason: finish_reason_from(body.get("stopReason").and_then(Value::as_str)),
                    });
                }
            }
            "metadata" => {
                self.ensure_start(&mut out);
                if let Some(usage) = body.get("usage") {
                    out.push(StreamEvent::Usage(decode_usage(Some(usage))));
                }
            }
            "internalServerException"
            | "validationException"
            | "throttlingException"
            | "modelStreamErrorException"
            | "serviceUnavailableException" => {
                self.finished = true;
                let status = match event.as_str() {
                    "validationException" => 400,
                    "throttlingException" => 429,
                    "serviceUnavailableException" => 503,
                    _ => 500,
                };
                let message = body
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or(&event)
                    .to_string();
                out.push(StreamEvent::Error(GatewayError::upstream(
                    status,
                    format!("bedrock {event}: {message}"),
                )));
            }
            _ => {}
        }
        Ok(out)
    }

    fn finish(&mut self) -> Vec<StreamEvent> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        vec![StreamEvent::Finish {
            reason: FinishReason::Stop,
        }]
    }
}

fn content_block_index(body: &Value) -> usize {
    body.get("contentBlockIndex")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize
}

/// Accepts both the raw event payload and the wrapped SDK union shape.
fn unwrap_event<'a>(value: &'a Value, event: &str) -> &'a Value {
    if event.is_empty() {
        return value;
    }
    value.get(event).unwrap_or(value)
}

#[derive(Debug, Default)]
pub struct Encoder;

impl Encoder {
    pub fn new() -> Self {
        Self
    }
}

impl StreamEncoder for Encoder {
    fn encode(&mut self, _event: &StreamEvent) -> ProtocolResult<Vec<SseFrame>> {
        // Bedrock is upstream-only: no ingress route selects this encoder.
        Ok(Vec::new())
    }

    fn finish(&mut self) -> ProtocolResult<Vec<SseFrame>> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn chunk(event: &str, payload: Value) -> String {
        format!("{event}\n{payload}")
    }

    #[test]
    fn decodes_text_stream_and_usage() {
        let mut decoder = Decoder::new();
        let mut events = Vec::new();
        for (event, payload) in [
            ("messageStart", json!({"role": "assistant"})),
            (
                "contentBlockDelta",
                json!({"contentBlockIndex": 0, "delta": {"text": "Hello"}}),
            ),
            (
                "contentBlockDelta",
                json!({"contentBlockIndex": 0, "delta": {"text": " world"}}),
            ),
            ("contentBlockStop", json!({"contentBlockIndex": 0})),
            ("messageStop", json!({"stopReason": "end_turn"})),
            (
                "metadata",
                json!({
                    "usage": {
                        "inputTokens": 3,
                        "outputTokens": 2,
                        "totalTokens": 5,
                    }
                }),
            ),
        ] {
            events.extend(decoder.decode_chunk(&chunk(event, payload)).unwrap());
        }
        assert_eq!(
            events,
            vec![
                StreamEvent::MessageStart {
                    id: "bedrock".to_string(),
                    model: String::new(),
                },
                StreamEvent::TextDelta {
                    text: "Hello".to_string(),
                },
                StreamEvent::TextDelta {
                    text: " world".to_string(),
                },
                StreamEvent::Finish {
                    reason: FinishReason::Stop,
                },
                StreamEvent::Usage(octra_llm_core::Usage {
                    input_tokens: 3,
                    output_tokens: 2,
                    ..Default::default()
                }),
            ]
        );
    }

    #[test]
    fn decodes_tool_use_and_reasoning() {
        let mut decoder = Decoder::new();
        let events = decoder
            .decode_chunk(&chunk(
                "contentBlockStart",
                json!({
                    "contentBlockIndex": 2,
                    "start": {"toolUse": {"toolUseId": "tooluse_1", "name": "lookup"}},
                }),
            ))
            .unwrap();
        assert_eq!(
            events,
            vec![
                StreamEvent::MessageStart {
                    id: "bedrock".to_string(),
                    model: String::new(),
                },
                StreamEvent::ToolCallStart {
                    index: 2,
                    id: "tooluse_1".to_string(),
                    name: "lookup".to_string(),
                    kind: ToolCallKind::Function,
                },
            ]
        );

        let events = decoder
            .decode_chunk(&chunk(
                "contentBlockDelta",
                json!({
                    "contentBlockIndex": 2,
                    "delta": {"toolUse": {"input": "{\"q\":"}},
                }),
            ))
            .unwrap();
        assert_eq!(
            events,
            vec![StreamEvent::ToolCallDelta {
                index: 2,
                arguments: "{\"q\":".to_string(),
            }]
        );

        let events = decoder
            .decode_chunk(&chunk(
                "contentBlockDelta",
                json!({
                    "contentBlockIndex": 1,
                    "delta": {"reasoningContent": {"text": "thinking"}},
                }),
            ))
            .unwrap();
        assert_eq!(
            events,
            vec![StreamEvent::ReasoningDelta {
                text: "thinking".to_string(),
            }]
        );
    }

    #[test]
    fn maps_stop_reasons_and_exception_events() {
        let mut decoder = Decoder::new();
        let events = decoder
            .decode_chunk(&chunk("messageStop", json!({"stopReason": "tool_use"})))
            .unwrap();
        assert_eq!(
            events,
            vec![
                StreamEvent::MessageStart {
                    id: "bedrock".to_string(),
                    model: String::new(),
                },
                StreamEvent::Finish {
                    reason: FinishReason::ToolCalls,
                },
            ]
        );
        assert!(decoder.finish().is_empty());

        let mut decoder = Decoder::new();
        let events = decoder
            .decode_chunk(&chunk(
                "throttlingException",
                json!({"message": "slow down"}),
            ))
            .unwrap();
        assert!(matches!(
            events.as_slice(),
            [StreamEvent::Error(error)]
                if error.status == 429 && error.message.contains("slow down")
        ));
        assert!(decoder.finish().is_empty());
    }

    #[test]
    fn finish_emits_stop_once() {
        let mut decoder = Decoder::new();
        assert_eq!(
            decoder.finish(),
            vec![StreamEvent::Finish {
                reason: FinishReason::Stop,
            }]
        );
        assert!(decoder.finish().is_empty());
    }
}
