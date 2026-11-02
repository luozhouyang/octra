//! Canonical LLM intermediate representation shared by all protocol adapters.
//!
//! Wire protocols (OpenAI Chat Completions, Anthropic Messages, ...) are
//! translated into [`ChatRequest`] / [`ChatResponse`] / [`StreamEvent`] by the
//! adapter crates. The gateway core only ever sees this IR, so adding a
//! protocol never touches routing, auth or usage accounting.
//!
//! Fields that have no canonical meaning are preserved in `extra` maps so a
//! request can round-trip vendor-specific options when ingress and egress
//! speak the same protocol.

pub mod protocol;
pub mod tokens;
pub mod types;

pub use protocol::{ErrorKind, GatewayError, Protocol, ProtocolError, ProtocolResult};
pub use tokens::estimate_tokens;
pub use types::{
    AssistantContent, Attachment, AttachmentData, ChatRequest, ChatResponse, ContentPart,
    FinishReason, FunctionCall, FunctionDefinition, Message, ProgrammaticOutput, Reasoning,
    ReasoningSummary, Role, StreamEvent, SystemPrompt, Tool, ToolCall, ToolCallKind, ToolResult,
    Usage, UserContent,
};

pub use serde_json::{Map, Value};
