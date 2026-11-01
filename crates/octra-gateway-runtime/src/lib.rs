//! Runtime snapshot management.
//!
//! The proxy hot path must not query SQLite per request. Configuration is
//! loaded into an immutable snapshot and swapped atomically on reload.

use std::sync::Arc;

use chrono::Utc;
use octra_core::{Result, RouteSummary, ServiceSummary, SnapshotInfo};
use octra_storage::{RuntimeData, Storage};
use tokio::sync::RwLock;

#[derive(Debug, Clone)]
pub struct RuntimeSnapshot {
    pub info: SnapshotInfo,
    pub services: Vec<ServiceSummary>,
    pub routes: Vec<RouteSummary>,
}

impl RuntimeSnapshot {
    fn from_data(data: RuntimeData) -> Self {
        Self {
            info: SnapshotInfo {
                generated_at: Utc::now(),
                counts: data.counts,
            },
            services: data.services,
            routes: data.routes,
        }
    }
}

pub struct GatewayRuntime {
    storage: Arc<Storage>,
    snapshot: RwLock<Arc<RuntimeSnapshot>>,
}

impl GatewayRuntime {
    pub async fn bootstrap(storage: Arc<Storage>) -> Result<Self> {
        let data = storage.load_runtime_data().await?;
        Ok(Self {
            storage,
            snapshot: RwLock::new(Arc::new(RuntimeSnapshot::from_data(data))),
        })
    }

    /// Loads a fresh snapshot and atomically replaces the current one.
    /// In-flight requests keep the snapshot they started with.
    pub async fn reload(&self) -> Result<SnapshotInfo> {
        let data = self.storage.load_runtime_data().await?;
        let next = Arc::new(RuntimeSnapshot::from_data(data));
        let info = next.info.clone();

        let mut guard = self.snapshot.write().await;
        *guard = next;

        Ok(info)
    }

    pub async fn snapshot(&self) -> Arc<RuntimeSnapshot> {
        self.snapshot.read().await.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reload_reflects_new_configuration() {
        let storage = Arc::new(Storage::connect_in_memory().await.unwrap());
        let runtime = GatewayRuntime::bootstrap(storage.clone()).await.unwrap();
        assert_eq!(runtime.snapshot().await.info.counts.services, 0);

        sqlx::query(
            "INSERT INTO services (id, name, protocol, enabled) VALUES ('svc-1', 'demo', 'http', 1)",
        )
        .execute(storage.pool())
        .await
        .unwrap();

        let info = runtime.reload().await.unwrap();
        assert_eq!(info.counts.services, 1);

        let snapshot = runtime.snapshot().await;
        assert_eq!(snapshot.services.len(), 1);
        assert_eq!(snapshot.services[0].name, "demo");
    }
}
