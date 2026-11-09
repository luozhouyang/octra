//! OpenAI Chat Completions SSE streaming.
//!
//! Decoding is incremental: upstream `chat.completion.chunk` frames become
//! canonical stream events. Tool call arguments arrive as JSON fragments and
//! are emitted as `ToolCallDelta` strings unchanged.
//!
//! Encoding stays on the canonical event path: the start event holds the
//! stream header, every other event becomes one chunk carrying only its delta.

use octra_llm_core::{FinishReason, StreamEvent, ToolCallKind, Value};

use serde_json::json;

use super::response::{finish_reason_from, finish_reason_to};
use crate::common::parse_sse_json;
use crate::{SseFrame, StreamDecoder, StreamEncoder};

#[derive(Debug, Default)]
pub struct Decoder {
    id: Option<String>,
    model: Option<String>,
    started: bool,
    finish: Option<FinishReason>,
    done: bool,
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    fn ensure_start(&mut self, id: String, model: String, out: &mut Vec<StreamEvent>) {
        if !self.started {
            self.started = true;
            out.push(StreamEvent::MessageStart {
                id: id.clone(),
                model: model.clone(),
            });
        }
        if self.id.is_none() {
            self.id = Some(id);
        }
        if self.model.is_none() || self.model.as_deref() == Some("") {
            self.model = Some(model);
        }
    }
}

impl StreamDecoder for Decoder {
    fn decode_chunk(&mut self, data: &str) -> octra_llm_core::ProtocolResult<Vec<StreamEvent>> {
        let mut out = Vec::new();
        let Some(value) = parse_sse_json(data)? else {
            // `[DONE]` terminates the stream; the finish event was already
            // produced when `finish_reason` was seen in a chunk.
            self.done = true;
            return Ok(out);
        };

        if let Some(error) = value.get("error") {
            return Err(octra_llm_core::ProtocolError::new(format!(
                "upstream error: {}",
                error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
            )));
        }

        let id = value
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("chatcmpl-unknown")
            .to_string();
        let model = value
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        self.ensure_start(id, model, &mut out);

        if let Some(usage) = value.get("usage").filter(|usage| usage.is_object()) {
            out.push(StreamEvent::Usage(super::response::decode_usage(Some(
                usage,
            ))));
        }

        let choices = value
            .get("choices")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for choice in choices {
            let delta = choice.get("delta").cloned().unwrap_or(Value::Null);
            if let Some(reasoning) = delta
                .get("reasoning_content")
                .or_else(|| delta.get("reasoning"))
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
            {
                out.push(StreamEvent::ReasoningDelta {
                    text: reasoning.to_string(),
                });
            }
            if let Some(text) = delta.get("content").and_then(Value::as_str)
                && !text.is_empty()
            {
                out.push(StreamEvent::TextDelta {
                    text: text.to_string(),
                });
            }
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    let index = call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                    if let Some(id) = call.get("id").and_then(Value::as_str) {
                        let name = call
                            .get("function")
                            .and_then(|function| function.get("name"))
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        out.push(StreamEvent::ToolCallStart {
                            index,
                            id: id.to_string(),
                            name: name.to_string(),
                            kind: ToolCallKind::Function,
                        });
                    }
                    if let Some(arguments) = call
                        .get("function")
                        .and_then(|function| function.get("arguments"))
                        .and_then(Value::as_str)
                        && !arguments.is_empty()
                    {
                        out.push(StreamEvent::ToolCallDelta {
                            index,
                            arguments: arguments.to_string(),
                        });
                    }
                }
            }
            if let Some(reason) = choice.get("finish_reason").filter(|value| !value.is_null()) {
                let reason = finish_reason_from(Some(reason));
                if self.finish.is_none() {
                    self.finish = Some(reason);
                    out.push(StreamEvent::Finish { reason });
                }
            }
        }
        Ok(out)
    }

    fn finish(&mut self) -> Vec<StreamEvent> {
        if self.done && self.finish.is_some() {
            return Vec::new();
        }
        if self.finish.is_none() {
            self.finish = Some(FinishReason::Stop);
            return vec![StreamEvent::Finish {
                reason: FinishReason::Stop,
            }];
        }
        Vec::new()
    }
}

#[derive(Debug)]
pub struct Encoder {
    id: String,
    model: String,
    created: i64,
    include_usage: bool,
    finished: bool,
    finish_reason: Option<FinishReason>,
    usage: Option<octra_llm_core::Usage>,
}

impl Default for Encoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Encoder {
    pub fn new() -> Self {
        Self {
            id: "chatcmpl-octra".to_string(),
            model: String::new(),
            created: now_seconds(),
            include_usage: false,
            finished: false,
            finish_reason: None,
            usage: None,
        }
    }

    pub fn with_usage(mut self, include: bool) -> Self {
        self.include_usage = include;
        self
    }

    fn usage_chunk(&self) -> SseFrame {
        let usage = self.usage.as_ref();
        crate::common::sse_data_frame(&json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [],
            "usage": usage
                .map(super::response::encode_usage)
                .unwrap_or_else(|| json!({"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0})),
        }))
    }

    fn chunk(&self, delta: Value, finish_reason: Option<&str>) -> SseFrame {
        crate::common::sse_data_frame(&json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{
                "index": 0,
                "delta": delta,
                "finish_reason": finish_reason,
            }],
        }))
    }
}

fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

impl StreamEncoder for Encoder {
    fn encode(&mut self, event: &StreamEvent) -> octra_llm_core::ProtocolResult<Vec<SseFrame>> {
        let frames = match event {
            StreamEvent::MessageStart { id, model } => {
                self.id = id.clone();
                self.model = model.clone();
                vec![self.chunk(json!({"role": "assistant", "content": ""}), None)]
            }
            StreamEvent::TextDelta { text } => vec![self.chunk(json!({"content": text}), None)],
            StreamEvent::ReasoningDelta { text } => {
                vec![self.chunk(json!({"reasoning_content": text}), None)]
            }
            StreamEvent::ReasoningSignatureDelta { .. } => Vec::new(),
            StreamEvent::ToolCallStart {
                index,
                id,
                name,
                kind: _,
            } => vec![self.chunk(
                json!({
                    "tool_calls": [{
                        "index": index,
                        "id": id,
                        "type": "function",
                        "function": {"name": name, "arguments": ""},
                    }]
                }),
                None,
            )],
            StreamEvent::ToolCallDelta { index, arguments } => vec![self.chunk(
                json!({
                    "tool_calls": [{
                        "index": index,
                        "function": {"arguments": arguments},
                    }]
                }),
                None,
            )],
            StreamEvent::ProviderToolStart { .. } | StreamEvent::ProviderToolResult { .. } => {
                Vec::new()
            }
            StreamEvent::Usage(usage) => {
                self.usage = Some(usage.clone());
                Vec::new()
            }
            StreamEvent::Finish { reason } => {
                // Usage can arrive after the finish chunk upstream; it is
                // buffered for `finish()`, which emits it before `[DONE]`.
                self.finish_reason = Some(*reason);
                vec![self.chunk(json!({}), Some(finish_reason_to(reason)))]
            }
            StreamEvent::Error(error) => super::response::error_frames(error),
        };
        Ok(frames)
    }

    fn finish(&mut self) -> octra_llm_core::ProtocolResult<Vec<SseFrame>> {
        if self.finished {
            return Ok(Vec::new());
        }
        self.finished = true;
        let mut frames = Vec::new();
        if self.finish_reason.is_none() {
            frames.push(self.chunk(json!({}), Some("stop")));
        }
        if self.include_usage {
            frames.push(self.usage_chunk());
        }
        frames.push(SseFrame::data("[DONE]"));
        Ok(frames)
    }
}
