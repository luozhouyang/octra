//! OpenAI Responses API SSE streaming.

use octra_llm_core::{FinishReason, StreamEvent, ToolCallKind, Usage, Value};
use serde_json::json;

use super::response::{data_event, decode_usage, encode_usage, response_object};
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

    fn ensure_start(&mut self, id: &str, model: &str, out: &mut Vec<StreamEvent>) {
        if !self.started {
            self.started = true;
            out.push(StreamEvent::MessageStart {
                id: id.to_string(),
                model: model.to_string(),
            });
        }
        self.id.get_or_insert_with(|| id.to_string());
        if self.model.as_deref().unwrap_or_default().is_empty() {
            self.model = Some(model.to_string());
        }
    }
}

impl StreamDecoder for Decoder {
    fn decode_chunk(&mut self, data: &str) -> octra_llm_core::ProtocolResult<Vec<StreamEvent>> {
        let mut out = Vec::new();
        let Some(value) = parse_sse_json(data)? else {
            self.done = true;
            return Ok(out);
        };
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if kind == "error" || kind == "response.failed" {
            return Err(octra_llm_core::ProtocolError::new(format!(
                "upstream error: {}",
                value
                    .get("message")
                    .or_else(|| value.get("error").and_then(|error| error.get("message")))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
            )));
        }

        if kind == "response.created" {
            if let Some(response) = value.get("response") {
                let id = response
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("resp_unknown");
                let model = response
                    .get("model")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                self.ensure_start(id, model, &mut out);
            }
            return Ok(out);
        }

        if !self.started {
            self.ensure_start("resp_unknown", "", &mut out);
        }

        match kind {
            "response.output_item.added" => {
                let output_index = value
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as usize;
                if let Some(item) = value.get("item")
                    && item.get("type").and_then(Value::as_str) == Some("function_call")
                {
                    out.push(StreamEvent::ToolCallStart {
                        index: output_index,
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
                        kind: ToolCallKind::Function,
                    });
                }
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                if let Some(delta) = value.get("delta").and_then(Value::as_str)
                    && !delta.is_empty()
                {
                    out.push(StreamEvent::TextDelta {
                        text: delta.to_string(),
                    });
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(delta) = value.get("delta").and_then(Value::as_str)
                    && !delta.is_empty()
                {
                    out.push(StreamEvent::ReasoningDelta {
                        text: delta.to_string(),
                    });
                }
            }
            "response.function_call_arguments.delta" => {
                if let Some(delta) = value.get("delta").and_then(Value::as_str)
                    && !delta.is_empty()
                {
                    out.push(StreamEvent::ToolCallDelta {
                        index: value
                            .get("output_index")
                            .and_then(Value::as_u64)
                            .unwrap_or(0) as usize,
                        arguments: delta.to_string(),
                    });
                }
            }
            "response.completed" | "response.incomplete" => {
                if let Some(response) = value.get("response") {
                    if let Some(usage) = response.get("usage").filter(|usage| usage.is_object()) {
                        out.push(StreamEvent::Usage(decode_usage(Some(usage))));
                    }
                    let reason = if kind == "response.incomplete" {
                        match response
                            .get("incomplete_details")
                            .and_then(|details| details.get("reason"))
                            .and_then(Value::as_str)
                        {
                            Some("content_filter") => FinishReason::ContentFilter,
                            _ => FinishReason::Length,
                        }
                    } else if response
                        .get("output")
                        .and_then(Value::as_array)
                        .is_some_and(|items| {
                            items.iter().any(|item| {
                                item.get("type").and_then(Value::as_str) == Some("function_call")
                            })
                        })
                    {
                        FinishReason::ToolCalls
                    } else {
                        FinishReason::Stop
                    };
                    self.finish = Some(reason);
                    out.push(StreamEvent::Finish { reason });
                    self.done = true;
                }
            }
            _ => {}
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
    call_id: String,
    name: String,
    arguments: String,
}

#[derive(Debug)]
pub struct Encoder {
    id: String,
    model: String,
    text: String,
    reasoning: String,
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
            id: "resp_octra".to_string(),
            model: String::new(),
            text: String::new(),
            reasoning: String::new(),
            tools: Vec::new(),
            usage: None,
            finish: None,
            started: false,
            finished: false,
        }
    }

    fn created(&mut self) -> Vec<SseFrame> {
        if self.started {
            return Vec::new();
        }
        self.started = true;
        let mut frames = vec![data_event(
            "response.created",
            json!({"response": response_object(&self.id, &self.model, "in_progress")}),
        )];
        frames.push(data_event(
            "response.in_progress",
            json!({"response": response_object(&self.id, &self.model, "in_progress")}),
        ));
        frames
    }

    fn completed(&self, status: &str) -> Value {
        let mut output = Vec::new();
        if !self.reasoning.is_empty() {
            output.push(json!({
                "type": "reasoning",
                "id": "rs_octra",
                "summary": [{"type": "summary_text", "text": self.reasoning}],
            }));
        }
        if !self.text.is_empty() {
            output.push(json!({
                "type": "message",
                "id": "msg_octra",
                "role": "assistant",
                "status": "completed",
                "content": [{"type": "output_text", "text": self.text}],
            }));
        }
        for tool in &self.tools {
            output.push(json!({
                "type": "function_call",
                "id": format!("fc_{}", tool.call_id),
                "call_id": tool.call_id,
                "name": tool.name,
                "arguments": tool.arguments,
                "status": "completed",
            }));
        }
        let mut response = response_object(&self.id, &self.model, status);
        response["output"] = Value::Array(output);
        response["output_text"] = json!(self.text);
        response["usage"] = self
            .usage
            .as_ref()
            .map(encode_usage)
            .unwrap_or_else(|| encode_usage(&Usage::default()));
        response
    }
}

impl StreamEncoder for Encoder {
    fn encode(&mut self, event: &StreamEvent) -> octra_llm_core::ProtocolResult<Vec<SseFrame>> {
        match event {
            StreamEvent::MessageStart { id, model } => {
                self.id = id.clone();
                self.model = model.clone();
                Ok(self.created())
            }
            StreamEvent::TextDelta { text } => {
                let mut frames = self.created();
                self.text.push_str(text);
                frames.push(data_event(
                    "response.output_text.delta",
                    json!({
                        "item_id": "msg_octra",
                        "output_index": 1,
                        "content_index": 0,
                        "delta": text,
                    }),
                ));
                Ok(frames)
            }
            StreamEvent::ReasoningDelta { text } => {
                let mut frames = self.created();
                self.reasoning.push_str(text);
                frames.push(data_event(
                    "response.reasoning_summary_text.delta",
                    json!({
                        "item_id": "rs_octra",
                        "output_index": 0,
                        "summary_index": 0,
                        "delta": text,
                    }),
                ));
                Ok(frames)
            }
            StreamEvent::ReasoningSignatureDelta { .. } => Ok(Vec::new()),
            StreamEvent::ToolCallStart {
                index, id, name, ..
            } => {
                let mut frames = self.created();
                self.tools.push(ToolState {
                    index: *index,
                    call_id: id.clone(),
                    name: name.clone(),
                    arguments: String::new(),
                });
                frames.push(data_event(
                    "response.output_item.added",
                    json!({
                        "output_index": 2 + index,
                        "item": {
                            "type": "function_call",
                            "id": format!("fc_{id}"),
                            "call_id": id,
                            "name": name,
                            "arguments": "",
                            "status": "in_progress",
                        },
                    }),
                ));
                Ok(frames)
            }
            StreamEvent::ToolCallDelta { index, arguments } => {
                if let Some(tool) = self.tools.iter_mut().find(|tool| tool.index == *index) {
                    tool.arguments.push_str(arguments);
                    Ok(vec![data_event(
                        "response.function_call_arguments.delta",
                        json!({
                            "item_id": format!("fc_{}", tool.call_id),
                            "output_index": 2 + index,
                            "delta": arguments,
                        }),
                    )])
                } else {
                    Ok(Vec::new())
                }
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
            StreamEvent::Error(error) => Ok(super::response::error_frames(error)),
        }
    }

    fn finish(&mut self) -> octra_llm_core::ProtocolResult<Vec<SseFrame>> {
        if self.finished {
            return Ok(Vec::new());
        }
        self.finished = true;
        let mut frames = self.created();
        let reason = self.finish.unwrap_or(FinishReason::Stop);
        let status = match reason {
            FinishReason::Length | FinishReason::ContentFilter => "incomplete",
            _ => "completed",
        };
        let event = if status == "incomplete" {
            "response.incomplete"
        } else {
            "response.completed"
        };
        frames.push(data_event(
            event,
            json!({"response": self.completed(status)}),
        ));
        Ok(frames)
    }
}
