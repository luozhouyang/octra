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

#[derive(Clone)]
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

    /// Snapshot of the installed plugin, used to restore the runtime when an
    /// upgrade or uninstall fails partway through.
    pub fn plugin(&self, name: &str) -> Option<InstalledWasmPlugin> {
        self.installed.read().get(name).cloned()
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    use octra_api_plugin_types::{EntityFieldSpec, FailPolicy, PluginKind, PluginRuntimeKind};

    fn manifest(name: &str) -> PluginManifest {
        PluginManifest {
            name: name.to_string(),
            version: "1.0.0".to_string(),
            kind: PluginKind::Api,
            runtime: PluginRuntimeKind::Wasm,
            priority: 0,
            description: String::new(),
            timeout_ms: 250,
            fail_policy: FailPolicy::Open,
            phases: Vec::new(),
            capabilities: vec![Capability::StorageEntities],
            config_schema: None,
            api_version: "2".to_string(),
            entities: vec![EntitySpec {
                name: "demo".to_string(),
                primary_key: vec!["id".to_string()],
                fields: vec![EntityFieldSpec {
                    name: "id".to_string(),
                    field_type: "string".to_string(),
                    nullable: false,
                }],
                unique: Vec::new(),
            }],
            migrations: Vec::new(),
            admin_routes: Vec::new(),
            subscriptions: Vec::new(),
            jobs: Vec::new(),
        }
    }

    fn provider(dir: &Path) -> WasmHostProvider {
        WasmHostProvider::new(
            Arc::new(PluginStorageRoot::new(dir.join("plugin-data"))),
            vec![InstalledWasmPlugin::new(
                manifest("demo"),
                vec![Capability::StorageEntities, Capability::Log],
            )],
        )
    }

    fn instance(name: &str) -> PluginInstance {
        PluginInstance {
            id: format!("{name}-1"),
            name: name.to_string(),
            instance_name: None,
            config: serde_json::json!({}),
            enabled: true,
            protocols: Vec::new(),
            ordering: None,
            service_id: None,
            route_id: None,
            consumer_id: None,
            consumer_group_id: None,
            tags: Vec::new(),
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn exposes_approved_capabilities_and_declared_entities() {
        let dir = tempfile::tempdir().unwrap();
        let provider = provider(dir.path());
        assert!(provider.contains("demo"));

        let host = provider.host_for(&instance("demo"));
        assert_eq!(
            host.capabilities(),
            &[Capability::StorageEntities, Capability::Log]
        );
        assert_eq!(host.plugin_name(), "demo");
        assert_eq!(host.plugin_instance_id(), "demo-1");

        assert!(host.storage().kv_get("missing").unwrap().is_none());
        host.storage().kv_put("key", b"value", None).unwrap();
        assert_eq!(
            host.storage().kv_get("key").unwrap(),
            Some(b"value".to_vec())
        );
        assert!(matches!(
            host.storage()
                .upsert("not-declared", &serde_json::json!({"id": "a"})),
            Err(StorageError::NotDeclared(_))
        ));
    }

    #[test]
    fn unknown_plugins_get_no_capabilities_and_unavailable_storage() {
        let dir = tempfile::tempdir().unwrap();
        let provider = provider(dir.path());
        assert!(!provider.contains("missing"));

        let host = provider.host_for(&instance("missing"));
        assert!(host.capabilities().is_empty());
        assert!(matches!(
            host.storage().kv_get("key"),
            Err(StorageError::Unavailable)
        ));
    }

    #[test]
    fn uninstall_can_preserve_or_purge_data() {
        let dir = tempfile::tempdir().unwrap();
        let provider = provider(dir.path());
        {
            let host = provider.host_for(&instance("demo"));
            host.storage().kv_put("key", b"value", None).unwrap();
        }

        provider.uninstall("demo", false).unwrap();
        assert!(!provider.contains("demo"));
        provider.install(manifest("demo"), vec![Capability::StorageEntities]);
        let host = provider.host_for(&instance("demo"));
        assert_eq!(
            host.storage().kv_get("key").unwrap(),
            Some(b"value".to_vec())
        );
        drop(host);

        provider.uninstall("demo", true).unwrap();
        provider.install(manifest("demo"), vec![Capability::StorageEntities]);
        let host = provider.host_for(&instance("demo"));
        assert_eq!(host.storage().kv_get("key").unwrap(), None);
    }

    #[test]
    fn storage_bytes_track_the_plugin_database() {
        let dir = tempfile::tempdir().unwrap();
        let provider = provider(dir.path());
        {
            let host = provider.host_for(&instance("demo"));
            host.storage().kv_put("key", b"value", None).unwrap();
        }
        assert!(provider.storage_bytes("demo").unwrap() > 0);
        assert!(provider.storage_bytes("missing").is_none());
    }
}
