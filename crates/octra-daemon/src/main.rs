mod maintenance;
mod token;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use axum::Router;
use octra_api_gateway::{
    ApiControlState, ApiDataState, ApiMetrics, AuditState, LlmWasmLifecycle, WasmControlState,
};
use octra_api_plugin_runtime::{
    CompositePluginFactoryResolver, PluginFactoryResolver, PluginRegistry, PluginRuntime,
};
use octra_api_plugin_types::PluginManifest;
use octra_api_plugin_wasm::{
    InstalledWasmPlugin, WasmEngine, WasmHostProvider, WasmLimits, WasmPluginCatalog,
    WasmPluginFactory,
};
use octra_config::Workspace;
use octra_llm_gateway::{LlmControlState, LlmDataState, LlmPluginRuntime, LlmRuntime};
use octra_llm_plugin_runtime::LlmPluginRegistry;
use octra_llm_plugin_wasm::LlmWasmPluginFactory;
use octra_mcp_gateway::{McpControlState, McpDataState, McpRuntime};
use octra_plugin_storage::PluginStorageRoot;
use octra_runtime::GatewayRuntime;
use octra_types::{AppState, AuthState};
use tokio_util::sync::CancellationToken;

/// Bridges the API installer to the LLM plugin registry. The runtime is
/// reloaded after every change; a failed reload restores the previous factory
/// so the catalog and the compiled chains stay consistent.
struct LlmWasmRuntime {
    registry: Arc<LlmPluginRegistry>,
    runtime: Arc<LlmPluginRuntime>,
    engine: Arc<WasmEngine>,
}

#[async_trait::async_trait]
impl LlmWasmLifecycle for LlmWasmRuntime {
    async fn install(
        &self,
        manifest: PluginManifest,
        artifact_dir: std::path::PathBuf,
        replaces_hash: Option<&str>,
    ) -> Result<(), String> {
        let factory = LlmWasmPluginFactory::from_artifact(&self.engine, &artifact_dir)
            .map_err(|error| error.message)?;
        let installed_hash = factory.component_hash().to_string();
        let previous = self.registry.factory(&manifest.name);
        self.registry.register_arc(Arc::new(factory));
        match self.runtime.reload().await {
            Ok(()) => {
                if let Some(old_hash) = replaces_hash
                    && old_hash != installed_hash
                {
                    self.engine.remove_component(old_hash);
                }
                Ok(())
            }
            Err(error) => {
                match previous {
                    Some(previous) => self.registry.register_arc(previous),
                    None => {
                        self.registry.remove(&manifest.name);
                    }
                }
                if let Err(rollback) = self.runtime.reload().await {
                    tracing::warn!(
                        %rollback,
                        plugin = %manifest.name,
                        "cannot restore LLM plugin chains after a failed install"
                    );
                }
                // The new component is not referenced by any registered
                // factory at this point, unless it is the version we just
                // tried to replace.
                if replaces_hash != Some(installed_hash.as_str()) {
                    self.engine.remove_component(&installed_hash);
                }
                Err(error.to_string())
            }
        }
    }

    async fn uninstall(&self, name: &str, artifact_hash: Option<&str>) -> Result<(), String> {
        let previous = self.registry.remove(name);
        match self.runtime.reload().await {
            Ok(()) => {
                if let Some(hash) = artifact_hash {
                    self.engine.remove_component(hash);
                }
                Ok(())
            }
            Err(error) => {
                if let Some(previous) = previous {
                    self.registry.register_arc(previous);
                    if let Err(rollback) = self.runtime.reload().await {
                        tracing::warn!(
                            %rollback,
                            plugin = %name,
                            "cannot restore LLM plugin chains after a failed uninstall"
                        );
                    }
                }
                Err(error.to_string())
            }
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let workspace = Workspace::resolve().context("cannot resolve the Octra workspace")?;
    let config =
        Arc::new(octra_config::load_or_create(&workspace).context("cannot load configuration")?);

    octra_observability::init(&config.logging.level, config.logging.format)
        .context("cannot initialize logging")?;

    let database = workspace.database_path(&config);
    tracing::info!(path = %database.display(), "opening sqlite storage");

    let storage = Arc::new(
        octra_storage::Storage::connect(&database)
            .await
            .context("cannot open sqlite storage")?,
    );

    let api_runtime = Arc::new(
        GatewayRuntime::bootstrap(storage.clone())
            .await
            .context("cannot build the API gateway snapshot")?,
    );
    let health_runtime = api_runtime.clone();
    let llm_runtime = Arc::new(
        LlmRuntime::bootstrap(storage.clone(), config.clone())
            .await
            .context("cannot build the LLM gateway runtime")?,
    );
    let mcp_runtime = Arc::new(
        McpRuntime::bootstrap(storage.clone(), config.clone())
            .await
            .context("cannot build the MCP gateway runtime")?,
    );

    let mut plugin_registry = PluginRegistry::new();
    octra_api_plugin_builtin::register(&mut plugin_registry);
    let plugin_registry = Arc::new(plugin_registry);
    let plugin_instances = storage
        .api
        .list_plugin_instances()
        .await
        .context("cannot load plugin instances")?;

    let mut wasm_control: Option<Arc<WasmControlState>> = None;
    let plugin_runtime = if config.plugins.wasm_enabled {
        let engine = Arc::new(
            WasmEngine::new(WasmLimits {
                max_fuel: config.plugins.max_fuel,
                max_memory_bytes: config.plugins.max_memory_bytes,
                timeout: Duration::from_millis(config.plugins.default_timeout_ms),
                max_body_bytes: config.plugins.max_body_bytes,
                max_inflight: config.plugins.max_inflight,
            })
            .context("cannot create the wasm plugin engine")?,
        );
        let storage_root = Arc::new(
            PluginStorageRoot::new(workspace.plugin_dir(&config))
                .with_max_bytes(config.plugins.max_plugin_db_bytes),
        );
        let catalog = Arc::new(WasmPluginCatalog::new());
        let mut installed = Vec::new();

        for record in storage
            .llm
            .list_installed_wasm_plugins()
            .await
            .context("cannot load installed wasm plugins")?
        {
            let manifest: PluginManifest = match serde_json::from_str(&record.manifest_json) {
                Ok(manifest) => manifest,
                Err(error) => {
                    if config.plugins.strict_boot {
                        return Err(anyhow::anyhow!(
                            "cannot parse manifest for plugin '{}': {error}",
                            record.name
                        ));
                    }
                    tracing::warn!(plugin = %record.name, %error, "skipping invalid plugin manifest");
                    continue;
                }
            };
            let Some(path) = record.artifact_path.as_deref() else {
                tracing::warn!(plugin = %record.name, "installed plugin has no artifact path");
                continue;
            };
            let factory =
                match WasmPluginFactory::from_artifact(&engine, std::path::Path::new(path)) {
                    Ok(factory) => factory,
                    Err(error) => {
                        if config.plugins.strict_boot {
                            return Err(anyhow::anyhow!(
                                "cannot load plugin '{}': {error}",
                                record.name
                            ));
                        }
                        tracing::warn!(plugin = %record.name, %error, "skipping wasm plugin");
                        continue;
                    }
                };
            catalog.insert(Arc::new(factory));
            installed.push(InstalledWasmPlugin::new(
                manifest,
                record.approved_capabilities.clone(),
            ));
        }

        let resolver: Arc<dyn PluginFactoryResolver> = Arc::new(
            CompositePluginFactoryResolver::new(plugin_registry.clone(), catalog.clone()),
        );
        let host_provider = Arc::new(WasmHostProvider::new(storage_root.clone(), installed));
        let runtime =
            PluginRuntime::with_resolver(plugin_registry, resolver, host_provider.clone());
        runtime
            .reload(plugin_instances)
            .context("cannot build the plugin runtime")?;
        wasm_control = Some(Arc::new(WasmControlState {
            engine,
            catalog,
            host_provider,
            storage_root,
            artifact_dir: workspace.plugin_artifact_dir(&config),
            max_artifact_bytes: u64::try_from(config.plugins.max_artifact_bytes)
                .unwrap_or(u64::MAX),
            trusted_keys: config.plugins.trusted_keys.clone(),
            allow_unsigned: config.plugins.allow_unsigned,
        }));
        Arc::new(runtime)
    } else {
        Arc::new(
            PluginRuntime::with_plugins(plugin_registry, plugin_instances)
                .context("cannot build the plugin runtime")?,
        )
    };

    let pii_engines = Arc::new(octra_llm_gateway::pii::PiiEngineRuntime::with_models_dir(
        workspace.model_dir(),
    ));
    let pii_records = storage
        .llm
        .list_pii_engines()
        .await
        .context("cannot load PII engine definitions")?;
    for warning in pii_engines.reload(&pii_records) {
        tracing::warn!(%warning, "PII engine unavailable");
    }

    let llm_registry = octra_llm_plugin_builtin::registry_with(Some(pii_engines.clone()));
    if let Some(control) = wasm_control.as_ref() {
        for record in storage
            .llm
            .list_installed_wasm_plugins()
            .await
            .context("cannot load installed LLM wasm plugins")?
        {
            if record.kind != "llm" {
                continue;
            }
            let Some(path) = record.artifact_path.as_deref() else {
                tracing::warn!(plugin = %record.name, "installed LLM wasm plugin has no artifact path");
                continue;
            };
            match LlmWasmPluginFactory::from_artifact(&control.engine, std::path::Path::new(path)) {
                Ok(factory) => llm_registry.register_arc(Arc::new(factory)),
                Err(error) => {
                    if config.plugins.strict_boot {
                        return Err(anyhow::anyhow!(
                            "cannot load LLM wasm plugin '{}': {error}",
                            record.name
                        ));
                    }
                    tracing::warn!(plugin = %record.name, %error, "skipping LLM wasm plugin");
                }
            }
        }
    }
    let llm_registry = Arc::new(llm_registry);
    let llm_plugin_runtime = Arc::new(
        LlmPluginRuntime::bootstrap(storage.clone(), llm_registry.clone())
            .await
            .context("cannot build the LLM plugin runtime")?,
    );
    let llm_wasm_lifecycle: Option<Arc<dyn LlmWasmLifecycle>> =
        wasm_control.as_ref().map(|control| {
            Arc::new(LlmWasmRuntime {
                registry: llm_registry.clone(),
                runtime: llm_plugin_runtime.clone(),
                engine: control.engine.clone(),
            }) as Arc<dyn LlmWasmLifecycle>
        });

    let token =
        token::load_or_create(&workspace.token_file()).context("cannot prepare control token")?;
    let token = Arc::new(token);

    let app_state = AppState {
        runtime: api_runtime.clone(),
        storage: storage.clone(),
        config: config.clone(),
        token: token.clone(),
        started_at: Instant::now(),
    };

    let api_metrics = Arc::new(ApiMetrics::default());

    let mut api_control = ApiControlState::with_plugins(
        storage.clone(),
        api_runtime.clone(),
        api_metrics.clone(),
        plugin_runtime.clone(),
    );
    // Consumer-group membership lives in the API store but is consumed by the
    // LLM identity snapshot. Reload the LLM runtime after group mutations
    // without making the API gateway depend on the LLM gateway.
    let llm_reload_runtime = llm_runtime.clone();
    api_control = api_control.with_reload_hook(Arc::new(move || {
        let runtime = llm_reload_runtime.clone();
        tokio::spawn(async move {
            if let Err(error) = runtime.reload().await {
                tracing::warn!(%error, "cannot reload LLM runtime after consumer group change");
            }
        });
    }));
    if let Some(wasm) = wasm_control {
        api_control = api_control.with_wasm(wasm);
    }
    if let Some(lifecycle) = llm_wasm_lifecycle {
        api_control = api_control.with_llm_wasm_lifecycle(lifecycle);
    }

    let control_app = Router::new()
        .merge(octra_types::core_router(app_state))
        .merge(octra_api_gateway::control_router(api_control))
        .merge(octra_llm_gateway::control_router(
            LlmControlState::new(llm_runtime.clone(), storage.clone())
                .with_plugins(llm_plugin_runtime.clone())
                .with_pii_engines(pii_engines.clone()),
        ))
        .merge(octra_mcp_gateway::control_router(McpControlState::new(
            mcp_runtime.clone(),
            storage.clone(),
        )))
        .layer(axum::middleware::from_fn_with_state(
            AuditState {
                storage: storage.clone(),
            },
            octra_api_gateway::record_control_audit,
        ))
        .layer(axum::middleware::from_fn_with_state(
            AuthState { token },
            octra_types::auth::require_bearer,
        ))
        .layer(octra_types::cors_layer());

    let data_app = Router::new()
        .merge(octra_llm_gateway::data_plane_router(
            LlmDataState::new(llm_runtime, storage.clone(), config.clone())
                .with_plugins(llm_plugin_runtime.clone()),
        ))
        .merge(octra_mcp_gateway::data_plane_router(McpDataState::new(
            mcp_runtime,
            storage.clone(),
            config.clone(),
        )))
        .merge(octra_api_gateway::data_plane_router(
            ApiDataState::with_plugins(api_runtime, config.clone(), api_metrics, plugin_runtime)
                .with_storage(storage.clone()),
        ));

    let control_listener = tokio::net::TcpListener::bind(&config.daemon.control_bind)
        .await
        .with_context(|| {
            format!(
                "cannot bind control plane at {}",
                config.daemon.control_bind
            )
        })?;
    let data_listener = tokio::net::TcpListener::bind(&config.gateway.listen_bind)
        .await
        .with_context(|| format!("cannot bind data plane at {}", config.gateway.listen_bind))?;

    let control_address = control_listener
        .local_addr()
        .context("cannot read control listener address")?;
    let data_address = data_listener
        .local_addr()
        .context("cannot read data listener address")?;

    tracing::info!(
        control = %control_address,
        data_plane = %data_address,
        version = octra_core::VERSION,
        token_file = %workspace.token_file().display(),
        home = %workspace.home.display(),
        "octra gateway listening"
    );

    let shutdown = CancellationToken::new();
    tokio::spawn(shutdown_signal(shutdown.clone()));
    tokio::spawn(health_check_loop(health_runtime, shutdown.clone()));
    if config.storage.retention.enabled {
        maintenance::spawn(
            storage.clone(),
            config.storage.retention.clone(),
            shutdown.clone(),
        );
    }

    let control_server = axum::serve(control_listener, control_app)
        .with_graceful_shutdown(wait_for_shutdown(shutdown.clone()));
    let data_server = axum::serve(
        data_listener,
        data_app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(wait_for_shutdown(shutdown.clone()));

    tokio::try_join!(
        async move { control_server.await.context("control api server failed") },
        async move { data_server.await.context("data plane server failed") },
    )?;

    tracing::info!("octra daemon stopped");
    Ok(())
}

async fn wait_for_shutdown(cancel: CancellationToken) {
    cancel.cancelled().await;
}

async fn health_check_loop(runtime: Arc<GatewayRuntime>, shutdown: CancellationToken) {
    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = ticker.tick() => runtime.check_health().await,
        }
    }
}

async fn shutdown_signal(cancel: CancellationToken) {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    tracing::info!("shutdown signal received");
    cancel.cancel();
}
