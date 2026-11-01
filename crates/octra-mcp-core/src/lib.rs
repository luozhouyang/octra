//! MCP JSON-RPC protocol types and gateway domain types.

pub mod domain;
pub mod jsonrpc;
pub mod protocol;

pub use domain::{
    McpApiKey, McpInvocation, McpServer, McpSession, McpTool, McpToolAcl, NewApiKey, NewMcpServer,
};
pub use protocol::{
    CallToolParams, InitializeResult, ListToolsResult, ProtocolTool, StdioConfig, Tool, Transport,
    parse_tool_name, qualify_tool_name,
};
