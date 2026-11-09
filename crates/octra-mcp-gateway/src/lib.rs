//! MCP Gateway client adapters, MCP endpoint, and control plane.
//!
//! The gateway fronts many upstream MCP servers (stdio or Streamable HTTP)
//! behind one `/mcp` endpoint, namespaces their tools as `<server>__<tool>`,
//! authenticates data plane callers with MCP API keys, enforces per-tool
//! consumer ACLs, and records every invocation.

pub mod auth;
pub mod client;
pub mod control;
pub mod conversion;
pub mod data;
pub mod http;
pub mod servers;
pub mod stdio;

use std::collections::HashSet;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use axum::Router;
use octra_config::OctraConfig;
use octra_core::{Error, Result};
use octra_mcp_core::jsonrpc;
use octra_mcp_core::protocol::methods;
use octra_storage::Storage;
use serde_json::Value;
use servers::{NotificationHub, ServerConnections};
use tokio::sync::broadcast;

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
    servers: Arc<ServerConnections>,
    notifications: Arc<NotificationHub>,
    refresh: Arc<RefreshContext>,
    coordinator: Arc<NotificationCoordinator>,
}

/// Everything a notice-triggered refresh needs, without holding the runtime.
pub(crate) struct RefreshContext {
    storage: Arc<Storage>,
    config: Arc<OctraConfig>,
    http: reqwest::Client,
    servers: Weak<ServerConnections>,
}

impl RefreshContext {
    /// Connects to a server, lists its tools, persists them, and records the
    /// negotiated protocol revision. The discovery connection is closed after
    /// the run; cached connections of the same server are dropped.
    async fn refresh(&self, server_id: &str) -> Result<DiscoveryStats> {
        let server = self
            .storage
            .mcp
            .get_server(server_id)
            .await?
            .ok_or_else(|| McpError::Config(format!("MCP server {server_id} does not exist")))
            .map_err(runtime_error)?;

        let servers = self
            .servers
            .upgrade()
            .ok_or_else(|| Error::Runtime("MCP runtime is shutting down".to_string()))?;
        let discovery = servers
            .discover(
                &server,
                Duration::from_secs(self.config.mcp.request_timeout_secs.max(1)),
                &self.http,
            )
            .await
            .map_err(runtime_error)?;
        let tools = self
            .storage
            .mcp
            .replace_tools(&server.id, &discovery.tools)
            .await?;
        self.storage
            .mcp
            .mark_server_seen(&server.id, &discovery.protocol_version)
            .await?;
        servers.invalidate(&server.id).await;

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
}

/// Debounces catalog-change notices and refreshes one server at a time.
pub struct NotificationCoordinator {
    hub: Arc<NotificationHub>,
    refresh: Arc<RefreshContext>,
    debounce: Duration,
    pending: Mutex<HashSet<String>>,
}

impl NotificationCoordinator {
    fn new(hub: Arc<NotificationHub>, refresh: Arc<RefreshContext>, debounce: Duration) -> Self {
        Self {
            hub,
            refresh,
            debounce,
            pending: Mutex::new(HashSet::new()),
        }
    }

    /// Handles one upstream notification. Only catalog-change methods trigger
    /// a refresh; everything else is forwarded by the caller.
    pub(crate) fn on_list_changed(self: &Arc<Self>, server_id: &str, method: &str) {
        if method != methods::LIST_CHANGED && method != methods::RESOURCES_LIST_CHANGED {
            return;
        }
        tracing::info!(
            server = %server_id,
            method,
            "upstream MCP catalog changed"
        );

        let inserted = {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            pending.insert(server_id.to_string())
        };
        if !inserted {
            return;
        }

        let this = self.clone();
        let id = server_id.to_string();
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            self.pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&id);
            return;
        };
        runtime.spawn(async move {
            tokio::time::sleep(this.debounce).await;
            this.pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&id);
            match this.refresh.refresh(&id).await {
                Ok(stats) => {
                    tracing::info!(
                        server = %id,
                        tools = stats.tools,
                        "refreshed MCP tools after a catalog change"
                    );
                    this.hub
                        .publish(&id, jsonrpc::notification(methods::LIST_CHANGED, None));
                }
                Err(err) => {
                    tracing::warn!(
                        server = %id,
                        error = %err,
                        "MCP tool refresh after a catalog change failed"
                    );
                }
            }
        });
    }
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
        let http = octra_data_plane::http_client(&config);
        let notifications = Arc::new(NotificationHub::default());
        let servers = Arc::new(ServerConnections::new(notifications.clone()));
        let refresh = Arc::new(RefreshContext {
            storage: storage.clone(),
            config: config.clone(),
            http: http.clone(),
            servers: Arc::downgrade(&servers),
        });
        let coordinator = Arc::new(NotificationCoordinator::new(
            notifications.clone(),
            refresh.clone(),
            Duration::from_millis(500),
        ));
        servers.set_coordinator(coordinator.clone());
        Ok(Self {
            http,
            storage,
            config,
            servers,
            notifications,
            refresh,
            coordinator,
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

    /// Subscribes to a server's notification channel. The channel is created
    /// on demand so a client GET stream can open before the upstream connects.
    pub fn notifications(&self, server_id: &str) -> Option<broadcast::Receiver<Value>> {
        self.notifications.subscribe(server_id)
    }

    /// Refreshes a server's tools after an upstream catalog change. The
    /// refresh is debounced and runs in the background.
    pub fn on_tool_list_changed(&self, server_id: &str) {
        self.coordinator
            .on_list_changed(server_id, methods::LIST_CHANGED);
    }

    /// Connects to a server, lists its tools, persists them, and records the
    /// negotiated protocol revision. The discovery connection is closed after
    /// the run; cached connections of the same server are dropped.
    pub(crate) async fn refresh_server(&self, server_id: &str) -> Result<DiscoveryStats> {
        self.refresh.refresh(server_id).await
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
