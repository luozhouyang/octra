//! API gateway for Octra: a reverse proxy with route matching, load
//! balancing, and API-key authentication.
//!
//! * [`data_plane_router`] is the catch-all proxy mounted on the unified data
//!   plane. It serves API routes and, at `/v1` and `/v1beta`, the in-process
//!   LLM upstream. MCP (`/mcp`) remains a sibling router.
//! * [`control_router`] exposes CRUD endpoints for the gateway configuration on
//!   the control plane. The daemon protects it with the shared bearer token.

pub mod admin;
pub mod audit;
pub mod client_ip;
pub mod data;
pub mod http_client;
pub mod internal;
pub mod key;
pub mod metrics;

use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use octra_api_plugin_runtime::PluginRuntime;
use octra_api_plugin_types::{EventPublisher, PluginManifest};
use octra_api_plugin_wasm::{WasmEngine, WasmHostProvider, WasmPluginCatalog};
use octra_config::OctraConfig;
use octra_plugin_storage::PluginStorageRoot;
use octra_runtime::GatewayRuntime;
use octra_storage::Storage;

pub use audit::{AuditState, record_control_audit};
pub use client_ip::ClientIpResolver;
pub use internal::{InternalUpstream, InternalUpstreamContext};
pub use key::{GeneratedApiKey, generate_api_key};
pub use metrics::{ApiMetrics, MetricsSnapshot};

#[derive(Clone)]
pub struct ApiDataState {
    pub runtime: Arc<GatewayRuntime>,
    pub plugins: Arc<PluginRuntime>,
    pub storage: Option<Arc<Storage>>,
    pub config: Arc<OctraConfig>,
    pub client: reqwest::Client,
    pub client_ip: Arc<ClientIpResolver>,
    pub metrics: Arc<ApiMetrics>,
    pub events: Option<Arc<dyn EventPublisher>>,
    /// Optional in-process upstream for services that are not reached over the
    /// network. The LLM gateway uses this when it is mounted behind an API
    /// gateway route.
    pub internal_upstream: Option<Arc<dyn InternalUpstream>>,
    /// Process-local key for signing sticky-target cookies.
    pub(crate) sticky_secret: Arc<[u8; 32]>,
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
        let client_ip = Arc::new(ClientIpResolver::from_config(&config.gateway));
        Self {
            runtime,
            plugins,
            storage: None,
            config,
            client,
            client_ip,
            metrics,
            events: None,
            internal_upstream: None,
            sticky_secret: Arc::new(rand::random::<[u8; 32]>()),
        }
    }

    /// Publishes gateway lifecycle events. Without this the data plane keeps
    /// its existing logging only.
    pub fn with_events(mut self, events: Arc<dyn EventPublisher>) -> Self {
        self.events = Some(events);
        self
    }

    /// Records access logs to storage. Without this the data plane keeps its
    /// existing in-memory logging only.
    pub fn with_storage(mut self, storage: Arc<Storage>) -> Self {
        self.storage = Some(storage);
        self
    }

    /// Serves services tagged as internal in-process instead of over the
    /// network.
    pub fn with_internal_upstream(mut self, upstream: Arc<dyn InternalUpstream>) -> Self {
        self.internal_upstream = Some(upstream);
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
    /// Base64 Ed25519 public keys; empty disables signature checks.
    pub trusted_keys: Vec<String>,
    pub allow_unsigned: bool,
}

#[derive(Clone)]
pub struct ApiControlState {
    pub storage: Arc<Storage>,
    pub runtime: Arc<GatewayRuntime>,
    pub plugins: Arc<PluginRuntime>,
    pub metrics: Arc<ApiMetrics>,
    pub wasm: Option<Arc<WasmControlState>>,
    /// Optional notification used when consumer-group membership changes and
    /// another runtime (for example the LLM identity snapshot) needs a reload.
    pub reload_hook: Option<ApiReloadHook>,
    /// Optional lifecycle hook for installing LLM WASM artifacts. The daemon
    /// owns the LLM registry and runtime, keeping this crate independent of it.
    pub llm_wasm: Option<Arc<dyn LlmWasmLifecycle>>,
    pub events: Option<Arc<dyn EventPublisher>>,
}

/// Dynamic lifecycle operations the API installer delegates to the LLM
/// runtime. Implementations must register/replace the factory and reload the
/// LLM plugin chains; the API installer writes the catalog row.
#[async_trait::async_trait]
pub trait LlmWasmLifecycle: Send + Sync {
    /// Registers or replaces a plugin. `replaces_hash` is the artifact hash of
    /// the version being upgraded, so the implementation can evict the old
    /// compiled component after a successful reload.
    async fn install(
        &self,
        manifest: PluginManifest,
        artifact_dir: PathBuf,
        replaces_hash: Option<&str>,
    ) -> Result<(), String>;
    /// Removes a plugin. `artifact_hash` identifies the compiled component to
    /// evict once the new chains are live.
    async fn uninstall(&self, name: &str, artifact_hash: Option<&str>) -> Result<(), String>;
    /// Whether the plugin factory is currently registered with the LLM
    /// runtime. Used by the shared health endpoint.
    fn registered(&self, name: &str) -> bool;
}

/// A synchronous notification hook. Implementations should schedule async work
/// themselves so control-plane handlers stay independent of other runtimes.
pub type ApiReloadHook = Arc<dyn Fn() + Send + Sync>;

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
            reload_hook: None,
            llm_wasm: None,
            events: None,
        }
    }

    pub fn with_events(mut self, events: Arc<dyn EventPublisher>) -> Self {
        self.events = Some(events);
        self
    }

    pub fn with_wasm(mut self, wasm: Arc<WasmControlState>) -> Self {
        self.wasm = Some(wasm);
        self
    }

    pub fn with_llm_wasm_lifecycle(mut self, lifecycle: Arc<dyn LlmWasmLifecycle>) -> Self {
        self.llm_wasm = Some(lifecycle);
        self
    }

    pub fn with_reload_hook(mut self, hook: ApiReloadHook) -> Self {
        self.reload_hook = Some(hook);
        self
    }

    pub(crate) fn notify_reload_hook(&self) {
        if let Some(hook) = &self.reload_hook {
            hook();
        }
    }
}

/// Catch-all data plane router. API routes, including the `/v1` and `/v1beta`
/// routes to the in-process LLM upstream, are resolved here.
pub fn data_plane_router(state: ApiDataState) -> Router {
    Router::new().fallback(data::proxy).with_state(state)
}

/// Control plane router for gateway configuration.
pub fn control_router(state: ApiControlState) -> Router {
    admin::router().with_state(state)
}
