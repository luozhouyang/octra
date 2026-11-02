//! Output adapters that map CLI JSONL into canonical stream events.
//!
//! `generic` is Octra's own stable contract: one JSON object per line with a
//! `type` of `text`, `reasoning`, `usage`, `done` or `error`, and plain lines
//! treated as text. The `claude_code` and `codex` profiles understand those
//! tools' JSONL event shapes; they are best effort against evolving CLIs, so
//! they degrade to plain text rather than failing the request.

use octra_llm_core::{FinishReason, GatewayError, StreamEvent, Usage};
use serde_json::{Map, Value};

/// Terminal state reported by an adapter once stdout closes.
#[derive(Debug, Clone, Default)]
pub struct AdapterFinish {
    pub usage: Usage,
    pub finish_reason: FinishReason,
    pub error: Option<GatewayError>,
    pub text: String,
    /// Whether the CLI emitted an explicit terminal event (result, turn end).
    pub terminal: bool,
}

pub trait StreamAdapter: Send {
    fn push_line(&mut self, line: &str) -> Vec<StreamEvent>;
    fn finish(&mut self) -> AdapterFinish;
}

fn cli_error(message: impl Into<String>) -> GatewayError {
    GatewayError::upstream(502, message).with_code("cli_error")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliFormat {
    Generic,
    ClaudeCode,
    Codex,
}

impl CliFormat {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "generic" | "jsonl" => Some(Self::Generic),
            "claude" | "claude_code" | "claude-code" | "stream-json" => Some(Self::ClaudeCode),
            "codex" | "codex_jsonl" | "codex-json" => Some(Self::Codex),
            _ => None,
        }
    }
}

pub fn adapter_for(format: CliFormat, partial_messages: bool) -> Box<dyn StreamAdapter> {
    match format {
        CliFormat::Generic => Box::new(GenericAdapter::default()),
        CliFormat::ClaudeCode => Box::new(ClaudeAdapter {
            partial: partial_messages,
            ..ClaudeAdapter::default()
        }),
        CliFormat::Codex => Box::new(CodexAdapter::default()),
    }
}

fn parse_usage(value: &Value) -> Option<Usage> {
    if !value.is_object() {
        return None;
    }
    let number = |keys: &[&str]| -> Option<u64> {
        keys.iter()
            .find_map(|key| value.get(*key).and_then(Value::as_u64))
    };
    Some(Usage {
        input_tokens: number(&["input_tokens", "prompt_tokens"]).unwrap_or(0),
        output_tokens: number(&["output_tokens", "completion_tokens"]).unwrap_or(0),
        cache_read_tokens: number(&["cache_read_input_tokens", "cached_input_tokens"]),
        cache_write_tokens: number(&["cache_creation_input_tokens"]),
        reasoning_tokens: number(&["reasoning_tokens"]),
    })
}

fn finish_reason(value: Option<&Value>) -> FinishReason {
    match value.and_then(Value::as_str) {
        Some("length") | Some("max_tokens") => FinishReason::Length,
        Some("tool_calls") | Some("tool_use") => FinishReason::ToolCalls,
        Some("content_filter") => FinishReason::ContentFilter,
        Some("refusal") => FinishReason::Refusal,
        Some("stop") | None => FinishReason::Stop,
        Some(_) => FinishReason::Other,
    }
}

fn error_message(value: &Map<String, Value>) -> String {
    if let Some(message) = value.get("message").and_then(Value::as_str) {
        return message.to_string();
    }
    if let Some(error) = value.get("error") {
        if let Some(message) = error.as_str() {
            return message.to_string();
        }
        if let Some(message) = error.get("message").and_then(Value::as_str) {
            return message.to_string();
        }
    }
    "CLI reported an error".to_string()
}

// ---------------------------------------------------------------------------
// Generic JSONL

#[derive(Default)]
struct GenericAdapter {
    text: String,
    usage: Usage,
    finish: FinishReason,
    terminal: bool,
}

impl StreamAdapter for GenericAdapter {
    fn push_line(&mut self, line: &str) -> Vec<StreamEvent> {
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() {
            return Vec::new();
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            self.text.push_str(line);
            self.text.push('\n');
            return vec![StreamEvent::TextDelta {
                text: format!("{line}\n"),
            }];
        };
        let Some(object) = value.as_object() else {
            return Vec::new();
        };
        match object.get("type").and_then(Value::as_str).unwrap_or("") {
            "text" | "text_delta" | "content" => object
                .get("text")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .map(|text| {
                    self.text.push_str(text);
                    vec![StreamEvent::TextDelta {
                        text: text.to_string(),
                    }]
                })
                .unwrap_or_default(),
            "reasoning" | "reasoning_delta" => object
                .get("text")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .map(|text| {
                    vec![StreamEvent::ReasoningDelta {
                        text: text.to_string(),
                    }]
                })
                .unwrap_or_default(),
            "usage" => match parse_usage(&value) {
                Some(usage) => {
                    self.usage = usage.clone();
                    vec![StreamEvent::Usage(usage)]
                }
                None => Vec::new(),
            },
            "done" | "finish" | "result" => {
                self.terminal = true;
                self.finish =
                    finish_reason(object.get("finish_reason").or_else(|| object.get("reason")));
                Vec::new()
            }
            "error" => {
                self.terminal = true;
                vec![StreamEvent::Error(cli_error(error_message(object)))]
            }
            _ => object
                .get("text")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .map(|text| {
                    self.text.push_str(text);
                    vec![StreamEvent::TextDelta {
                        text: text.to_string(),
                    }]
                })
                .unwrap_or_default(),
        }
    }

    fn finish(&mut self) -> AdapterFinish {
        AdapterFinish {
            usage: self.usage.clone(),
            finish_reason: self.finish,
            error: None,
            text: std::mem::take(&mut self.text),
            terminal: self.terminal,
        }
    }
}

// ---------------------------------------------------------------------------
// Claude Code (`--output-format stream-json [--include-partial-messages]`)

#[derive(Default)]
struct ClaudeAdapter {
    text: String,
    usage: Usage,
    finish: FinishReason,
    error: Option<GatewayError>,
    terminal: bool,
    partial: bool,
}

impl ClaudeAdapter {
    fn push_content(&mut self, content: Option<&Value>) -> Vec<StreamEvent> {
        let Some(blocks) = content.and_then(Value::as_array) else {
            return Vec::new();
        };
        let mut events = Vec::new();
        for block in blocks {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if self.partial {
                        continue;
                    }
                    if let Some(text) = block.get("text").and_then(Value::as_str)
                        && !text.is_empty()
                    {
                        self.text.push_str(text);
                        events.push(StreamEvent::TextDelta {
                            text: text.to_string(),
                        });
                    }
                }
                Some("thinking") => {
                    if self.partial {
                        continue;
                    }
                    if let Some(text) = block.get("thinking").and_then(Value::as_str)
                        && !text.is_empty()
                    {
                        events.push(StreamEvent::ReasoningDelta {
                            text: text.to_string(),
                        });
                    }
                }
                _ => {}
            }
        }
        events
    }

    fn push_stream_event(&mut self, event: &Value) -> Vec<StreamEvent> {
        let Some(object) = event.as_object() else {
            return Vec::new();
        };
        match object.get("type").and_then(Value::as_str).unwrap_or("") {
            "content_block_delta" => {
                let Some(delta) = object.get("delta").and_then(Value::as_object) else {
                    return Vec::new();
                };
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => delta
                        .get("text")
                        .and_then(Value::as_str)
                        .filter(|text| !text.is_empty())
                        .map(|text| {
                            self.text.push_str(text);
                            vec![StreamEvent::TextDelta {
                                text: text.to_string(),
                            }]
                        })
                        .unwrap_or_default(),
                    Some("thinking_delta") => delta
                        .get("thinking")
                        .and_then(Value::as_str)
                        .filter(|text| !text.is_empty())
                        .map(|text| {
                            vec![StreamEvent::ReasoningDelta {
                                text: text.to_string(),
                            }]
                        })
                        .unwrap_or_default(),
                    _ => Vec::new(),
                }
            }
            _ => Vec::new(),
        }
    }
}

impl StreamAdapter for ClaudeAdapter {
    fn push_line(&mut self, line: &str) -> Vec<StreamEvent> {
        let line = line.trim();
        if line.is_empty() {
            return Vec::new();
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            return Vec::new();
        };
        let Some(object) = value.as_object() else {
            return Vec::new();
        };
        match object.get("type").and_then(Value::as_str).unwrap_or("") {
            "assistant" => self.push_content(object.get("message").and_then(|m| m.get("content"))),
            "stream_event" => object
                .get("event")
                .map(|event| self.push_stream_event(event))
                .unwrap_or_default(),
            "result" => {
                self.terminal = true;
                if let Some(usage) = object.get("usage").and_then(parse_usage) {
                    self.usage = usage;
                }
                let subtype = object.get("subtype").and_then(Value::as_str).unwrap_or("");
                let failed = object
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                    || subtype.starts_with("error");
                if failed {
                    let message = object
                        .get("result")
                        .and_then(Value::as_str)
                        .filter(|text| !text.is_empty())
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("Claude Code failed ({subtype})"));
                    self.error = Some(cli_error(message));
                } else if subtype == "error_max_turns" {
                    self.finish = FinishReason::Length;
                }
                if !self.partial
                    && self.text.is_empty()
                    && let Some(text) = object.get("result").and_then(Value::as_str)
                    && !text.is_empty()
                    && self.error.is_none()
                {
                    self.text.push_str(text);
                    return vec![StreamEvent::TextDelta {
                        text: text.to_string(),
                    }];
                }
                Vec::new()
            }
            "error" => {
                self.terminal = true;
                let error = cli_error(error_message(object));
                self.error = Some(error.clone());
                vec![StreamEvent::Error(error)]
            }
            _ => Vec::new(),
        }
    }

    fn finish(&mut self) -> AdapterFinish {
        AdapterFinish {
            usage: self.usage.clone(),
            finish_reason: self.finish,
            error: self.error.clone(),
            text: std::mem::take(&mut self.text),
            terminal: self.terminal,
        }
    }
}

// ---------------------------------------------------------------------------
// Codex (`codex exec --json`)

#[derive(Default)]
struct CodexAdapter {
    text: String,
    reasoning: String,
    usage: Usage,
    error: Option<GatewayError>,
    terminal: bool,
}

impl CodexAdapter {
    fn emit_progressive(&mut self, reasoning: bool, full: &str) -> Option<String> {
        let accumulated = if reasoning {
            &mut self.reasoning
        } else {
            &mut self.text
        };
        if full.is_empty() || full == accumulated {
            return None;
        }
        if full.starts_with(accumulated.as_str()) {
            let delta = full[accumulated.len()..].to_string();
            *accumulated = full.to_string();
            (!delta.is_empty()).then_some(delta)
        } else {
            // Not a cumulative update: a fresh message, emitted whole.
            accumulated.push_str(full);
            Some(full.to_string())
        }
    }

    fn text_of(item: &Value) -> Option<&str> {
        item.get("text")
            .and_then(Value::as_str)
            .or_else(|| item.get("content").and_then(Value::as_str))
            .or_else(|| item.get("message").and_then(Value::as_str))
    }

    fn push_item(&mut self, item: &Value) -> Vec<StreamEvent> {
        match item.get("type").and_then(Value::as_str).unwrap_or("") {
            "agent_message" | "assistant_message" | "message" => {
                match Self::text_of(item).and_then(|text| self.emit_progressive(false, text)) {
                    Some(text) => vec![StreamEvent::TextDelta { text }],
                    None => Vec::new(),
                }
            }
            "reasoning" => {
                match Self::text_of(item).and_then(|text| self.emit_progressive(true, text)) {
                    Some(text) => vec![StreamEvent::ReasoningDelta { text }],
                    None => Vec::new(),
                }
            }
            _ => Vec::new(),
        }
    }
}

impl StreamAdapter for CodexAdapter {
    fn push_line(&mut self, line: &str) -> Vec<StreamEvent> {
        let line = line.trim();
        if line.is_empty() {
            return Vec::new();
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            return Vec::new();
        };
        let Some(object) = value.as_object() else {
            return Vec::new();
        };
        let object = object
            .get("msg")
            .and_then(Value::as_object)
            .unwrap_or(object);
        match object.get("type").and_then(Value::as_str).unwrap_or("") {
            "item.started" | "item.updated" | "item.completed" => object
                .get("item")
                .map(|item| self.push_item(item))
                .unwrap_or_default(),
            "agent_message" | "assistant_message" | "message" => object
                .get("text")
                .and_then(Value::as_str)
                .and_then(|text| self.emit_progressive(false, text))
                .map(|text| vec![StreamEvent::TextDelta { text }])
                .unwrap_or_default(),
            "turn.completed" => {
                self.terminal = true;
                if let Some(usage) = object.get("usage").and_then(parse_usage) {
                    self.usage = usage.clone();
                    vec![StreamEvent::Usage(usage)]
                } else {
                    Vec::new()
                }
            }
            "turn.failed" | "error" => {
                self.terminal = true;
                let error = cli_error(error_message(object));
                self.error = Some(error.clone());
                vec![StreamEvent::Error(error)]
            }
            _ => Vec::new(),
        }
    }

    fn finish(&mut self) -> AdapterFinish {
        AdapterFinish {
            usage: self.usage.clone(),
            finish_reason: FinishReason::Stop,
            error: self.error.clone(),
            text: std::mem::take(&mut self.text),
            terminal: self.terminal,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(events: &[StreamEvent]) -> String {
        let mut out = String::new();
        for event in events {
            if let StreamEvent::TextDelta { text } = event {
                out.push_str(text);
            }
        }
        out
    }

    #[test]
    fn generic_maps_jsonl_and_plain_lines() {
        let mut adapter = adapter_for(CliFormat::Generic, false);
        let mut events = Vec::new();
        events.extend(adapter.push_line(r#"{"type":"text_delta","text":"Hello"}"#));
        events.extend(adapter.push_line("plain tail"));
        events.extend(adapter.push_line(r#"{"type":"usage","input_tokens":3,"output_tokens":4}"#));
        events.extend(adapter.push_line(r#"{"type":"done","finish_reason":"length"}"#));
        assert_eq!(text_of(&events), "Helloplain tail\n");
        assert_eq!(
            events
                .iter()
                .find(|event| matches!(event, StreamEvent::Usage(_)))
                .cloned(),
            Some(StreamEvent::Usage(Usage {
                input_tokens: 3,
                output_tokens: 4,
                ..Usage::default()
            }))
        );
        let finish = adapter.finish();
        assert_eq!(finish.finish_reason, FinishReason::Length);
        assert!(finish.terminal);
        assert_eq!(finish.text, "Helloplain tail\n");
    }

    #[test]
    fn generic_maps_errors() {
        let mut adapter = adapter_for(CliFormat::Generic, false);
        let events = adapter.push_line(r#"{"type":"error","message":"boom"}"#);
        assert!(matches!(&events[0], StreamEvent::Error(error) if error.message.contains("boom")));
    }

    #[test]
    fn claude_uses_assistant_text_and_result_usage() {
        let mut adapter = adapter_for(CliFormat::ClaudeCode, false);
        let mut events = Vec::new();
        events.extend(adapter.push_line(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Hi"}]}}"#,
        ));
        events.extend(adapter.push_line(
            r#"{"type":"result","subtype":"success","is_error":false,"result":"Hi","usage":{"input_tokens":5,"output_tokens":2,"cache_read_input_tokens":1}}"#,
        ));
        assert_eq!(text_of(&events), "Hi");
        let finish = adapter.finish();
        assert_eq!(finish.text, "Hi");
        assert_eq!(finish.usage.input_tokens, 5);
        assert_eq!(finish.usage.output_tokens, 2);
        assert_eq!(finish.usage.cache_read_tokens, Some(1));
        assert!(finish.error.is_none());
    }

    #[test]
    fn claude_result_falls_back_to_final_text() {
        let mut adapter = adapter_for(CliFormat::ClaudeCode, false);
        let events = adapter
            .push_line(r#"{"type":"result","subtype":"success","is_error":false,"result":"Done"}"#);
        assert_eq!(text_of(&events), "Done");
    }

    #[test]
    fn claude_partial_events_win_over_assistant_echoes() {
        let mut adapter = adapter_for(CliFormat::ClaudeCode, true);
        let mut events = Vec::new();
        events.extend(adapter.push_line(
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"A"}}}"#,
        ));
        events.extend(adapter.push_line(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"AB"}]}}"#,
        ));
        events.extend(adapter.push_line(
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"B"}}}"#,
        ));
        assert_eq!(text_of(&events), "AB");
        assert_eq!(adapter.finish().text, "AB");
    }

    #[test]
    fn claude_errors_surface() {
        let mut adapter = adapter_for(CliFormat::ClaudeCode, false);
        let events = adapter.push_line(
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"bad"}"#,
        );
        assert!(events.is_empty());
        let finish = adapter.finish();
        assert!(
            finish
                .error
                .as_ref()
                .is_some_and(|error| error.message == "bad")
        );
    }

    #[test]
    fn codex_unwraps_msg_and_cumulative_updates() {
        let mut adapter = adapter_for(CliFormat::Codex, false);
        let mut events = Vec::new();
        events.extend(adapter.push_line(
            r#"{"msg":{"type":"item.updated","item":{"type":"agent_message","text":"Hel"}}}"#,
        ));
        events.extend(adapter.push_line(
            r#"{"msg":{"type":"item.completed","item":{"type":"agent_message","text":"Hello"}}}"#,
        ));
        events.extend(adapter.push_line(
            r#"{"msg":{"type":"turn.completed","usage":{"input_tokens":7,"output_tokens":3}}}"#,
        ));
        assert_eq!(text_of(&events), "Hello");
        let finish = adapter.finish();
        assert_eq!(finish.text, "Hello");
        assert_eq!(finish.usage.input_tokens, 7);
        assert!(finish.terminal);
    }
}
