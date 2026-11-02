use std::collections::HashMap;
use std::sync::Arc;

use octra_api_core::PluginInstance;
use octra_api_plugin_types::{
    Capability, EntityQuery, EntitySpec, LogLevel, PluginHost, PluginHostProvider, PluginManifest,
    PluginStorage, StorageError,
};
use octra_plugin_storage::{PluginStorageError, PluginStorageRoot, PluginStore};
use parking_lot::{Mutex, RwLock};
use serde_json::Value;

pub struct InstalledWasmPlugin {
    pub manifest: PluginManifest,
    pub entities: Vec<EntitySpec>,
    pub approved_capabilities: Vec<Capability>,
}

impl InstalledWasmPlugin {
    pub fn new(manifest: PluginManifest, approved_capabilities: Vec<Capability>) -> Self {
        let entities = manifest.entities.clone();
        Self {
            manifest,
            entities,
            approved_capabilities,
        }
    }
}

pub struct WasmHostProvider {
    storage_root: Arc<PluginStorageRoot>,
    installed: RwLock<HashMap<String, InstalledWasmPlugin>>,
    stores: Mutex<HashMap<String, Arc<PluginStore>>>,
}

impl WasmHostProvider {
    pub fn new(storage_root: Arc<PluginStorageRoot>, installed: Vec<InstalledWasmPlugin>) -> Self {
        let installed = installed
            .into_iter()
            .map(|plugin| (plugin.manifest.name.clone(), plugin))
            .collect();
        Self {
            storage_root,
            installed: RwLock::new(installed),
            stores: Mutex::new(HashMap::new()),
        }
    }

    /// Registers a newly installed or upgraded plugin. The cached store is
    /// dropped so the next host opens it with the new entity declarations.
    pub fn install(&self, manifest: PluginManifest, approved_capabilities: Vec<Capability>) {
        let name = manifest.name.clone();
        self.installed.write().insert(
            name.clone(),
            InstalledWasmPlugin::new(manifest, approved_capabilities),
        );
        self.stores.lock().remove(&name);
    }

    pub fn contains(&self, name: &str) -> bool {
        self.installed.read().contains_key(name)
    }

    /// Current on-disk database size, if the plugin is installed and openable.
    pub fn storage_bytes(&self, name: &str) -> Option<u64> {
        let installed = self.installed.read();
        let plugin = installed.get(name)?;
        let entities = plugin.entities.clone();
        drop(installed);
        let store = self.store(name, &entities)?;
        store.size_bytes().ok()
    }

    pub fn uninstall(&self, name: &str, purge_data: bool) -> Result<(), PluginStorageError> {
        self.installed.write().remove(name);
        self.stores.lock().remove(name);
        if purge_data {
            let directory = self.storage_root.plugin_dir(name)?;
            match std::fs::remove_dir_all(&directory) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(PluginStorageError::Io(error)),
            }
        }
        Ok(())
    }

    fn store(&self, name: &str, entities: &[EntitySpec]) -> Option<Arc<PluginStore>> {
        let mut stores = self.stores.lock();
        if let Some(store) = stores.get(name) {
            return Some(store.clone());
        }
        match self.storage_root.open(name, entities.to_vec()) {
            Ok(store) => {
                let store = Arc::new(store);
                stores.insert(name.to_string(), store.clone());
                Some(store)
            }
            Err(error) => {
                tracing::warn!(plugin = name, %error, "cannot open plugin storage");
                None
            }
        }
    }
}

impl PluginHostProvider for WasmHostProvider {
    fn host_for(&self, instance: &PluginInstance) -> Arc<dyn PluginHost> {
        let installed = self.installed.read();
        let Some(plugin) = installed.get(&instance.name) else {
            return Arc::new(WasmPluginHost {
                plugin_name: instance.name.clone(),
                instance_id: instance.id.clone(),
                capabilities: Vec::new(),
                storage: None,
            });
        };
        let entities = plugin.entities.clone();
        let capabilities = plugin.approved_capabilities.clone();
        drop(installed);
        let storage = self.store(&instance.name, &entities);
        Arc::new(WasmPluginHost {
            plugin_name: instance.name.clone(),
            instance_id: instance.id.clone(),
            capabilities,
            storage,
        })
    }
}

struct WasmPluginHost {
    plugin_name: String,
    instance_id: String,
    capabilities: Vec<Capability>,
    storage: Option<Arc<PluginStore>>,
}

impl PluginHost for WasmPluginHost {
    fn plugin_name(&self) -> &str {
        &self.plugin_name
    }

    fn plugin_instance_id(&self) -> &str {
        &self.instance_id
    }

    fn capabilities(&self) -> &[Capability] {
        &self.capabilities
    }

    fn storage(&self) -> &dyn PluginStorage {
        match &self.storage {
            Some(storage) => storage.as_ref(),
            None => &UNAVAILABLE_STORAGE,
        }
    }

    fn log(&self, level: LogLevel, message: &str) {
        match level {
            LogLevel::Trace => {
                tracing::trace!(plugin = %self.plugin_name, instance = %self.instance_id, "{message}")
            }
            LogLevel::Debug => {
                tracing::debug!(plugin = %self.plugin_name, instance = %self.instance_id, "{message}")
            }
            LogLevel::Info => {
                tracing::info!(plugin = %self.plugin_name, instance = %self.instance_id, "{message}")
            }
            LogLevel::Warn => {
                tracing::warn!(plugin = %self.plugin_name, instance = %self.instance_id, "{message}")
            }
            LogLevel::Error => {
                tracing::error!(plugin = %self.plugin_name, instance = %self.instance_id, "{message}")
            }
        }
    }
}

static UNAVAILABLE_STORAGE: UnavailableStorage = UnavailableStorage;

struct UnavailableStorage;

impl PluginStorage for UnavailableStorage {
    fn kv_get(&self, _key: &str) -> Result<Option<Vec<u8>>, StorageError> {
        Err(StorageError::Unavailable)
    }

    fn kv_put(
        &self,
        _key: &str,
        _value: &[u8],
        _ttl: Option<std::time::Duration>,
    ) -> Result<(), StorageError> {
        Err(StorageError::Unavailable)
    }

    fn kv_delete(&self, _key: &str) -> Result<bool, StorageError> {
        Err(StorageError::Unavailable)
    }

    fn kv_incr(
        &self,
        _key: &str,
        _delta: i64,
        _ttl: Option<std::time::Duration>,
    ) -> Result<i64, StorageError> {
        Err(StorageError::Unavailable)
    }

    fn get(&self, _entity: &str, _key: &Value) -> Result<Option<Value>, StorageError> {
        Err(StorageError::Unavailable)
    }

    fn upsert(&self, _entity: &str, _values: &Value) -> Result<(), StorageError> {
        Err(StorageError::Unavailable)
    }

    fn increment(
        &self,
        _entity: &str,
        _key: &Value,
        _field: &str,
        _delta: i64,
        _extra: Option<&Value>,
    ) -> Result<i64, StorageError> {
        Err(StorageError::Unavailable)
    }

    fn delete(&self, _entity: &str, _key: &Value) -> Result<bool, StorageError> {
        Err(StorageError::Unavailable)
    }

    fn query(&self, _entity: &str, _query: &EntityQuery) -> Result<Vec<Value>, StorageError> {
        Err(StorageError::Unavailable)
    }
}
