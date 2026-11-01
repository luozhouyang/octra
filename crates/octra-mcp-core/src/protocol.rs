//! MCP method names, payload types, and the gateway tool namespace.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod methods {
    pub const INITIALIZE: &str = "initialize";
    pub const INITIALIZED: &str = "notifications/initialized";
    pub const PING: &str = "ping";
    pub const TOOLS_LIST: &str = "tools/list";
    pub const TOOLS_CALL: &str = "tools/call";
    pub const LIST_CHANGED: &str = "notifications/tools/list_changed";
    pub const RESOURCES_LIST_CHANGED: &str = "notifications/resources/list_changed";
    pub const PROMPTS_LIST_CHANGED: &str = "notifications/prompts/list_changed";
    pub const CANCELLED: &str = "notifications/cancelled";
    pub const PROGRESS: &str = "notifications/progress";
    pub const LOGGING: &str = "notifications/message";
    pub const RESOURCES_UPDATED: &str = "notifications/resources/updated";
}

/// Protocol revisions Octra can speak to upstream servers.
pub const SUPPORTED_PROTOCOL_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];
pub const LATEST_PROTOCOL_VERSION: &str = "2025-06-18";

/// Negotiates the protocol revision for an upstream `initialize` handshake.
pub fn negotiate_protocol_version(requested: Option<&str>, supported: Option<&[String]>) -> String {
    if let Some(requested) = requested
        && is_supported(requested)
    {
        return requested.to_string();
    }
    if let Some(supported) = supported
        && let Some(found) = supported.iter().find(|version| is_supported(version))
    {
        return found.clone();
    }
    LATEST_PROTOCOL_VERSION.to_string()
}

pub fn is_supported(version: &str) -> bool {
    SUPPORTED_PROTOCOL_VERSIONS.contains(&version)
}

/// Namespace separator used by the gateway to combine many servers.
pub const TOOL_NAMESPACE_SEPARATOR: &str = "__";

/// Lands inside every `mcp_tools.name` when a server exposes a tool whose own
/// name contains the separator; such tools cannot be re-exported safely.
pub const RESERVED_TOOL_NAME: &str = "octra-reserved-name";

/// A server name usable both as an entity name and as a namespace prefix.
pub fn is_valid_server_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    name.len() <= 64
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        && !name.contains(TOOL_NAMESPACE_SEPARATOR)
}

/// Builds the externally visible `<server>__<tool>` name.
pub fn qualify_tool_name(server: &str, tool: &str) -> String {
    format!("{server}{TOOL_NAMESPACE_SEPARATOR}{tool}")
}

/// Splits a namespaced tool name into its server and tool segments. Both
/// segments must be non-empty.
pub fn parse_tool_name(qualified: &str) -> Option<(&str, &str)> {
    let (server, tool) = qualified.split_once(TOOL_NAMESPACE_SEPARATOR)?;
    if server.is_empty() || tool.is_empty() || tool.contains(TOOL_NAMESPACE_SEPARATOR) {
        return None;
    }
    Some((server, tool))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Implementation {
    pub name: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ServerCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompts: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logging: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completions: Option<Value>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InitializeParams {
    #[serde(rename = "protocolVersion")]
    pub protocol_version: String,
    pub capabilities: Value,
    #[serde(rename = "clientInfo")]
    pub client_info: Implementation,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InitializeResult {
    #[serde(rename = "protocolVersion")]
    pub protocol_version: String,
    #[serde(default)]
    pub capabilities: ServerCapabilities,
    #[serde(rename = "serverInfo")]
    pub server_info: Implementation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tool {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(rename = "inputSchema", default)]
    pub input_schema: Value,
    #[serde(
        rename = "outputSchema",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub output_schema: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<Value>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ListToolsResult {
    #[serde(default)]
    pub tools: Vec<Tool>,
    #[serde(
        rename = "nextCursor",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallToolParams {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallToolResult {
    #[serde(default)]
    pub content: Vec<Value>,
    #[serde(
        rename = "structuredContent",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub structured_content: Option<Value>,
    #[serde(rename = "isError", default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

/// The raw `tools/list` item as an upstream reports it, before namespacing.
pub type ProtocolTool = Tool;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    Stdio,
    /// Streamable HTTP. Serialized as `http`.
    #[serde(rename = "http", alias = "streamable-http", alias = "streamable_http")]
    Http,
}

impl Transport {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stdio => "stdio",
            Self::Http => "http",
        }
    }
}

impl std::str::FromStr for Transport {
    type Err = ParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "stdio" => Ok(Self::Stdio),
            "http" | "streamable-http" | "streamable_http" => Ok(Self::Http),
            other => Err(ParseError::Transport(other.to_string())),
        }
    }
}

impl fmt::Display for Transport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ParseError {
    #[error("unknown MCP transport: {0}")]
    Transport(String),
    #[error("tool name is reserved by the gateway: {0}")]
    ReservedToolName(String),
    #[error("{0}")]
    Invalid(String),
}

/// Options passed to a spawned stdio command from `mcp_servers.config_json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StdioConfig {
    pub args: Vec<String>,
    pub env: std::collections::BTreeMap<String, String>,
    pub cwd: Option<String>,
}

/// Validates a tool name before it is stored. The characters allowed keep the
/// qualified `<server>__<tool>` form parseable.
pub fn validate_tool_name(tool: &str) -> Result<(), ParseError> {
    let mut chars = tool.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphanumeric() => {}
        _ => {
            return Err(ParseError::Invalid(format!(
                "tool name must start with an ASCII letter or digit: {tool}"
            )));
        }
    }
    if tool.len() > 128 || !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(ParseError::Invalid(format!(
            "tool name contains unsupported characters: {tool}"
        )));
    }
    if tool.contains(TOOL_NAMESPACE_SEPARATOR) {
        return Err(ParseError::ReservedToolName(tool.to_string()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qualifies_and_parses_tool_names() {
        let qualified = qualify_tool_name("files", "read_file");
        assert_eq!(qualified, "files__read_file");
        assert_eq!(parse_tool_name(&qualified), Some(("files", "read_file")));
    }

    #[test]
    fn rejects_ambiguous_tool_names() {
        assert!(parse_tool_name("noseparator").is_none());
        assert!(parse_tool_name("a__b__c").is_none());
        assert!(parse_tool_name("__tool").is_none());
        assert!(parse_tool_name("server__").is_none());
    }

    #[test]
    fn validates_server_and_tool_names() {
        assert!(is_valid_server_name("files"));
        assert!(is_valid_server_name("my_server-1"));
        assert!(!is_valid_server_name("bad__name"));
        assert!(!is_valid_server_name("-leading"));

        assert!(validate_tool_name("read_file").is_ok());
        assert!(validate_tool_name("octra-reserved-name").is_ok());
        assert_eq!(
            validate_tool_name("bad__tool"),
            Err(ParseError::ReservedToolName("bad__tool".into()))
        );
        assert!(validate_tool_name("with space").is_err());
        assert!(validate_tool_name("").is_err());
    }

    #[test]
    fn transport_round_trips() {
        assert_eq!("stdio".parse::<Transport>().unwrap(), Transport::Stdio);
        assert_eq!(
            "streamable-http".parse::<Transport>().unwrap(),
            Transport::Http
        );
        assert_eq!(serde_json::to_value(Transport::Http).unwrap(), "http");
        assert!("sse".parse::<Transport>().is_err());
    }

    #[test]
    fn negotiates_protocol_version() {
        assert_eq!(
            negotiate_protocol_version(Some("2025-03-26"), None),
            "2025-03-26"
        );
        assert_eq!(
            negotiate_protocol_version(
                Some("1999-01-01"),
                Some(&[SUPPORTED_PROTOCOL_VERSIONS[2].to_string()])
            ),
            "2024-11-05"
        );
        assert_eq!(
            negotiate_protocol_version(None, None),
            LATEST_PROTOCOL_VERSION
        );
    }

    #[test]
    fn tool_deserializes_open_schema() {
        let tool: Tool = serde_json::from_value(serde_json::json!({
            "name": "echo",
            "inputSchema": {"type": "object"},
            "custom": 1
        }))
        .unwrap();
        assert_eq!(tool.name, "echo");
        assert_eq!(tool.extra.get("custom"), Some(&serde_json::json!(1)));
    }
}
