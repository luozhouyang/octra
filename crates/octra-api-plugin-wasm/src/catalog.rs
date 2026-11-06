use std::collections::HashMap;
use std::sync::Arc;

use octra_api_plugin_types::{ApiGatewayPluginFactory, PluginFactoryResolver, PluginManifest};
use parking_lot::RwLock;

use crate::WasmPluginFactory;

#[derive(Default)]
pub struct WasmPluginCatalog {
    factories: RwLock<HashMap<String, Arc<WasmPluginFactory>>>,
}

impl WasmPluginCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, factory: Arc<WasmPluginFactory>) {
        let name = factory.manifest().name;
        self.factories.write().insert(name, factory);
    }

    pub fn remove(&self, name: &str) {
        self.factories.write().remove(name);
    }

    pub fn contains(&self, name: &str) -> bool {
        self.factories.read().contains_key(name)
    }

    pub fn get(&self, name: &str) -> Option<Arc<WasmPluginFactory>> {
        self.factories.read().get(name).cloned()
    }
}

impl PluginFactoryResolver for WasmPluginCatalog {
    fn factory(&self, name: &str) -> Option<Arc<dyn ApiGatewayPluginFactory>> {
        self.factories
            .read()
            .get(name)
            .cloned()
            .map(|factory| factory as Arc<dyn ApiGatewayPluginFactory>)
    }

    fn manifests(&self) -> Vec<PluginManifest> {
        let mut manifests: Vec<_> = self
            .factories
            .read()
            .values()
            .map(|factory| factory.manifest())
            .collect();
        manifests.sort_by(|left, right| left.name.cmp(&right.name));
        manifests
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{WasmEngine, WasmLimits};
    use serde_json::json;

    const EMPTY_COMPONENT: [u8; 8] = [0x00, 0x61, 0x73, 0x6d, 0x0d, 0x00, 0x01, 0x00];

    fn artifact(name: &str, priority: i32) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("plugin.json"),
            json!({
                "name": name,
                "version": "1.0.0",
                "kind": "api",
                "runtime": "wasm",
                "priority": priority,
                "description": "",
                "capabilities": [],
                "api_version": "2"
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(dir.path().join("plugin.wasm"), EMPTY_COMPONENT).unwrap();
        dir
    }

    fn factory(
        engine: &WasmEngine,
        name: &str,
        priority: i32,
    ) -> (tempfile::TempDir, Arc<WasmPluginFactory>) {
        let dir = artifact(name, priority);
        let factory = Arc::new(WasmPluginFactory::from_artifact(engine, dir.path()).unwrap());
        (dir, factory)
    }

    #[test]
    fn inserts_lists_looks_up_and_removes_factories() {
        let engine = WasmEngine::new(WasmLimits::default()).unwrap();
        let (_beta_dir, beta) = factory(&engine, "beta", 20);
        let (_alpha_dir, alpha) = factory(&engine, "alpha", 10);
        let catalog = WasmPluginCatalog::new();

        assert!(!catalog.contains("alpha"));
        assert!(catalog.get("alpha").is_none());
        assert!(catalog.factory("alpha").is_none());
        assert!(catalog.manifests().is_empty());

        catalog.insert(beta);
        catalog.insert(alpha.clone());
        assert!(catalog.contains("alpha"));
        assert!(Arc::ptr_eq(&catalog.get("alpha").unwrap(), &alpha));
        assert_eq!(
            catalog
                .manifests()
                .iter()
                .map(|manifest| manifest.name.as_str())
                .collect::<Vec<_>>(),
            ["alpha", "beta"]
        );
        assert_eq!(catalog.factory("beta").unwrap().manifest().priority, 20);

        catalog.insert(alpha);
        assert_eq!(catalog.manifests().len(), 2);

        catalog.remove("alpha");
        assert!(!catalog.contains("alpha"));
        assert_eq!(catalog.manifests().len(), 1);
    }
}
