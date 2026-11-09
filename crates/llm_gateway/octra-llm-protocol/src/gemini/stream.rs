//! Gemini streamGenerateContent SSE streaming.

use octra_llm_core::{FinishReason, StreamEvent, ToolCallKind, Usage, Value};
use serde_json::json;

use super::response::{
    check_blocked, decode_usage, encode_usage, error_frames, finish_reason_from, finish_reason_to,
};
use crate::common::{parse_sse_json, sse_data_frame};
use crate::{SseFrame, StreamDecoder, StreamEncoder};

#[derive(Debug, Default)]
pub struct Decoder {
    started: bool,
    finish: Option<FinishReason>,
    done: bool,
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }
}

impl StreamDecoder for Decoder {
    fn decode_chunk(&mut self, data: &str) -> octra_llm_core::ProtocolResult<Vec<StreamEvent>> {
        let mut out = Vec::new();
        let Some(value) = parse_sse_json(data)? else {
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
        if let Some(object) = value.as_object() {
            check_blocked(object)?;
        }
        if !self.started {
            self.started = true;
            out.push(StreamEvent::MessageStart {
                id: format!("gemini-{}", chrono_seconds()),
                model: value
                    .get("modelVersion")
                    .and_then(Value::as_str)
                    .unwrap_or("gemini")
                    .to_string(),
            });
        }

        let candidate = value
            .get("candidates")
            .and_then(Value::as_array)
            .and_then(|candidates| candidates.first())
            .cloned()
            .unwrap_or(Value::Null);
        if let Some(parts) = candidate
            .get("content")
            .and_then(|content| content.get("parts"))
            .and_then(Value::as_array)
        {
            for (index, part) in parts.iter().enumerate() {
                if let Some(call) = part.get("functionCall") {
                    let name = call
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let id = format!("call_{name}_{index}");
                    out.push(StreamEvent::ToolCallStart {
                        index,
                        id,
                        name,
                        kind: ToolCallKind::Function,
                    });
                    let arguments = call
                        .get("args")
                        .cloned()
                        .unwrap_or_else(|| json!({}))
                        .to_string();
                    out.push(StreamEvent::ToolCallDelta { index, arguments });
                    continue;
                }
                if let Some(text) = part.get("text").and_then(Value::as_str)
                    && !text.is_empty()
                {
                    if part
                        .get("thought")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                    {
                        out.push(StreamEvent::ReasoningDelta {
                            text: text.to_string(),
                        });
                    } else {
                        out.push(StreamEvent::TextDelta {
                            text: text.to_string(),
                        });
                    }
                }
            }
        }

        if let Some(usage) = value.get("usageMetadata").filter(|usage| usage.is_object()) {
            out.push(StreamEvent::Usage(decode_usage(Some(usage))));
        }
        if let Some(reason) = candidate
            .get("finishReason")
            .filter(|value| !value.is_null())
        {
            let reason = finish_reason_from(Some(reason));
            if self.finish.is_none() {
                self.finish = Some(reason);
                out.push(StreamEvent::Finish { reason });
            }
            self.done = true;
        }
        Ok(out)
    }

    fn finish(&mut self) -> Vec<StreamEvent> {
        if self.finish.is_some() {
            return Vec::new();
        }
        self.finish = Some(FinishReason::Stop);
        vec![StreamEvent::Finish {
            reason: FinishReason::Stop,
        }]
    }
}

#[derive(Debug)]
struct ToolState {
    index: usize,
    name: String,
    arguments: String,
}

#[derive(Debug)]
pub struct Encoder {
    model: String,
    tools: Vec<ToolState>,
    usage: Option<Usage>,
    finish: Option<FinishReason>,
    started: bool,
    finished: bool,
}

impl Default for Encoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Encoder {
    pub fn new() -> Self {
        Self {
            model: String::new(),
            tools: Vec::new(),
            usage: None,
            finish: None,
            started: false,
            finished: false,
        }
    }

    fn start(&mut self) -> Vec<SseFrame> {
        if self.started {
            return Vec::new();
        }
        self.started = true;
        vec![sse_data_frame(&json!({
            "candidates": [{"content": {"role": "model", "parts": []}, "index": 0}],
            "modelVersion": self.model,
        }))]
    }

    fn chunk(parts: Value) -> SseFrame {
        sse_data_frame(&json!({
            "candidates": [{"content": {"role": "model", "parts": parts}, "index": 0}],
        }))
    }

    fn finish_frame(&self, reason: FinishReason) -> SseFrame {
        let mut value = json!({
            "candidates": [{
                "content": {"role": "model", "parts": []},
                "finishReason": finish_reason_to(&reason),
                "index": 0,
            }],
        });
        if let Some(usage) = &self.usage {
            value["usageMetadata"] = encode_usage(usage);
        }
        sse_data_frame(&value)
    }
}

impl StreamEncoder for Encoder {
    fn encode(&mut self, event: &StreamEvent) -> octra_llm_core::ProtocolResult<Vec<SseFrame>> {
        match event {
            StreamEvent::MessageStart { model, .. } => {
                self.model = model.clone();
                Ok(self.start())
            }
            StreamEvent::TextDelta { text } => {
                let mut frames = self.start();
                frames.push(Self::chunk(json!([{"text": text}])));
                Ok(frames)
            }
            StreamEvent::ReasoningDelta { text } => {
                let mut frames = self.start();
                frames.push(Self::chunk(json!([{"text": text, "thought": true}])));
                Ok(frames)
            }
            StreamEvent::ReasoningSignatureDelta { .. } => Ok(Vec::new()),
            StreamEvent::ToolCallStart { index, name, .. } => {
                if self.tools.iter().all(|tool| tool.index != *index) {
                    self.tools.push(ToolState {
                        index: *index,
                        name: name.clone(),
                        arguments: String::new(),
                    });
                }
                Ok(Vec::new())
            }
            StreamEvent::ToolCallDelta { index, arguments } => {
                if let Some(tool) = self.tools.iter_mut().find(|tool| tool.index == *index) {
                    tool.arguments.push_str(arguments);
                }
                Ok(Vec::new())
            }
            StreamEvent::ProviderToolStart { .. } | StreamEvent::ProviderToolResult { .. } => {
                Ok(Vec::new())
            }
            StreamEvent::Usage(usage) => {
                self.usage = Some(usage.clone());
                Ok(Vec::new())
            }
            StreamEvent::Finish { reason } => {
                self.finish = Some(*reason);
                Ok(Vec::new())
            }
            StreamEvent::Error(error) => Ok(error_frames(error)),
        }
    }

    fn finish(&mut self) -> octra_llm_core::ProtocolResult<Vec<SseFrame>> {
        if self.finished {
            return Ok(Vec::new());
        }
        self.finished = true;
        let mut frames = self.start();
        if !self.tools.is_empty() {
            let parts: Vec<Value> = self
                .tools
                .iter()
                .map(|tool| {
                    let arguments = serde_json::from_str::<Value>(&tool.arguments)
                        .unwrap_or_else(|_| json!({}));
                    json!({"functionCall": {"name": tool.name, "args": arguments}})
                })
                .collect();
            frames.push(Self::chunk(Value::Array(parts)));
        }
        frames.push(self.finish_frame(self.finish.unwrap_or(FinishReason::Stop)));
        Ok(frames)
    }
}

fn chrono_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}
