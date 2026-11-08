//! Canonical chat IR.
//!
//! The model is deliberately richer than any single wire format: blocks keep
//! their order, tool calls carry provider-specific ids, reasoning is a separate
//! block instead of being folded into text, and unknown fields survive in
//! `extra` so a same-protocol round trip never silently drops options.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::protocol::GatewayError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    Developer,
    User,
    Assistant,
    Tool,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::System => "system",
            Role::Developer => "developer",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }
}

/// Ordered content of a user message or tool result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Map::is_empty")]
        extra: Map<String, Value>,
    },
    Image(Attachment),
    File(Attachment),
    /// Inline reasoning shown by some providers inside tool/result content.
    Reasoning {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
        #[serde(default, skip_serializing_if = "Map::is_empty")]
        extra: Map<String, Value>,
    },
    /// Content the IR cannot render; adapters may drop or stringify it.
    Unsupported {
        kind: String,
        raw: Value,
    },
}

impl ContentPart {
    pub fn text(text: impl Into<String>) -> Self {
        ContentPart::Text {
            text: text.into(),
            extra: Map::new(),
        }
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            ContentPart::Text { text, .. } => Some(text),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Attachment {
    /// Media type such as `image/png` or `application/pdf`.
    pub media_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<AttachmentData>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AttachmentData {
    /// Raw base64 payload without a data URL prefix.
    Base64 { data: String },
}

/// Non-empty ordered user content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UserContent {
    pub parts: Vec<ContentPart>,
}

impl UserContent {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            parts: vec![ContentPart::text(text)],
        }
    }

    pub fn from_parts(parts: Vec<ContentPart>) -> Self {
        Self { parts }
    }

    pub fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }

    /// Returns the content as a single string when it is a lone text block.
    pub fn as_text(&self) -> Option<&str> {
        match self.parts.as_slice() {
            [ContentPart::Text { text, .. }] => Some(text),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reasoning {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// Encrypted reasoning blobs (for example Anthropic `redacted_thinking`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub encrypted: Vec<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AssistantContent {
    Text {
        text: String,
    },
    Reasoning {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: Value,
        #[serde(default, skip_serializing_if = "is_function_kind")]
        kind: Option<ToolCallKind>,
    },
    /// Anthropic-style provider-executed tool results (web search, code
    /// execution, ...). OpenAI adapters cannot represent these.
    ToolResult {
        tool_use_id: String,
        content: Value,
        #[serde(default)]
        is_error: bool,
    },
    Refusal {
        text: String,
    },
    /// Opaque provider block kept for same-protocol round trips.
    Raw {
        kind: String,
        raw: Value,
    },
}

fn is_function_kind(kind: &Option<ToolCallKind>) -> bool {
    matches!(kind, None | Some(ToolCallKind::Function))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub tool_call_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub content: UserContent,
    #[serde(default)]
    pub is_error: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<UserContent>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assistant: Vec<AssistantContent>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_results: Vec<ToolResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Self::user_parts(vec![ContentPart::text(text)])
    }

    pub fn user_parts(parts: Vec<ContentPart>) -> Self {
        Self {
            role: Role::User,
            content: Some(UserContent::from_parts(parts)),
            assistant: Vec::new(),
            tool_results: Vec::new(),
            name: None,
            extra: Map::new(),
        }
    }

    pub fn new(role: Role) -> Self {
        Self {
            role,
            content: None,
            assistant: Vec::new(),
            tool_results: Vec::new(),
            name: None,
            extra: Map::new(),
        }
    }
}

/// System prompt as an ordered list of text parts. OpenAI accepts a single
/// string, Anthropic accepts an array; both are represented faithfully here.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SystemPrompt {
    pub blocks: Vec<String>,
}

impl SystemPrompt {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            blocks: vec![text.into()],
        }
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolCallKind {
    Function,
    /// Server-executed tools (`web_search`, `computer`, Anthropic built-ins).
    Provider,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FunctionDefinition {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tool {
    #[serde(default = "default_tool_kind")]
    pub kind: ToolCallKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function: Option<FunctionDefinition>,
    /// Type name for provider tools, e.g. `web_search_20250305`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_config: Option<Value>,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

fn default_tool_kind() -> ToolCallKind {
    ToolCallKind::Function
}

impl Tool {
    pub fn function(function: FunctionDefinition) -> Self {
        Self {
            kind: ToolCallKind::Function,
            name: None,
            function: Some(function),
            provider_type: None,
            provider_config: None,
            extra: Map::new(),
        }
    }

    /// Name used when rendering the tool to protocols without provider tools.
    pub fn function_name(&self) -> Option<&str> {
        self.name
            .as_deref()
            .or_else(|| self.function.as_ref().map(|f| f.name.as_str()))
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    Stop,
    Length,
    ToolCalls,
    ContentFilter,
    Refusal,
    Pause,
    #[default]
    Other,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
}

impl Usage {
    pub fn is_empty(&self) -> bool {
        self.input_tokens == 0 && self.output_tokens == 0
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: Value,
    /// Parse error when the upstream produced invalid JSON arguments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Canonical tool call id. OpenAI calls are `call_*` (or client-provided),
    /// Anthropic `tool_use` ids are `toolu_*`.
    pub id: String,
    /// Id used when forwarding to an Anthropic upstream. Export-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anthropic_id: Option<String>,
    pub kind: ToolCallKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function: Option<FunctionCall>,
    /// For provider tools: the tool type, e.g. `web_search`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_tool_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_input: Option<Value>,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

impl ToolCall {
    pub fn id_for(&self, protocol: crate::protocol::Protocol) -> &str {
        match protocol {
            crate::protocol::Protocol::Anthropic => {
                self.anthropic_id.as_deref().unwrap_or(&self.id)
            }
            crate::protocol::Protocol::Openai
            | crate::protocol::Protocol::OpenaiResponses
            | crate::protocol::Protocol::Gemini
            | crate::protocol::Protocol::Vertex
            | crate::protocol::Protocol::Bedrock => &self.id,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReasoningSummary {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProgrammaticOutput {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub json: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatRequest {
    pub model: String,
    #[serde(default, skip_serializing_if = "SystemPrompt::is_empty")]
    pub system: SystemPrompt,
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Tool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    /// Ask the upstream to include token usage in the final stream event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_usage: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_format: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Unknown request fields, kept for same-protocol forwarding.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

impl ChatRequest {
    pub fn new(model: impl Into<String>, messages: Vec<Message>) -> Self {
        Self {
            model: model.into(),
            system: SystemPrompt::default(),
            messages,
            tools: Vec::new(),
            tool_choice: None,
            parallel_tool_calls: None,
            temperature: None,
            top_p: None,
            top_k: None,
            max_tokens: None,
            stop: Vec::new(),
            stream: None,
            include_usage: None,
            response_format: None,
            reasoning_effort: None,
            user: None,
            extra: Map::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatResponse {
    pub id: String,
    pub model: String,
    pub message: Message,
    pub finish_reason: FinishReason,
    pub usage: Usage,
    pub reasoning: Option<Reasoning>,
    pub programmatic: Option<ProgrammaticOutput>,
    pub extra: Map<String, Value>,
}

impl ChatResponse {
    pub fn new(id: impl Into<String>, model: impl Into<String>, message: Message) -> Self {
        Self {
            id: id.into(),
            model: model.into(),
            message,
            finish_reason: FinishReason::Stop,
            usage: Usage::default(),
            reasoning: None,
            programmatic: None,
            extra: Map::new(),
        }
    }

    /// Concatenated assistant text.
    pub fn text(&self) -> String {
        let mut out = String::new();
        for block in &self.message.assistant {
            if let AssistantContent::Text { text } = block {
                out.push_str(text);
            }
        }
        out
    }
}

/// Provider-neutral streaming event. Adapters translate upstream deltas into
/// these and re-encode them for the ingress protocol.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    /// Response identity, emitted once before any content.
    MessageStart {
        id: String,
        model: String,
    },
    TextDelta {
        text: String,
    },
    /// Reasoning text delta (Anthropic `thinking`, OpenAI reasoning content).
    ReasoningDelta {
        text: String,
    },
    ReasoningSignatureDelta {
        signature: String,
    },
    /// A new tool call begins.
    ToolCallStart {
        index: usize,
        id: String,
        name: String,
        kind: ToolCallKind,
    },
    /// JSON fragment of a tool call's arguments.
    ToolCallDelta {
        index: usize,
        arguments: String,
    },
    /// Provider-executed tool metadata streamed before the result.
    ProviderToolStart {
        index: usize,
        tool_type: String,
    },
    /// Provider-executed tool result (web search, ...).
    ProviderToolResult {
        index: usize,
        tool_use_id: String,
        content: Value,
        is_error: bool,
    },
    /// Final usage numbers.
    Usage(Usage),
    Finish {
        reason: FinishReason,
    },
    /// An error mid-stream; the stream terminates after this event.
    Error(GatewayError),
}

/// Status code used when an upstream error is forwarded to the client.
pub fn clamp_upstream_status(status: u16) -> u16 {
    match status {
        400..=599 => status,
        _ => 502,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_content_round_trips_through_json() {
        let content = UserContent::from_parts(vec![
            ContentPart::text("hello"),
            ContentPart::Image(Attachment {
                media_type: "image/png".into(),
                data: Some(AttachmentData::Base64 {
                    data: "aGVsbG8=".into(),
                }),
                url: None,
                name: None,
                extra: Map::new(),
            }),
        ]);
        let json = serde_json::to_value(&content).unwrap();
        assert_eq!(json[0]["type"], "text");
        assert_eq!(json[1]["type"], "image");
        assert_eq!(json[1]["media_type"], "image/png");
        let back: UserContent = serde_json::from_value(json).unwrap();
        assert_eq!(back, content);
    }

    #[test]
    fn finish_reason_and_usage_defaults() {
        assert_eq!(FinishReason::default(), FinishReason::Other);
        assert!(Usage::default().is_empty());
        let used = Usage {
            input_tokens: 3,
            ..Usage::default()
        };
        assert!(!used.is_empty());
    }

    #[test]
    fn tool_call_prefers_anthropic_id() {
        let call = ToolCall {
            id: "call_1".into(),
            anthropic_id: Some("toolu_1".into()),
            kind: ToolCallKind::Function,
            function: Some(FunctionCall {
                name: "lookup".into(),
                arguments: serde_json::json!({}),
                arguments_error: None,
            }),
            provider_tool_type: None,
            provider_input: None,
            extra: Map::new(),
        };
        assert_eq!(call.id_for(crate::protocol::Protocol::Anthropic), "toolu_1");
        assert_eq!(call.id_for(crate::protocol::Protocol::Openai), "call_1");
        assert_eq!(call.id_for(crate::protocol::Protocol::Vertex), "call_1");
        assert_eq!(call.id_for(crate::protocol::Protocol::Bedrock), "call_1");
    }

    #[test]
    fn retryable_error_kinds() {
        assert!(crate::ErrorKind::Upstream.is_retryable());
        assert!(!crate::ErrorKind::InvalidRequest.is_retryable());
    }

    #[test]
    fn reasoning_block_round_trips() {
        let message = Message {
            role: Role::Assistant,
            content: None,
            assistant: vec![
                AssistantContent::Reasoning {
                    text: "think".into(),
                    signature: Some("sig".into()),
                },
                AssistantContent::ToolCall {
                    id: "call_1".into(),
                    name: "lookup".into(),
                    arguments: serde_json::json!({"q": "x"}),
                    kind: None,
                },
                AssistantContent::Text {
                    text: "done".into(),
                },
            ],
            tool_results: Vec::new(),
            name: None,
            extra: Map::new(),
        };
        let json = serde_json::to_value(&message).unwrap();
        assert_eq!(json["assistant"][0]["type"], "reasoning");
        assert_eq!(json["assistant"][1]["type"], "tool_call");
        let back: Message = serde_json::from_value(json).unwrap();
        assert_eq!(back, message);
    }
}
