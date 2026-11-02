//! API gateway for Octra: a reverse proxy with route matching, load
//! balancing, and API-key authentication.
//!
//! * [`data_plane_router`] is the catch-all proxy mounted on the unified data
//!   plane. It only sees requests not claimed by the LLM (`/v1/*`) or MCP
//!   (`/mcp`) gateways.
//! * [`control_router`] exposes CRUD endpoints for the gateway configuration on
//!   the control plane. The daemon protects it with the shared bearer token.

pub mod admin;
pub mod audit;
pub mod data;
pub mod http_client;
pub mod key;
pub mod metrics;

use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use octra_api_plugin_runtime::PluginRuntime;
use octra_api_plugin_wasm::{WasmEngine, WasmHostProvider, WasmPluginCatalog};
use octra_config::OctraConfig;
use octra_plugin_storage::PluginStorageRoot;
use octra_runtime::GatewayRuntime;
use octra_storage::Storage;

pub use audit::{AuditState, record_control_audit};
pub use key::{GeneratedApiKey, generate_api_key};
pub use metrics::{ApiMetrics, MetricsSnapshot};

#[derive(Clone)]
pub struct ApiDataState {
    pub runtime: Arc<GatewayRuntime>,
    pub plugins: Arc<PluginRuntime>,
    pub storage: Option<Arc<Storage>>,
    pub config: Arc<OctraConfig>,
    pub client: reqwest::Client,
    pub metrics: Arc<ApiMetrics>,
}

impl ApiDataState {
    pub fn new(runtime: Arc<GatewayRuntime>, config: Arc<OctraConfig>) -> Self {
        Self::with_metrics(runtime, config, Arc::new(ApiMetrics::default()))
    }

    pub fn with_metrics(
        runtime: Arc<GatewayRuntime>,
        config: Arc<OctraConfig>,
        metrics: Arc<ApiMetrics>,
    ) -> Self {
        Self::with_plugins(runtime, config, metrics, Arc::new(PluginRuntime::empty()))
    }

    pub fn with_plugins(
        runtime: Arc<GatewayRuntime>,
        config: Arc<OctraConfig>,
        metrics: Arc<ApiMetrics>,
        plugins: Arc<PluginRuntime>,
    ) -> Self {
        let client = http_client::build(&config);
        Self {
            runtime,
            plugins,
            storage: None,
            config,
            client,
            metrics,
        }
    }

    /// Records access logs to storage. Without this the data plane keeps its
    /// existing in-memory logging only.
    pub fn with_storage(mut self, storage: Arc<Storage>) -> Self {
        self.storage = Some(storage);
        self
    }

    pub fn metrics(&self) -> &Arc<ApiMetrics> {
        &self.metrics
    }
}

/// Shared handles used by the plugin lifecycle endpoints. Present only when
/// the daemon enabled the WASM runtime.
pub struct WasmControlState {
    pub engine: Arc<WasmEngine>,
    pub catalog: Arc<WasmPluginCatalog>,
    pub host_provider: Arc<WasmHostProvider>,
    pub storage_root: Arc<PluginStorageRoot>,
    pub artifact_dir: PathBuf,
    pub max_artifact_bytes: u64,
}

#[derive(Clone)]
pub struct ApiControlState {
    pub storage: Arc<Storage>,
    pub runtime: Arc<GatewayRuntime>,
    pub plugins: Arc<PluginRuntime>,
    pub metrics: Arc<ApiMetrics>,
    pub wasm: Option<Arc<WasmControlState>>,
}

impl ApiControlState {
    pub fn new(storage: Arc<Storage>, runtime: Arc<GatewayRuntime>) -> Self {
        Self::with_metrics(storage, runtime, Arc::new(ApiMetrics::default()))
    }

    pub fn with_metrics(
        storage: Arc<Storage>,
        runtime: Arc<GatewayRuntime>,
        metrics: Arc<ApiMetrics>,
    ) -> Self {
        Self::with_plugins(storage, runtime, metrics, Arc::new(PluginRuntime::empty()))
    }

    pub fn with_plugins(
        storage: Arc<Storage>,
        runtime: Arc<GatewayRuntime>,
        metrics: Arc<ApiMetrics>,
        plugins: Arc<PluginRuntime>,
    ) -> Self {
        Self {
            storage,
            runtime,
            plugins,
            metrics,
            wasm: None,
        }
    }

    pub fn with_wasm(mut self, wasm: Arc<WasmControlState>) -> Self {
        self.wasm = Some(wasm);
        self
    }
}

/// Catch-all data plane router, mounted between the LLM and MCP routers and
/// therefore the fallback for the unified listener.
pub fn data_plane_router(state: ApiDataState) -> Router {
    Router::new().fallback(data::proxy).with_state(state)
}

/// Control plane router for gateway configuration.
pub fn control_router(state: ApiControlState) -> Router {
    admin::router().with_state(state)
}
