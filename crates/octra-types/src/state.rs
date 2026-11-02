use std::sync::Arc;
use std::time::Instant;

use octra_config::OctraConfig;
use octra_runtime::GatewayRuntime;
use octra_storage::Storage;

#[derive(Clone)]
pub struct AppState {
    pub runtime: Arc<GatewayRuntime>,
    pub storage: Arc<Storage>,
    pub config: Arc<OctraConfig>,
    pub token: Arc<String>,
    pub started_at: Instant,
}
