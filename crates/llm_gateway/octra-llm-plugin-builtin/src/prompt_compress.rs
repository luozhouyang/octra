//! Deterministic prompt compression for the canonical request.
//!
//! The compressor rewrites system prompt blocks, user text content parts and
//! assistant text blocks before the request leaves the gateway. It performs
//! only deterministic transforms: whitespace normalization, adjacent
//! line/paragraph deduplication and middle-out truncation under a character
//! budget.
//!
//! Whitespace collapse treats runs of spaces and tabs uniformly, so code
//! indentation and aligned tables can be normalized as well. Tool-call
//! arguments, tool results, reasoning signatures and non-text parts are never
//! modified.

use std::ops::Range;
use std::sync::Arc;

use async_trait::async_trait;
use octra_api_plugin_types::{Capability, FailPolicy, PluginError, PluginManifest};
use octra_llm_core::{AssistantContent, ChatRequest, ContentPart, Role};
use octra_llm_plugin_types::{
    LlmAction, LlmPlugin, LlmPluginFactory, LlmPluginResult, LlmRequestContext, LlmScope,
    REQUEST_LLM_SCOPES,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::manifest;

const DEFAULT_MAX_CHARS: usize = 12_000;
const MIN_MAX_CHARS: usize = 256;
const MAX_MAX_CHARS: usize = 1_000_000;
const DEFAULT_MARKER: &str = "\n[...truncated...]\n";

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Config {
    max_chars: usize,
    preserve_system: bool,
    preserve_last_user: bool,
    collapse_whitespace: bool,
    dedupe_adjacent_lines: bool,
    dedupe_adjacent_paragraphs: bool,
    marker: String,
    min_field_chars: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_chars: DEFAULT_MAX_CHARS,
            preserve_system: true,
            preserve_last_user: true,
            collapse_whitespace: true,
            dedupe_adjacent_lines: true,
            dedupe_adjacent_paragraphs: true,
            marker: DEFAULT_MARKER.to_string(),
            min_field_chars: 0,
        }
    }
}

#[derive(Debug)]
pub struct PromptCompress {
    manifest: PluginManifest,
    max_chars: usize,
    preserve_system: bool,
    preserve_last_user: bool,
    collapse_whitespace: bool,
    dedupe_adjacent_lines: bool,
    dedupe_adjacent_paragraphs: bool,
    marker: String,
    min_field_chars: usize,
}

pub struct PromptCompressFactory;

/// Location of one selected text field in the canonical request.
#[derive(Debug, Clone, Copy)]
enum Field {
    System(usize),
    UserPart { message: usize, part: usize },
    AssistantText { message: usize, block: usize },
}

impl Field {
    fn text_mut<'a>(&self, request: &'a mut ChatRequest) -> Option<&'a mut String> {
        match *self {
            Field::System(index) => request.system.blocks.get_mut(index),
            Field::UserPart { message, part } => {
                let message = request.messages.get_mut(message)?;
                match message.content.as_mut()?.parts.get_mut(part)? {
                    ContentPart::Text { text, .. } => Some(text),
                    _ => None,
                }
            }
            Field::AssistantText { message, block } => {
                let message = request.messages.get_mut(message)?;
                match message.assistant.get_mut(block)? {
                    AssistantContent::Text { text } => Some(text),
                    _ => None,
                }
            }
        }
    }
}

struct SelectedField {
    location: Field,
    chars: usize,
    truncatable: bool,
}

impl PromptCompressFactory {
    fn parse(config: Value) -> Result<PromptCompress, PluginError> {
        let config: Config = serde_json::from_value(config).map_err(|error| {
            PluginError::fail_closed(format!("invalid llm-prompt-compress config: {error}"))
        })?;
        if !(MIN_MAX_CHARS..=MAX_MAX_CHARS).contains(&config.max_chars) {
            return Err(PluginError::fail_closed(format!(
                "llm-prompt-compress `max_chars` must be between {MIN_MAX_CHARS} and {MAX_MAX_CHARS}"
            )));
        }
        if config.marker.is_empty() {
            return Err(PluginError::fail_closed(
                "llm-prompt-compress `marker` must not be empty",
            ));
        }

        Ok(PromptCompress {
            manifest: Self::make_manifest(),
            max_chars: config.max_chars,
            preserve_system: config.preserve_system,
            preserve_last_user: config.preserve_last_user,
            collapse_whitespace: config.collapse_whitespace,
            dedupe_adjacent_lines: config.dedupe_adjacent_lines,
            dedupe_adjacent_paragraphs: config.dedupe_adjacent_paragraphs,
            marker: config.marker,
            min_field_chars: config.min_field_chars,
        })
    }

    fn make_manifest() -> PluginManifest {
        let mut manifest = manifest(
            "llm-prompt-compress",
            "Reduce prompt size with deterministic whitespace, duplicate and length trimming",
        );
        manifest.priority = 550;
        manifest.fail_policy = FailPolicy::Closed;
        manifest.capabilities = vec![Capability::RequestBody];
        manifest.config_schema = Some(json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "title": "llm-prompt-compress config",
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "max_chars": {
                    "type": "integer",
                    "minimum": MIN_MAX_CHARS,
                    "maximum": MAX_MAX_CHARS,
                    "default": DEFAULT_MAX_CHARS,
                    "description": "Total character budget across compressed text fields."
                },
                "preserve_system": {
                    "type": "boolean",
                    "default": true,
                    "description": "Normalize but never truncate system prompt text."
                },
                "preserve_last_user": {
                    "type": "boolean",
                    "default": true,
                    "description": "Normalize but never truncate the last user message."
                },
                "collapse_whitespace": {"type": "boolean", "default": true},
                "dedupe_adjacent_lines": {"type": "boolean", "default": true},
                "dedupe_adjacent_paragraphs": {"type": "boolean", "default": true},
                "marker": {
                    "type": "string",
                    "minLength": 1,
                    "default": DEFAULT_MARKER
                },
                "min_field_chars": {
                    "type": "integer",
                    "minimum": 0,
                    "default": 0,
                    "description": "Fields at or below this length are never truncated."
                }
            }
        }));
        manifest
    }
}

impl LlmPluginFactory for PromptCompressFactory {
    fn manifest(&self) -> PluginManifest {
        Self::make_manifest()
    }

    fn supported_scopes(&self) -> &'static [LlmScope] {
        &REQUEST_LLM_SCOPES
    }

    fn create(&self, config: Value) -> Result<Arc<dyn LlmPlugin>, PluginError> {
        Ok(Arc::new(Self::parse(config)?))
    }
}

impl PromptCompress {
    fn normalize(&self, text: &mut String) {
        if self.collapse_whitespace {
            *text = collapse_whitespace(text);
        }
        if self.dedupe_adjacent_lines {
            *text = dedupe_adjacent_lines(text);
        }
        if self.dedupe_adjacent_paragraphs {
            *text = dedupe_adjacent_paragraphs(text);
        }
    }

    fn apply(&self, request: &mut ChatRequest) {
        let last_user = request
            .messages
            .iter()
            .rposition(|message| message.role == Role::User);
        let mut fields = Vec::new();

        for (index, block) in request.system.blocks.iter_mut().enumerate() {
            self.normalize(block);
            fields.push(SelectedField {
                location: Field::System(index),
                chars: char_count(block),
                truncatable: !self.preserve_system,
            });
        }

        for (message_index, message) in request.messages.iter_mut().enumerate() {
            if message.role == Role::User
                && let Some(content) = &mut message.content
            {
                for (part_index, part) in content.parts.iter_mut().enumerate() {
                    let ContentPart::Text { text, .. } = part else {
                        continue;
                    };
                    self.normalize(text);
                    fields.push(SelectedField {
                        location: Field::UserPart {
                            message: message_index,
                            part: part_index,
                        },
                        chars: char_count(text),
                        truncatable: !(self.preserve_last_user && last_user == Some(message_index)),
                    });
                }
            }

            if message.role == Role::Assistant {
                for (block_index, block) in message.assistant.iter_mut().enumerate() {
                    let AssistantContent::Text { text } = block else {
                        continue;
                    };
                    self.normalize(text);
                    fields.push(SelectedField {
                        location: Field::AssistantText {
                            message: message_index,
                            block: block_index,
                        },
                        chars: char_count(text),
                        truncatable: true,
                    });
                }
            }
        }

        self.truncate(request, &fields);
    }

    fn truncate(&self, request: &mut ChatRequest, fields: &[SelectedField]) {
        let mut total: usize = fields
            .iter()
            .map(|field| field.chars)
            .fold(0usize, usize::saturating_add);
        if total <= self.max_chars {
            return;
        }

        let marker_len = char_count(&self.marker);
        for field in fields {
            if total <= self.max_chars {
                break;
            }
            if !field.truncatable || field.chars <= self.min_field_chars {
                continue;
            }
            let Some(text) = field.location.text_mut(request) else {
                continue;
            };
            let len = char_count(text);
            if len <= self.min_field_chars {
                continue;
            }

            let excess = total - self.max_chars;
            let max_reduction = len - self.min_field_chars;
            let reduction = excess.min(max_reduction);
            // Remove `reduction` characters net, reserving room for the marker.
            let keep = len.saturating_sub(reduction.saturating_add(marker_len));
            if keep >= len {
                // The marker is longer than the field, so truncating it would
                // not reduce the total.
                continue;
            }
            let truncated = truncate_middle(text, keep, &self.marker);
            let new_len = char_count(&truncated);
            if new_len >= len {
                continue;
            }
            *text = truncated;
            total -= len - new_len;
        }
    }
}

#[async_trait]
impl LlmPlugin for PromptCompress {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    async fn on_request(&self, cx: &mut LlmRequestContext<'_>) -> LlmPluginResult {
        self.apply(cx.request);
        Ok(LlmAction::Continue)
    }
}

fn char_count(text: &str) -> usize {
    text.chars().count()
}

fn collapse_whitespace(text: &str) -> String {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut out = String::with_capacity(normalized.len());
    for (index, line) in normalized.split('\n').enumerate() {
        if index > 0 {
            out.push('\n');
        }
        let line = line.trim_end_matches([' ', '\t']);
        let mut in_whitespace = false;
        for ch in line.chars() {
            if ch == ' ' || ch == '\t' {
                if !in_whitespace {
                    out.push(' ');
                    in_whitespace = true;
                }
            } else {
                out.push(ch);
                in_whitespace = false;
            }
        }
    }
    collapse_blank_lines(&out)
}

fn collapse_blank_lines(text: &str) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let trailing_empties = lines
        .iter()
        .rev()
        .take_while(|line| line.is_empty())
        .count();
    // A trailing newline terminates the previous line instead of starting a
    // blank one, so keep room for the terminator in the trailing run.
    let max_trailing = if text.ends_with('\n') { 3 } else { 2 };
    let mut kept: Vec<&str> = Vec::with_capacity(lines.len());
    let mut blank_run = 0usize;
    for (index, line) in lines.iter().enumerate() {
        if line.is_empty() {
            blank_run += 1;
            let trailing = trailing_empties > 0 && index >= lines.len() - trailing_empties;
            let limit = if trailing { max_trailing } else { 2 };
            if blank_run > limit {
                continue;
            }
        } else {
            blank_run = 0;
        }
        kept.push(line);
    }
    kept.join("\n")
}

fn dedupe_adjacent_lines(text: &str) -> String {
    let mut kept: Vec<&str> = Vec::new();
    for line in text.split('\n') {
        if !line.is_empty() && kept.last() == Some(&line) {
            continue;
        }
        kept.push(line);
    }
    kept.join("\n")
}

/// Removes an immediately repeated blank-line separated paragraph.
///
/// Removal starts at the end of the preceding paragraph, so the duplicate and
/// the separator in front of it are dropped together. All other bytes,
/// including the separator before the following paragraph, are preserved.
fn dedupe_adjacent_paragraphs(text: &str) -> String {
    let mut ranges: Vec<Range<usize>> = Vec::new();
    let mut previous: Option<String> = None;
    let mut previous_end: Option<usize> = None;
    let mut block_start: Option<usize> = None;
    let mut block_end = 0usize;
    let mut offset = 0usize;

    for line in text.split('\n') {
        let line_start = offset;
        let line_end = line_start + line.len();
        offset = line_end + 1;
        if line.is_empty() {
            if let Some(start) = block_start.take() {
                let content = &text[start..block_end];
                if previous.as_deref() == Some(content)
                    && let Some(end) = previous_end
                {
                    ranges.push(end..block_end);
                }
                previous = Some(content.to_string());
                previous_end = Some(block_end);
            }
        } else {
            if block_start.is_none() {
                block_start = Some(line_start);
            }
            block_end = line_end;
        }
    }

    if let Some(start) = block_start {
        let content = &text[start..block_end];
        if previous.as_deref() == Some(content)
            && let Some(end) = previous_end
        {
            ranges.push(end..block_end);
        }
    }

    if ranges.is_empty() {
        return text.to_string();
    }

    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    for range in ranges {
        if range.start > cursor {
            out.push_str(&text[cursor..range.start]);
        }
        cursor = range.end;
    }
    out.push_str(&text[cursor..]);
    out
}

/// Keeps the first and last halves of `keep` characters and joins them with
/// `marker`.
fn truncate_middle(text: &str, keep: usize, marker: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    if keep >= chars.len() {
        return text.to_string();
    }
    let first = keep.div_ceil(2);
    let last = keep - first;
    let mut out = String::with_capacity(text.len() + marker.len());
    out.extend(chars[..first].iter());
    out.push_str(marker);
    out.extend(chars[chars.len() - last..].iter());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use octra_llm_core::{Message, Protocol, SystemPrompt, ToolResult, UserContent};
    use octra_llm_plugin_types::{LlmRequestMeta, LlmResponseHeaders};

    fn meta() -> LlmRequestMeta<'static> {
        LlmRequestMeta {
            request_id: "request-1",
            ingress: Protocol::Openai,
            consumer_id: None,
            virtual_key_id: None,
            virtual_key_family: None,
            consumer_group_ids: &[],
            session_id: None,
            client_ip: None,
            user_agent: None,
            intent: None,
        }
    }

    async fn apply(config: Value, request: &mut ChatRequest) {
        let plugin = PromptCompressFactory.create(config).unwrap();
        let meta = meta();
        let mut state = Value::Null;
        let mut cx = LlmRequestContext {
            meta: &meta,
            response_headers: &mut LlmResponseHeaders::default(),
            request,
            state: &mut state,
        };
        let action = plugin.on_request(&mut cx).await.unwrap();
        assert!(matches!(action, LlmAction::Continue));
    }

    fn user_text(request: &ChatRequest, message: usize) -> &str {
        request.messages[message].content.as_ref().unwrap().parts[0]
            .as_text()
            .unwrap()
    }

    #[tokio::test]
    async fn collapses_whitespace_and_blank_lines() {
        let mut request = ChatRequest::new(
            "m",
            vec![Message::user(
                "alpha   beta\t\tgamma   \r\nsecond\rline\n\n\n\nlast",
            )],
        );
        apply(json!({}), &mut request).await;
        assert_eq!(
            user_text(&request, 0),
            "alpha beta gamma\nsecond\nline\n\n\nlast"
        );
    }

    #[test]
    fn collapses_trailing_blank_lines_without_dropping_the_terminator() {
        assert_eq!(collapse_blank_lines("a\n\n\n\n"), "a\n\n\n");
        assert_eq!(collapse_blank_lines("a\n\n\n"), "a\n\n\n");
    }

    #[tokio::test]
    async fn dedupes_adjacent_lines() {
        let mut request =
            ChatRequest::new("m", vec![Message::user("alpha\nalpha\nbeta\ngamma\ngamma")]);
        apply(
            json!({
                "collapse_whitespace": false,
                "dedupe_adjacent_paragraphs": false
            }),
            &mut request,
        )
        .await;
        assert_eq!(user_text(&request, 0), "alpha\nbeta\ngamma");
    }

    #[tokio::test]
    async fn dedupes_adjacent_paragraphs() {
        let mut request = ChatRequest::new(
            "m",
            vec![Message::user("first\nline\n\nsecond\n\nsecond\n\nthird")],
        );
        apply(
            json!({
                "collapse_whitespace": false,
                "dedupe_adjacent_lines": false
            }),
            &mut request,
        )
        .await;
        assert_eq!(user_text(&request, 0), "first\nline\n\nsecond\n\nthird");
    }

    #[tokio::test]
    async fn truncates_from_the_middle_with_the_marker() {
        let mut request = ChatRequest::new("m", vec![Message::user("a".repeat(1000))]);
        apply(
            json!({"max_chars": 256, "preserve_last_user": false}),
            &mut request,
        )
        .await;
        let text = user_text(&request, 0);
        assert_eq!(text.chars().count(), 256);
        assert_eq!(
            text,
            format!("{}{}{}", "a".repeat(119), DEFAULT_MARKER, "a".repeat(118))
        );
    }

    #[tokio::test]
    async fn preserves_the_last_user_message() {
        let first = "a".repeat(500);
        let last = "b".repeat(500);
        let mut request = ChatRequest::new("m", vec![Message::user(first), Message::user(last)]);
        apply(json!({"max_chars": 600}), &mut request).await;
        let first = user_text(&request, 0);
        let second = user_text(&request, 1);
        assert!(first.contains("[...truncated...]"));
        assert_eq!(first.chars().count(), 100);
        assert_eq!(second, "b".repeat(500));
    }

    #[tokio::test]
    async fn preserves_the_system_prompt_when_configured() {
        let system = "s".repeat(500);
        let user = "u".repeat(500);
        let mut request = ChatRequest::new("m", vec![Message::user(user)]);
        request.system = SystemPrompt::text(system.clone());
        apply(
            json!({
                "max_chars": 600,
                "preserve_system": true,
                "preserve_last_user": false
            }),
            &mut request,
        )
        .await;
        assert_eq!(request.system.blocks[0], system);
        assert_eq!(user_text(&request, 0).chars().count(), 100);
    }

    #[tokio::test]
    async fn leaves_tool_results_and_tool_call_arguments_untouched() {
        let arguments = json!({"query": "alpha   beta", "limit": 5});
        let tool_result = format!("tool   output\n{}", "x".repeat(1000));
        let assistant = Message {
            role: Role::Assistant,
            content: None,
            assistant: vec![AssistantContent::ToolCall {
                id: "call-1".to_string(),
                name: "lookup".to_string(),
                arguments: arguments.clone(),
                kind: None,
            }],
            tool_results: Vec::new(),
            name: None,
            extra: Default::default(),
        };
        let tool = Message {
            role: Role::Tool,
            content: None,
            assistant: Vec::new(),
            tool_results: vec![ToolResult {
                tool_call_id: "call-1".to_string(),
                name: Some("lookup".to_string()),
                content: UserContent::text(tool_result.clone()),
                is_error: false,
            }],
            name: None,
            extra: Default::default(),
        };
        let mut request =
            ChatRequest::new("m", vec![Message::user("a".repeat(1000)), assistant, tool]);
        apply(
            json!({"max_chars": 256, "preserve_last_user": false}),
            &mut request,
        )
        .await;

        match &request.messages[1].assistant[0] {
            AssistantContent::ToolCall {
                arguments: actual, ..
            } => assert_eq!(actual, &arguments),
            other => panic!("unexpected assistant block: {other:?}"),
        }
        assert_eq!(
            request.messages[2].tool_results[0].content.as_text(),
            Some(tool_result.as_str())
        );
        assert!(user_text(&request, 0).contains("[...truncated...]"));
    }

    #[test]
    fn rejects_invalid_configuration() {
        for config in [
            json!({"marker": ""}),
            json!({"max_chars": 255}),
            json!({"max_chars": 1_000_001}),
            json!({"max_chars": "large"}),
            json!({"unknown": true}),
        ] {
            let error = PromptCompressFactory
                .create(config)
                .err()
                .expect("invalid config must be rejected");
            assert_eq!(error.fail_policy, FailPolicy::Closed);
        }
        assert!(PromptCompressFactory.create(json!({})).is_ok());
    }

    #[tokio::test]
    async fn under_budget_text_is_only_normalized() {
        let mut request = ChatRequest::new(
            "m",
            vec![Message::user("hello   world \r\nsecond\t\tline   ")],
        );
        apply(json!({}), &mut request).await;
        assert_eq!(user_text(&request, 0), "hello world\nsecond line");
    }
}
