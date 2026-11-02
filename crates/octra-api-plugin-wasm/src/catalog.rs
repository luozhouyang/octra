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
