//! Lifecycle bridge between the shared plugin installer and the LLM runtime.

use std::sync::Arc;

use octra_api_gateway::LlmWasmLifecycle;
use octra_api_plugin_types::PluginManifest;
use octra_api_plugin_wasm::WasmEngine;
use octra_llm_gateway::LlmPluginRuntime;
use octra_llm_plugin_runtime::LlmPluginRegistry;
use octra_llm_plugin_wasm::LlmWasmPluginFactory;
use octra_plugin_storage::PluginStorageRoot;

/// Bridges the API installer to the LLM plugin registry. The runtime is
/// reloaded after every change; a failed reload restores the previous factory
/// so the catalog and the compiled chains stay consistent.
pub struct LlmWasmRuntime {
    registry: Arc<LlmPluginRegistry>,
    runtime: Arc<LlmPluginRuntime>,
    engine: Arc<WasmEngine>,
    storage_root: Option<Arc<PluginStorageRoot>>,
}

impl LlmWasmRuntime {
    pub fn new(
        registry: Arc<LlmPluginRegistry>,
        runtime: Arc<LlmPluginRuntime>,
        engine: Arc<WasmEngine>,
        storage_root: Option<Arc<PluginStorageRoot>>,
    ) -> Self {
        Self {
            registry,
            runtime,
            engine,
            storage_root,
        }
    }
}

#[async_trait::async_trait]
impl LlmWasmLifecycle for LlmWasmRuntime {
    async fn install(
        &self,
        manifest: PluginManifest,
        artifact_dir: std::path::PathBuf,
        replaces_hash: Option<&str>,
    ) -> Result<(), String> {
        let factory = LlmWasmPluginFactory::from_artifact(
            &self.engine,
            &artifact_dir,
            self.storage_root.clone(),
        )
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

    fn registered(&self, name: &str) -> bool {
        self.registry.factory(name).is_some()
    }
}
