//! Registry of configured PII engines.
//!
//! The daemon rebuilds the registry after any control-plane mutation. Chains
//! compiled afterwards resolve engines by name; in-flight requests keep the
//! client they already hold.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use octra_pii_types::{EngineReadiness, PiiDetectorClient, PiiEngineResolver};
use octra_storage::LlmPiiEngineRecord;
use serde::Serialize;

use super::client::build_client;

#[derive(Debug, Clone, Serialize)]
pub struct EngineStatus {
    pub id: String,
    pub name: String,
    pub mode: String,
    pub transport: String,
    pub enabled: bool,
    pub readiness: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

#[derive(Default)]
pub struct PiiEngineRuntime {
    engines: RwLock<HashMap<String, Arc<dyn PiiDetectorClient>>>,
    status: RwLock<HashMap<String, EngineStatus>>,
}

impl PiiEngineRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    /// Rebuilds the client map from stored definitions. Returns warnings for
    /// definitions that could not be built; they stay visible in status.
    pub fn reload(&self, records: &[LlmPiiEngineRecord]) -> Vec<String> {
        let mut engines = HashMap::new();
        let mut status = HashMap::new();
        let mut warnings = Vec::new();

        for record in records {
            let mut engine_status = EngineStatus {
                id: record.id.clone(),
                name: record.name.clone(),
                mode: record.mode.clone(),
                transport: record.transport.clone(),
                enabled: record.enabled,
                readiness: "disabled",
                last_error: None,
            };
            if !record.enabled {
                status.insert(record.name.clone(), engine_status);
                continue;
            }
            if record.mode == "managed" {
                engine_status.readiness = "starting";
                engine_status.last_error =
                    Some("managed PII engines are not started yet".to_string());
                status.insert(record.name.clone(), engine_status);
                continue;
            }
            if record.mode != "external" {
                engine_status.readiness = "failed";
                engine_status.last_error = Some(format!("unknown engine mode '{}'", record.mode));
                warnings.push(format!(
                    "PII engine '{}' has unknown mode '{}'",
                    record.name, record.mode
                ));
                status.insert(record.name.clone(), engine_status);
                continue;
            }
            match build_client(record) {
                Ok(client) => {
                    engine_status.readiness = readiness_label(client.readiness());
                    engines.insert(record.name.clone(), client);
                }
                Err(error) => {
                    engine_status.readiness = "failed";
                    engine_status.last_error = Some(error.clone());
                    warnings.push(format!(
                        "PII engine '{}' could not be built: {error}",
                        record.name
                    ));
                }
            }
            status.insert(record.name.clone(), engine_status);
        }

        *self.engines.write().unwrap_or_else(|error| error.into_inner()) = engines;
        *self.status.write().unwrap_or_else(|error| error.into_inner()) = status;
        warnings
    }

    pub fn statuses(&self) -> Vec<EngineStatus> {
        let mut statuses: Vec<EngineStatus> = self
            .status
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .values()
            .cloned()
            .collect();
        statuses.sort_by(|left, right| left.name.cmp(&right.name));
        statuses
    }

    pub fn get_status(&self, name: &str) -> Option<EngineStatus> {
        self.status
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .get(name)
            .cloned()
    }
}

impl PiiEngineResolver for PiiEngineRuntime {
    fn resolve(&self, name: &str) -> Option<Arc<dyn PiiDetectorClient>> {
        self.engines
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .get(name)
            .cloned()
    }
}

fn readiness_label(readiness: EngineReadiness) -> &'static str {
    match readiness {
        EngineReadiness::Ready => "ready",
        EngineReadiness::Starting => "starting",
        EngineReadiness::Degraded => "degraded",
        EngineReadiness::Failed => "failed",
    }
}
