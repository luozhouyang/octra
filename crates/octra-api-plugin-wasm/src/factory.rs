use std::path::Path;
use std::sync::Arc;

use octra_api_plugin_types::{
    ApiGatewayPlugin, ApiGatewayPluginFactory, PluginError, PluginHost, PluginManifest, PluginPhase,
};
use serde_json::Value;
use wasmtime::component::Component;

use crate::WasmApiGatewayPlugin;
use crate::engine::{WasmEngine, WasmLimits};
use crate::gate::InflightGate;
use crate::metrics::PluginStats;

pub struct WasmPluginFactory {
    manifest: PluginManifest,
    component: Arc<Component>,
    limits: WasmLimits,
    gate: Arc<InflightGate>,
    stats: Arc<PluginStats>,
}

impl WasmPluginFactory {
    pub fn from_artifact(engine: &WasmEngine, artifact_dir: &Path) -> Result<Self, PluginError> {
        let layout =
            octra_plugin_storage::validate_artifact_dir(artifact_dir).map_err(|error| {
                PluginError::fail_closed(format!("invalid plugin artifact: {error}"))
            })?;
        let component = engine
            .component(&layout.hash, &artifact_dir.join("plugin.wasm"))
            .map_err(|error| {
                PluginError::fail_closed(format!("cannot load plugin component: {error}"))
            })?;
        let mut limits = engine.limits().clone();
        limits.timeout = layout.manifest.timeout();
        let stats = engine.metrics().stats(&layout.manifest.name);
        Ok(Self {
            manifest: layout.manifest,
            component,
            limits,
            gate: engine.gate(),
            stats,
        })
    }

    /// Creates a concrete plugin instance for Admin API dispatch.
    pub fn create_admin(
        &self,
        host: Arc<dyn PluginHost>,
        config: Value,
    ) -> Result<Arc<WasmApiGatewayPlugin>, PluginError> {
        let config_json = serde_json::to_string(&config).map_err(|error| {
            PluginError::fail_closed(format!("cannot serialize plugin config: {error}"))
        })?;
        let plugin = WasmApiGatewayPlugin::new(
            self.manifest.clone(),
            self.component.clone(),
            host,
            config_json,
            self.limits.clone(),
            self.gate.clone(),
            self.stats.clone(),
        )?;
        Ok(Arc::new(plugin))
    }
}

impl ApiGatewayPluginFactory for WasmPluginFactory {
    fn manifest(&self) -> PluginManifest {
        self.manifest.clone()
    }

    fn create(&self, _id: &str, _config: Value) -> Result<Arc<dyn ApiGatewayPlugin>, PluginError> {
        Err(PluginError::fail_closed(
            "wasm plugins require a plugin host and cannot be created without one",
        ))
    }

    fn create_with_host(
        &self,
        host: Arc<dyn PluginHost>,
        _id: &str,
        config: Value,
    ) -> Result<Arc<dyn ApiGatewayPlugin>, PluginError> {
        let config_json = serde_json::to_string(&config).map_err(|error| {
            PluginError::fail_closed(format!("cannot serialize plugin config: {error}"))
        })?;
        let plugin = WasmApiGatewayPlugin::new(
            self.manifest.clone(),
            self.component.clone(),
            host,
            config_json,
            self.limits.clone(),
            self.gate.clone(),
            self.stats.clone(),
        )?;
        Ok(Arc::new(plugin))
    }

    fn phases(&self) -> Vec<PluginPhase> {
        if self.manifest.phases.is_empty() {
            vec![PluginPhase::Access, PluginPhase::Response, PluginPhase::Log]
        } else {
            self.manifest.phases.clone()
        }
    }

    fn validate(&self, config: &Value) -> Result<(), PluginError> {
        if config.is_object() {
            Ok(())
        } else {
            Err(PluginError::fail_closed(
                "plugin config must be a JSON object",
            ))
        }
    }
}
