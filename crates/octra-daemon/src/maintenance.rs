//! Periodic retention pruning.

use std::sync::Arc;
use std::time::Duration;

use octra_config::RetentionConfig;
use octra_storage::{RetentionPolicy, Storage};
use tokio_util::sync::CancellationToken;

pub fn spawn(storage: Arc<Storage>, config: RetentionConfig, shutdown: CancellationToken) {
    tokio::spawn(run(storage, config, shutdown));
}

async fn run(storage: Arc<Storage>, config: RetentionConfig, shutdown: CancellationToken) {
    let policy = RetentionPolicy {
        llm_usage_days: config.llm_usage_days,
        request_logs_days: config.request_logs_days,
        mcp_invocations_days: config.mcp_invocations_days,
        audit_logs_days: config.audit_logs_days,
        usage_rollup_days: config.usage_rollup_days,
        batch_size: config.batch_size,
    };
    let mut ticker = tokio::time::interval(Duration::from_secs(config.interval_secs));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ticker.tick().await;
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = ticker.tick() => {
                let started = std::time::Instant::now();
                match storage.prune(&policy).await {
                    Ok(report) => tracing::info!(
                        ?report,
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        "retention prune complete"
                    ),
                    Err(error) => tracing::warn!(%error, "retention prune failed"),
                }
            }
        }
    }
}
