//! MCP Gateway client adapters, MCP endpoint, and control plane.
//!
//! The gateway fronts many upstream MCP servers (stdio or Streamable HTTP)
//! behind one `/mcp` endpoint, namespaces their tools as `<server>__<tool>`,
//! authenticates data plane callers with MCP API keys, enforces per-tool
//! consumer ACLs, and records every invocation.

pub mod auth;
pub mod client;
pub mod control;
pub mod data;
pub mod http;
pub mod servers;
pub mod stdio;

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use octra_config::OctraConfig;
use octra_core::{Error, Result};
use octra_storage::Storage;
use servers::ServerConnections;

use crate::client::McpError;

/// Outcome of a successful tool discovery run.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct DiscoveryStats {
    pub server_id: String,
    pub protocol_version: String,
    /// `serverInfo.name` reported by the upstream during `initialize`.
    pub upstream_name: String,
    pub tools: usize,
}

pub struct McpRuntime {
    pub storage: Arc<Storage>,
    pub config: Arc<OctraConfig>,
    http: reqwest::Client,
    servers: ServerConnections,
}

#[derive(Clone)]
pub struct McpDataState {
    pub runtime: Arc<McpRuntime>,
    pub storage: Arc<Storage>,
    pub config: Arc<OctraConfig>,
}

impl McpDataState {
    pub fn new(runtime: Arc<McpRuntime>, storage: Arc<Storage>, config: Arc<OctraConfig>) -> Self {
        Self {
            runtime,
            storage,
            config,
        }
    }
}

#[derive(Clone)]
pub struct McpControlState {
    pub runtime: Arc<McpRuntime>,
    pub storage: Arc<Storage>,
}

impl McpControlState {
    pub fn new(runtime: Arc<McpRuntime>, storage: Arc<Storage>) -> Self {
        Self { runtime, storage }
    }
}

impl McpRuntime {
    /// Builds the runtime. Upstream servers start lazily: on the first request,
    /// a discovery run, or an explicit refresh.
    pub async fn bootstrap(storage: Arc<Storage>, config: Arc<OctraConfig>) -> Result<Self> {
        Ok(Self {
            http: octra_data_plane::http_client(&config),
            storage,
            config,
            servers: ServerConnections::default(),
        })
    }

    /// Closes every live upstream connection. The next request reconnects
    /// against the current database state.
    pub async fn reload(&self) -> Result<()> {
        self.servers.close_all().await;
        Ok(())
    }

    /// Timeout for discovery and method-level upstream calls.
    pub fn request_timeout(&self) -> Duration {
        Duration::from_secs(self.config.mcp.request_timeout_secs.max(1))
    }

    /// Timeout for upstream tool execution.
    pub fn tool_call_timeout(&self) -> Duration {
        Duration::from_secs(self.config.mcp.tool_call_timeout_secs.max(1))
    }

    pub(crate) fn http(&self) -> &reqwest::Client {
        &self.http
    }

    pub(crate) fn servers(&self) -> &ServerConnections {
        &self.servers
    }

    /// Connects to a server, lists its tools, persists them, and records the
    /// negotiated protocol revision. The discovery connection is closed after
    /// the run; cached connections of the same server are dropped.
    pub(crate) async fn refresh_server(&self, server_id: &str) -> Result<DiscoveryStats> {
        let server = self
            .storage
            .get_mcp_server(server_id)
            .await?
            .ok_or_else(|| McpError::Config(format!("MCP server {server_id} does not exist")))
            .map_err(runtime_error)?;

        let discovery = self
            .servers
            .discover(&server, self.request_timeout(), self.http())
            .await
            .map_err(runtime_error)?;
        let tools = self
            .storage
            .replace_mcp_tools(&server.id, &discovery.tools)
            .await?;
        self.storage
            .mark_mcp_server_seen(&server.id, &discovery.protocol_version)
            .await?;
        self.servers.invalidate(&server.id).await;

        tracing::info!(
            server = %server.name,
            tools = tools.len(),
            protocol_version = %discovery.protocol_version,
            "discovered MCP tools"
        );

        Ok(DiscoveryStats {
            server_id: server.id,
            protocol_version: discovery.protocol_version,
            upstream_name: discovery.server_name,
            tools: tools.len(),
        })
    }

    /// Reloads the snapshot after a control plane mutation. Failure to reload
    /// is logged rather than failing the mutation: the database is the source
    /// of truth and the next request reconnects lazily.
    pub(crate) async fn reload_after_mutation(&self) {
        if let Err(err) = self.reload().await {
            tracing::warn!(error = %err, "MCP runtime reload after mutation failed");
        }
    }
}

/// Registers the `/mcp` routes on the unified data plane.
pub fn data_plane_router(state: McpDataState) -> Router {
    data::router(state)
}

/// Registers the `/api/v1/mcp/*` control plane routes.
pub fn control_router(state: McpControlState) -> Router {
    control::router(state)
}

/// Maps a transport error onto the daemon error type.
pub fn runtime_error(err: McpError) -> Error {
    Error::Runtime(err.to_string())
}
