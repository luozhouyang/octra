//! Upstream MCP client abstraction and shared error type.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use octra_mcp_core::jsonrpc::JsonRpcError;
use octra_mcp_core::protocol::{CallToolResult, InitializeResult, ListToolsResult};
use octra_mcp_core::{McpServer, Tool, Transport};
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

use crate::http::HttpClient;
use crate::stdio::StdioClient;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

const MAX_PAGES: usize = 50;

#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("{}", .0.message)]
    Upstream(JsonRpcError),
    #[error("MCP transport error: {0}")]
    Transport(String),
    #[error("MCP upstream timed out")]
    Timeout,
    #[error("MCP transport is not supported: {0}")]
    Unsupported(String),
    #[error("MCP server configuration is invalid: {0}")]
    Config(String),
    #[error("MCP storage error: {0}")]
    Storage(String),
    #[error("MCP internal error: {0}")]
    Internal(String),
}

impl McpError {
    /// Wraps an upstream JSON-RPC error, keeping its message verbatim so
    /// clients see the server's own wording.
    pub fn upstream(err: JsonRpcError) -> Self {
        Self::Upstream(err)
    }

    /// Stable machine code surfaced to JSON-RPC clients.
    pub fn code(&self) -> i64 {
        match self {
            Self::Upstream(err) => err.code,
            Self::Timeout => octra_mcp_core::jsonrpc::codes::UPSTREAM_ERROR,
            Self::Config(_) => octra_mcp_core::jsonrpc::codes::INVALID_PARAMS,
            _ => octra_mcp_core::jsonrpc::codes::INTERNAL_ERROR,
        }
    }
}

impl From<octra_core::Error> for McpError {
    fn from(err: octra_core::Error) -> Self {
        Self::Storage(err.to_string())
    }
}

impl From<JsonRpcError> for McpError {
    fn from(err: JsonRpcError) -> Self {
        Self::Upstream(err)
    }
}

/// One live connection to an upstream MCP server.
pub trait UpstreamClient: Send + Sync {
    /// Sends a JSON-RPC request and resolves with its `result`.
    fn request<'a>(
        &'a self,
        method: &'a str,
        params: Option<Value>,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<Value, McpError>>;

    /// Sends a JSON-RPC notification and waits for the write to complete.
    fn notify<'a>(&'a self, method: &'a str) -> BoxFuture<'a, Result<(), McpError>>;

    fn protocol_version(&self) -> Option<String>;

    /// Returns a receiver for server-initiated notifications. Transports
    /// without a notification channel return `None`.
    fn subscribe_notifications(&self) -> Option<tokio::sync::broadcast::Receiver<Value>> {
        None
    }

    /// Terminates the connection; later calls fail.
    fn close(&self);

    /// Downcast hook for transports that expose extra lifecycle state.
    fn as_any(&self) -> &dyn std::any::Any;
}

/// The stdio client behind a connection, when the transport is stdio.
pub fn as_stdio(client: &dyn UpstreamClient) -> Option<&StdioClient> {
    client.as_any().downcast_ref::<StdioClient>()
}

/// Opens a live connection for a persisted server definition.
pub fn connect(
    server: &McpServer,
    timeout: Duration,
    http: Option<&reqwest::Client>,
) -> Result<Arc<dyn UpstreamClient>, McpError> {
    match server.transport {
        Transport::Stdio => Ok(Arc::new(StdioClient::spawn(server, timeout)?)),
        Transport::Http => {
            let http = http
                .cloned()
                .ok_or_else(|| McpError::Config("shared HTTP client is unavailable".into()))?;
            Ok(Arc::new(HttpClient::new(server, http)))
        }
    }
}

/// Performs the MCP handshake and returns the negotiated protocol revision.
pub async fn initialize(
    client: &dyn UpstreamClient,
    timeout: Duration,
) -> Result<InitializeResult, McpError> {
    let params = json!({
        "protocolVersion": octra_mcp_core::protocol::LATEST_PROTOCOL_VERSION,
        "capabilities": {},
        "clientInfo": {
            "name": "octra-mcp-gateway",
            "version": octra_core::VERSION,
        }
    });

    let result: InitializeResult =
        typed(client.request("initialize", Some(params), timeout).await?)?;
    // Clients SHOULD send the initialized notification. A broken notification
    // channel must not fail an otherwise successful handshake.
    let _ = client.notify("notifications/initialized").await;
    Ok(result)
}

/// Follows `nextCursor` to collect every tool a server exposes.
pub async fn list_all_tools(
    client: &dyn UpstreamClient,
    timeout: Duration,
) -> Result<Vec<Tool>, McpError> {
    let mut tools = Vec::new();
    let mut cursor: Option<String> = None;

    for _ in 0..MAX_PAGES {
        let params = cursor.take().map(|cursor| json!({ "cursor": cursor }));
        let page: ListToolsResult = typed(client.request("tools/list", params, timeout).await?)?;
        tools.extend(page.tools);
        match page.next_cursor {
            Some(next) if !next.is_empty() => cursor = Some(next),
            _ => return Ok(tools),
        }
    }

    Err(McpError::Transport(format!(
        "tools/list did not terminate after {MAX_PAGES} pages"
    )))
}

pub async fn call_tool(
    client: &dyn UpstreamClient,
    name: &str,
    arguments: Option<Value>,
    timeout: Duration,
) -> Result<CallToolResult, McpError> {
    let mut params = Map::new();
    params.insert("name".into(), json!(name));
    if let Some(arguments) = arguments {
        params.insert("arguments".into(), arguments);
    }
    typed(
        client
            .request("tools/call", Some(Value::Object(params)), timeout)
            .await?,
    )
}

fn typed<T: DeserializeOwned>(value: Value) -> Result<T, McpError> {
    serde_json::from_value(value).map_err(|err| {
        McpError::Transport(format!("upstream returned an unexpected payload: {err}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_map_to_json_rpc_codes() {
        assert_eq!(McpError::Timeout.code(), -32006);
        assert_eq!(
            McpError::Upstream(JsonRpcError::method_not_found("x")).code(),
            -32601
        );
        assert_eq!(McpError::Config("x".into()).code(), -32602);
        assert_eq!(McpError::Transport("x".into()).code(), -32603);
        assert_eq!(
            McpError::from(octra_core::Error::Storage("db".into())).code(),
            -32603
        );
    }
}
