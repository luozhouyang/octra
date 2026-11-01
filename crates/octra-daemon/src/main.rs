mod token;

use std::sync::Arc;
use std::time::Instant;

use anyhow::Context;
use octra_config::Workspace;
use octra_gateway_api::AppState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let workspace = Workspace::resolve().context("cannot resolve the Octra workspace")?;
    let config = octra_config::load_or_create(&workspace).context("cannot load configuration")?;

    octra_observability::init(&config.logging.level, config.logging.format)
        .context("cannot initialize logging")?;

    let database = workspace.database_path(&config);
    tracing::info!(path = %database.display(), "opening sqlite storage");

    let storage = Arc::new(
        octra_storage::Storage::connect(&database)
            .await
            .context("cannot open sqlite storage")?,
    );

    let runtime = Arc::new(
        octra_gateway_runtime::GatewayRuntime::bootstrap(storage.clone())
            .await
            .context("cannot build the runtime snapshot")?,
    );

    let token =
        token::load_or_create(&workspace.token_file()).context("cannot prepare control token")?;

    let state = AppState {
        runtime,
        storage,
        config: Arc::new(config.clone()),
        token: Arc::new(token),
        started_at: Instant::now(),
    };

    let app = octra_gateway_api::router(state);
    let listener = tokio::net::TcpListener::bind(&config.daemon.control_bind)
        .await
        .with_context(|| {
            format!(
                "cannot bind control plane at {}",
                config.daemon.control_bind
            )
        })?;
    let address = listener
        .local_addr()
        .context("cannot read listener address")?;

    tracing::info!(
        address = %address,
        version = octra_core::VERSION,
        token_file = %workspace.token_file().display(),
        home = %workspace.home.display(),
        "octra control api listening"
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("control api server failed")?;

    tracing::info!("octra daemon stopped");
    Ok(())
}

async fn shutdown_signal() {
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
}
