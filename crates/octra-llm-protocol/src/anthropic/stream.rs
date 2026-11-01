//! Anthropic Messages SSE streaming.
//!
//! Decoding wraps each `content_block_start` with a canonical
//! `ToolCallStart` / `ProviderToolStart`, forwards `content_block_delta`
//! payloads, and emits `Usage` / `Finish` from `message_delta` and
//! `message_stop`. Encoder output groups consecutive delta events into one
//! `content_block_delta` object.

use octra_llm_core::{
    FinishReason, ProtocolError, ProtocolResult, StreamEvent, ToolCallKind, Usage, Value,
};
use serde_json::json;

use super::response::{encode_usage, stop_reason_to};
use crate::common::{parse_sse_json, sse_event_frame};
use crate::{SseFrame, StreamDecoder, StreamEncoder};

#[derive(Debug, Default)]
pub struct Decoder {
    started: bool,
    finished: bool,
    message_id: String,
    model: String,
    next_index: usize,
    input_tokens: Option<u64>,
    pending_finish: Option<FinishReason>,
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    fn ensure_start(&mut self, out: &mut Vec<StreamEvent>) {
        if !self.started {
            self.started = true;
            out.push(StreamEvent::MessageStart {
                id: self.message_id.clone(),
                model: self.model.clone(),
            });
        }
    }
}

impl StreamDecoder for Decoder {
    fn decode_chunk(&mut self, data: &str) -> ProtocolResult<Vec<StreamEvent>> {
        let mut out = Vec::new();
        // The gateway feeds `event: value` pairs as `event\ndata`, mirroring
        // the shared SSE decoder's event names.
        let (event, payload) = match data.split_once('\n') {
            Some((event, payload)) if !event.trim_start().starts_with('{') => {
                (event.trim().to_string(), payload.to_string())
            }
            _ => (String::new(), data.to_string()),
        };
        let Some(value) = parse_sse_json(&payload)? else {
            return Ok(out);
        };
        let kind = if event.is_empty() {
            value
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        } else {
            event
        };

        match kind.as_str() {
            "message_start" => {
                let message = value.get("message").cloned().unwrap_or(Value::Null);
                self.message_id = message
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("msg_unknown")
                    .to_string();
                self.model = message
                    .get("model")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                self.input_tokens = message
                    .get("usage")
                    .and_then(|usage| usage.get("input_tokens"))
                    .and_then(Value::as_u64);
                self.ensure_start(&mut out);
            }
            "content_block_start" => {
                self.ensure_start(&mut out);
                let index = value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                self.next_index = index + 1;
                let block = value.get("content_block").cloned().unwrap_or(Value::Null);
                let block_type = block
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("text")
                    .to_string();
                match block_type.as_str() {
                    "tool_use" | "server_tool_use" => {
                        out.push(StreamEvent::ToolCallStart {
                            index,
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
                            kind: if block_type == "server_tool_use" {
                                ToolCallKind::Provider
                            } else {
                                ToolCallKind::Function
                            },
                        });
                        // The block's initial input is normally empty; non-empty
                        // input is emitted as a delta so the arguments are not
                        // lost.
                        if let Some(input) = block.get("input")
                            && input.as_object().is_some_and(|map| !map.is_empty())
                        {
                            out.push(StreamEvent::ToolCallDelta {
                                index,
                                arguments: input.to_string(),
                            });
                        }
                    }
                    "thinking" | "redacted_thinking" | "text" => {}
                    other => {
                        out.push(StreamEvent::ProviderToolResult {
                            index,
                            tool_use_id: block
                                .get("tool_use_id")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            content: json!({"type": other, "block": block}),
                            is_error: false,
                        });
                    }
                }
            }
            "content_block_delta" => {
                let index = value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let delta = value.get("delta").cloned().unwrap_or(Value::Null);
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        if let Some(text) = delta.get("text").and_then(Value::as_str)
                            && !text.is_empty()
                        {
                            out.push(StreamEvent::TextDelta {
                                text: text.to_string(),
                            });
                        }
                    }
                    Some("thinking_delta") => {
                        if let Some(text) = delta.get("thinking").and_then(Value::as_str)
                            && !text.is_empty()
                        {
                            out.push(StreamEvent::ReasoningDelta {
                                text: text.to_string(),
                            });
                        }
                    }
                    Some("signature_delta") => {
                        if let Some(signature) = delta.get("signature").and_then(Value::as_str)
                            && !signature.is_empty()
                        {
                            out.push(StreamEvent::ReasoningSignatureDelta {
                                signature: signature.to_string(),
                            });
                        }
                    }
                    Some("input_json_delta") => {
                        if let Some(partial) = delta.get("partial_json").and_then(Value::as_str)
                            && !partial.is_empty()
                        {
                            out.push(StreamEvent::ToolCallDelta {
                                index,
                                arguments: partial.to_string(),
                            });
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {}
            "message_delta" => {
                self.ensure_start(&mut out);
                if let Some(usage) = value.get("usage") {
                    let output = usage
                        .get("output_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    out.push(StreamEvent::Usage(Usage {
                        input_tokens: self.input_tokens.unwrap_or(0),
                        output_tokens: output,
                        cache_read_tokens: usage
                            .get("cache_read_input_tokens")
                            .and_then(Value::as_u64),
                        cache_write_tokens: usage
                            .get("cache_creation_input_tokens")
                            .and_then(Value::as_u64),
                        reasoning_tokens: None,
                    }));
                }
                if let Some(reason) = value
                    .get("delta")
                    .and_then(|delta| delta.get("stop_reason"))
                    .and_then(Value::as_str)
                {
                    let reason = match reason {
                        "tool_use" => FinishReason::ToolCalls,
                        "max_tokens" => FinishReason::Length,
                        "pause_turn" => FinishReason::Pause,
                        "refusal" => FinishReason::Refusal,
                        "end_turn" | "stop_sequence" => FinishReason::Stop,
                        _ => FinishReason::Other,
                    };
                    self.pending_finish = Some(reason);
                }
            }
            "message_stop" => {
                let reason = self.pending_finish.take().unwrap_or(FinishReason::Stop);
                if !self.finished {
                    self.finished = true;
                    out.push(StreamEvent::Finish { reason });
                }
            }
            "error" => {
                let error = value.get("error").cloned().unwrap_or(Value::Null);
                return Err(ProtocolError::new(format!(
                    "upstream error: {}",
                    error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                )));
            }
            "ping" => {}
            _ => {}
        }
        Ok(out)
    }

    fn finish(&mut self) -> Vec<StreamEvent> {
        if self.finished || !self.started {
            return Vec::new();
        }
        self.finished = true;
        vec![StreamEvent::Finish {
            reason: self.pending_finish.take().unwrap_or(FinishReason::Stop),
        }]
    }
}

#[derive(Debug)]
enum OpenBlock {
    None,
    Text { index: usize },
    Thinking { index: usize },
    ToolUse { index: usize },
    Other { index: usize },
}

#[derive(Debug)]
pub struct Encoder {
    started: bool,
    finished: bool,
    id: String,
    model: String,
    open: OpenBlock,
    next_index: usize,
    usage: Usage,
    finish_reason: FinishReason,
    finished_usage: bool,
}

impl Default for Encoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Encoder {
    pub fn new() -> Self {
        Self {
            started: false,
            finished: false,
            id: "msg_octra".to_string(),
            model: String::new(),
            open: OpenBlock::None,
            next_index: 0,
            usage: Usage::default(),
            finish_reason: FinishReason::Stop,
            finished_usage: false,
        }
    }

    fn close(&mut self) -> Option<SseFrame> {
        let index = match std::mem::replace(&mut self.open, OpenBlock::None) {
            OpenBlock::None => return None,
            OpenBlock::Text { index }
            | OpenBlock::Thinking { index }
            | OpenBlock::ToolUse { index }
            | OpenBlock::Other { index } => index,
        };
        Some(sse_event_frame(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": index}),
        ))
    }

    fn start(&mut self) -> Vec<SseFrame> {
        if self.started {
            return Vec::new();
        }
        self.started = true;
        vec![sse_event_frame(
            "message_start",
            &json!({
                "type": "message_start",
                "message": {
                    "id": self.id,
                    "type": "message",
                    "role": "assistant",
                    "model": self.model,
                    "content": [],
                    "stop_reason": Value::Null,
                    "stop_sequence": Value::Null,
                    "usage": {"input_tokens": 0, "output_tokens": 0},
                }
            }),
        )]
    }

    fn delta_frame(&self, index: usize, delta: Value) -> SseFrame {
        sse_event_frame(
            "content_block_delta",
            &json!({"type": "content_block_delta", "index": index, "delta": delta}),
        )
    }

    fn finish_frames(&mut self) -> Vec<SseFrame> {
        if self.finished_usage {
            return Vec::new();
        }
        self.finished_usage = true;
        let usage = if self.usage.input_tokens > 0 || self.usage.output_tokens > 0 {
            encode_usage(&self.usage)
        } else {
            json!({"input_tokens": 0, "output_tokens": 0})
        };
        vec![
            sse_event_frame(
                "message_delta",
                &json!({
                    "type": "message_delta",
                    "delta": {
                        "stop_reason": stop_reason_to(&self.finish_reason),
                        "stop_sequence": Value::Null,
                    },
                    "usage": usage,
                }),
            ),
            sse_event_frame("message_stop", &json!({"type": "message_stop"})),
        ]
    }
}

impl StreamEncoder for Encoder {
    fn encode(&mut self, event: &StreamEvent) -> ProtocolResult<Vec<SseFrame>> {
        match event {
            StreamEvent::MessageStart { id, model } => {
                self.id = id.clone();
                self.model = model.clone();
                Ok(self.start())
            }
            StreamEvent::TextDelta { text } => {
                let mut frames = self.start();
                let index = match self.open {
                    OpenBlock::Text { index } => index,
                    _ => {
                        if let Some(frame) = self.close() {
                            frames.push(frame);
                        }
                        let index = self.next_index;
                        self.next_index += 1;
                        self.open = OpenBlock::Text { index };
                        frames.push(sse_event_frame(
                            "content_block_start",
                            &json!({
                                "type": "content_block_start",
                                "index": index,
                                "content_block": {"type": "text", "text": ""},
                            }),
                        ));
                        index
                    }
                };
                frames.push(self.delta_frame(index, json!({"type": "text_delta", "text": text})));
                Ok(frames)
            }
            StreamEvent::ReasoningDelta { text } => {
                let mut frames = self.start();
                let index = match self.open {
                    OpenBlock::Thinking { index } => index,
                    _ => {
                        if let Some(frame) = self.close() {
                            frames.push(frame);
                        }
                        let index = self.next_index;
                        self.next_index += 1;
                        self.open = OpenBlock::Thinking { index };
                        frames.push(sse_event_frame(
                            "content_block_start",
                            &json!({
                                "type": "content_block_start",
                                "index": index,
                                "content_block": {"type": "thinking", "thinking": "", "signature": ""},
                            }),
                        ));
                        index
                    }
                };
                frames.push(
                    self.delta_frame(index, json!({"type": "thinking_delta", "thinking": text})),
                );
                Ok(frames)
            }
            StreamEvent::ReasoningSignatureDelta { signature } => {
                let mut frames = self.start();
                if let OpenBlock::Thinking { index } = self.open {
                    frames.push(self.delta_frame(
                        index,
                        json!({"type": "signature_delta", "signature": signature}),
                    ));
                }
                Ok(frames)
            }
            StreamEvent::ToolCallStart {
                index: _,
                id,
                name,
                kind,
            } => {
                let mut frames = self.start();
                if let Some(frame) = self.close() {
                    frames.push(frame);
                }
                let index = self.next_index;
                self.next_index += 1;
                let provider = *kind == ToolCallKind::Provider;
                self.open = OpenBlock::ToolUse { index };
                let block = if provider {
                    json!({
                        "type": "server_tool_use",
                        "id": id,
                        "name": name,
                        "input": {},
                    })
                } else {
                    json!({
                        "type": "tool_use",
                        "id": id,
                        "name": name,
                        "input": {},
                    })
                };
                frames.push(sse_event_frame(
                    "content_block_start",
                    &json!({
                        "type": "content_block_start",
                        "index": index,
                        "content_block": block,
                    }),
                ));
                Ok(frames)
            }
            StreamEvent::ToolCallDelta { arguments, .. } => {
                let mut frames = self.start();
                if let OpenBlock::ToolUse { index } = self.open {
                    frames.push(self.delta_frame(
                        index,
                        json!({"type": "input_json_delta", "partial_json": arguments}),
                    ));
                }
                Ok(frames)
            }
            StreamEvent::ProviderToolStart { tool_type, .. } => {
                let mut frames = self.start();
                if let Some(frame) = self.close() {
                    frames.push(frame);
                }
                let index = self.next_index;
                self.next_index += 1;
                self.open = OpenBlock::Other { index };
                frames.push(sse_event_frame(
                    "content_block_start",
                    &json!({
                        "type": "content_block_start",
                        "index": index,
                        "content_block": {"type": tool_type},
                    }),
                ));
                Ok(frames)
            }
            StreamEvent::ProviderToolResult {
                tool_use_id,
                content,
                is_error,
                ..
            } => {
                let mut frames = self.start();
                if let Some(frame) = self.close() {
                    frames.push(frame);
                }
                let index = self.next_index;
                self.next_index += 1;
                let block_type = content
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("web_search_tool_result")
                    .to_string();
                let mut block = if block_type.ends_with("_tool_result") {
                    content.clone()
                } else {
                    json!({"type": "web_search_tool_result"})
                };
                if let Some(map) = block.as_object_mut() {
                    map.insert("tool_use_id".to_string(), json!(tool_use_id));
                    if let Some(inner) = content.get("block") {
                        map.insert(block_type, inner.clone());
                    }
                    if *is_error {
                        map.insert("is_error".to_string(), json!(true));
                    }
                }
                frames.push(sse_event_frame(
                    "content_block_start",
                    &json!({
                        "type": "content_block_start",
                        "index": index,
                        "content_block": block,
                    }),
                ));
                frames.push(sse_event_frame(
                    "content_block_stop",
                    &json!({"type": "content_block_stop", "index": index}),
                ));
                Ok(frames)
            }
            StreamEvent::Usage(usage) => {
                // Anthropic reports usage on message_start and message_delta,
                // both after the stream began; keep it for the final frames.
                self.usage = usage.clone();
                Ok(Vec::new())
            }
            StreamEvent::Finish { reason } => {
                let mut frames = self.start();
                self.finish_reason = *reason;
                if let Some(frame) = self.close() {
                    frames.push(frame);
                }
                frames.extend(self.finish_frames());
                self.finished = true;
                Ok(frames)
            }
            StreamEvent::Error(error) => Ok(super::response::error_frames(error)),
        }
    }

    fn finish(&mut self) -> ProtocolResult<Vec<SseFrame>> {
        if self.finished {
            return Ok(Vec::new());
        }
        self.finished = true;
        let mut frames = self.start();
        if let Some(frame) = self.close() {
            frames.push(frame);
        }
        frames.extend(self.finish_frames());
        Ok(frames)
    }
}
