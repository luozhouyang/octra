//! Per-server connection management: lazy start, request retry with one
//! restart, tool discovery, and teardown on reload.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use octra_mcp_core::McpServer;
use octra_storage::mcp::DiscoveredTool;
use serde_json::{Map, Value};
use tokio::sync::Mutex;

use crate::client::{
    McpError, UpstreamClient, as_stdio, call_tool, connect, initialize, list_all_tools,
};

/// A live connection plus the definition it was opened for.
///
/// The definition is part of the hash key, so changing a server's URL,
/// command, headers, or config opens a fresh connection instead of reusing a
/// stale one.
#[derive(Clone)]
struct Live {
    definition: McpServer,
    client: Arc<dyn UpstreamClient>,
}

#[derive(Default)]
pub struct ServerConnections {
    live: Mutex<HashMap<String, Live>>,
}

impl ServerConnections {
    /// Returns a live client, starting and handshaking one if needed.
    ///
    /// The registry lock is never held across a handshake, child spawn, or
    /// any other await: `close_all` must always be able to cancel hung
    /// children. Two callers racing on a cold server may each start one; the
    /// loser's client is closed right after it is stored.
    pub async fn acquire(
        &self,
        server: &McpServer,
        timeout: Duration,
        http: &reqwest::Client,
    ) -> Result<Arc<dyn UpstreamClient>, McpError> {
        {
            let guard = self.live.lock().await;
            if let Some(live) = guard.get(&server.id)
                && same_definition(&live.definition, server)
            {
                return Ok(live.client.clone());
            }
        }

        let client = connect(server, timeout, Some(http))?;
        match tokio::time::timeout(timeout, initialize(client.as_ref(), timeout)).await {
            Ok(Ok(_)) => {}
            Ok(Err(err)) => {
                self.discard(&client).await;
                return Err(err);
            }
            Err(_) => {
                self.discard(&client).await;
                return Err(McpError::Timeout);
            }
        }

        let mut guard = self.live.lock().await;
        if let Some(previous) = guard.insert(
            server.id.clone(),
            Live {
                definition: server.clone(),
                client: client.clone(),
            },
        ) {
            previous.client.close();
        }
        Ok(client)
    }

    /// Closes a client that failed to handshake, waiting for the stdio worker
    /// so the child is gone before the caller reports the failure.
    async fn discard(&self, client: &Arc<dyn UpstreamClient>) {
        client.close();
        if let Some(stdio) = as_stdio(client.as_ref()) {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            while !stdio.is_closed() && tokio::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }

    /// Drops the live client for a server; the next request reconnects.
    pub async fn invalidate(&self, server_id: &str) {
        let removed = self.live.lock().await.remove(server_id);
        if let Some(live) = removed {
            live.client.close();
        }
    }

    /// Requests a tool call, retrying exactly once after a failed connection.
    pub async fn call(
        &self,
        server: &McpServer,
        tool: &str,
        arguments: Option<Value>,
        timeout: Duration,
        http: &reqwest::Client,
    ) -> Result<Value, McpError> {
        let client = self.acquire(server, timeout, http).await?;
        let first = call_tool(client.as_ref(), tool, arguments.clone(), timeout).await;

        match first {
            Ok(result) => serde_json::to_value(result)
                .map_err(|err| McpError::Internal(format!("cannot encode tool result: {err}"))),
            Err(err) if retryable(server, &err) => {
                tracing::info!(
                    server = %server.name,
                    tool,
                    error = %err,
                    "restarting MCP server after a failed call"
                );
                self.invalidate(&server.id).await;
                let client = self.acquire(server, timeout, http).await?;
                let result = call_tool(client.as_ref(), tool, arguments, timeout).await;
                let result = result?;
                serde_json::to_value(result)
                    .map_err(|err| McpError::Internal(format!("cannot encode tool result: {err}")))
            }
            Err(err) => Err(err),
        }
    }

    /// Connects, handshakes, lists every tool, and closes the connection.
    pub async fn discover(
        &self,
        server: &McpServer,
        timeout: Duration,
        http: &reqwest::Client,
    ) -> Result<Discovery, McpError> {
        let client = connect(server, timeout, Some(http))?;
        let outcome = async {
            let init = initialize(client.as_ref(), timeout).await?;
            let tools = list_all_tools(client.as_ref(), timeout).await?;
            Ok::<_, McpError>((init, tools))
        };
        let outcome = tokio::time::timeout(timeout.saturating_mul(2), outcome)
            .await
            .map_err(|_| McpError::Timeout)?;
        client.close();

        let (init, tools) = outcome?;
        Ok(Discovery {
            protocol_version: init.protocol_version,
            server_name: init.server_info.name,
            tools: tools
                .into_iter()
                .filter_map(|tool| match octra_mcp_core::protocol::validate_tool_name(&tool.name) {
                    Ok(()) => Some(DiscoveredTool {
                        name: tool.name,
                        description: tool.description.unwrap_or_default(),
                        schema: tool.input_schema,
                    }),
                    Err(err) => {
                        tracing::warn!(server = %server.name, %err, "skipping an unusable tool name");
                        None
                    }
                })
                .collect(),
        })
    }

    /// Drops every live connection (runtime reload).
    pub async fn close_all(&self) {
        let live: Vec<Live> = self
            .live
            .lock()
            .await
            .drain()
            .map(|(_, live)| live)
            .collect();
        for entry in live {
            entry.client.close();
        }
    }

    pub async fn live_count(&self) -> usize {
        self.live.lock().await.len()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Discovery {
    pub protocol_version: String,
    pub server_name: String,
    pub tools: Vec<DiscoveredTool>,
}

fn same_definition(left: &McpServer, right: &McpServer) -> bool {
    // `last_seen_at` and `protocol_version` change on every heartbeat and are
    // intentionally excluded from the comparison.
    let left = Definition {
        name: &left.name,
        transport: left.transport.as_str(),
        command: left.command.as_deref(),
        url: left.url.as_deref(),
        config: canonical(&left.config),
        headers: canonical(&Value::Object(left.headers.clone())),
        enabled: left.enabled,
    };
    let right = Definition {
        name: &right.name,
        transport: right.transport.as_str(),
        command: right.command.as_deref(),
        url: right.url.as_deref(),
        config: canonical(&right.config),
        headers: canonical(&Value::Object(right.headers.clone())),
        enabled: right.enabled,
    };
    left == right
}

#[derive(PartialEq)]
struct Definition<'a> {
    name: &'a str,
    transport: &'a str,
    command: Option<&'a str>,
    url: Option<&'a str>,
    config: String,
    headers: String,
    enabled: bool,
}

/// Re-serializes an object with sorted keys for stable comparison.
fn canonical(value: &Value) -> String {
    fn sort(value: &Value) -> Value {
        match value {
            Value::Object(map) => {
                let mut sorted = Map::new();
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                for key in keys {
                    sorted.insert(key.clone(), sort(&map[key]));
                }
                Value::Object(sorted)
            }
            Value::Array(items) => Value::Array(items.iter().map(sort).collect()),
            other => other.clone(),
        }
    }
    sort(value).to_string()
}

/// Only transport failures are retried, and only for stdio servers: a stdio
/// child that died is safe to start again, while an HTTP failure may have run
/// the tool on the remote side and must not be replayed. Protocol errors and
/// timeouts are always final.
fn retryable(server: &McpServer, err: &McpError) -> bool {
    matches!(err, McpError::Transport(_)) && server.transport == octra_mcp_core::Transport::Stdio
}
