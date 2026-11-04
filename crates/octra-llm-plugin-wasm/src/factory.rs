//! Loads LLM plugin artifacts and turns them into factories.

use std::path::Path;

use octra_api_plugin_types::{PluginError, PluginKind};
use octra_api_plugin_wasm::{WasmEngine, WasmLimits};

use crate::adapter::LlmWasmPluginFactory;

impl LlmWasmPluginFactory {
    /// Loads `plugin.json` and `plugin.wasm` from an artifact directory.
    pub fn from_artifact(engine: &WasmEngine, artifact_dir: &Path) -> Result<Self, PluginError> {
        let layout =
            octra_plugin_storage::validate_artifact_dir_for_kind(artifact_dir, PluginKind::Llm)
                .map_err(|error| {
                    PluginError::fail_closed(format!("invalid LLM plugin artifact: {error}"))
                })?;
        let component = engine
            .component(&layout.hash, &artifact_dir.join("plugin.wasm"))
            .map_err(|error| {
                PluginError::fail_closed(format!("cannot load LLM plugin component: {error}"))
            })?;
        let mut limits: WasmLimits = engine.limits().clone();
        limits.timeout = layout.manifest.timeout();
        crate::adapter::probe_component(&component, &limits, &layout.manifest.name)?;
        Ok(Self::new(layout.manifest, component, limits))
    }
}
