mod token;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use axum::Router;
use octra_api_gateway::{ApiControlState, ApiDataState, ApiMetrics, AuditState};
use octra_api_plugin_runtime::{PluginRegistry, PluginRuntime};
use octra_config::Workspace;
use octra_llm_gateway::{LlmControlState, LlmDataState, LlmPluginRuntime, LlmRuntime};
use octra_mcp_gateway::{McpControlState, McpDataState, McpRuntime};
use octra_runtime::GatewayRuntime;
use octra_types::{AppState, AuthState};
use tokio_util::sync::CancellationToken;

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
    let plugin_instances = storage
        .api
        .list_plugin_instances()
        .await
        .context("cannot load plugin instances")?;
    let plugin_runtime = Arc::new(
        PluginRuntime::with_plugins(Arc::new(plugin_registry), plugin_instances)
            .context("cannot build the plugin runtime")?,
    );

    let llm_plugin_runtime = Arc::new(
        LlmPluginRuntime::bootstrap(
            storage.clone(),
            Arc::new(octra_llm_plugin_builtin::registry()),
        )
        .await
        .context("cannot build the LLM plugin runtime")?,
    );

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

    let control_app = Router::new()
        .merge(octra_types::core_router(app_state))
        .merge(octra_api_gateway::control_router(
            ApiControlState::with_plugins(
                storage.clone(),
                api_runtime.clone(),
                api_metrics.clone(),
                plugin_runtime.clone(),
            ),
        ))
        .merge(octra_llm_gateway::control_router(
            LlmControlState::new(llm_runtime.clone(), storage.clone())
                .with_plugins(llm_plugin_runtime.clone()),
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
