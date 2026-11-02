use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use octra_api_plugin_types::{
    Capability, EntityQuery, LogLevel as ApiLogLevel, PluginHost, PluginStorage,
};
use wasmtime::component::ResourceTable;
use wasmtime::{StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::octra::api_plugin::{host as bindings, types as wit};

pub struct HostState {
    pub host: Arc<dyn PluginHost>,
    pub request_id: String,
    pub limits: StoreLimits,
    auth: AuthView,
    wasi: WasiCtx,
    wasi_table: ResourceTable,
}

/// Auth state visible to the component during one call. The adapter seeds it
/// from the gateway state and applies `pending` afterwards.
#[derive(Default)]
struct AuthView {
    current: Option<wit::AuthIdentity>,
    authenticated: bool,
    pending: Option<wit::AuthIdentity>,
    active: bool,
}

impl HostState {
    pub fn new(host: Arc<dyn PluginHost>, max_memory_bytes: usize) -> Self {
        Self {
            host,
            request_id: String::new(),
            limits: StoreLimitsBuilder::new()
                .memory_size(max_memory_bytes)
                .build(),
            auth: AuthView::default(),
            wasi: WasiCtxBuilder::new().build(),
            wasi_table: ResourceTable::new(),
        }
    }

    pub fn begin_auth(&mut self, current: Option<wit::AuthIdentity>, authenticated: bool) {
        self.auth.current = current;
        self.auth.authenticated = authenticated;
        self.auth.pending = None;
        self.auth.active = true;
    }

    pub fn take_pending_auth(&mut self) -> Option<wit::AuthIdentity> {
        self.auth.active = false;
        self.auth.pending.take()
    }

    fn require(&self, capability: Capability) -> Result<(), String> {
        if self.host.capabilities().contains(&capability) {
            Ok(())
        } else {
            Err(format!("capability '{capability:?}' is not granted"))
        }
    }

    fn storage(&self) -> &dyn PluginStorage {
        self.host.storage()
    }
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.wasi_table,
        }
    }
}

impl wit::Host for HostState {}

impl bindings::Host for HostState {
    fn log(&mut self, level: bindings::LogLevel, message: String) {
        if self.host.capabilities().contains(&Capability::Log) {
            self.host.log(map_level(level), &message);
        }
    }

    fn now_unix(&mut self) -> i64 {
        now_unix()
    }

    fn instance_id(&mut self) -> String {
        self.host.plugin_instance_id().to_string()
    }

    fn authenticate(&mut self, identity: bindings::AuthIdentity) -> Result<(), String> {
        self.require(Capability::Auth)?;
        if !self.auth.active {
            return Err("authenticate is only available during the request phase".to_string());
        }
        self.auth.authenticated = true;
        self.auth.current = Some(identity.clone());
        self.auth.pending = Some(identity);
        Ok(())
    }

    fn authenticated(&mut self) -> bool {
        self.auth.authenticated
    }

    fn current_auth(&mut self) -> Option<bindings::AuthIdentity> {
        self.auth.current.clone()
    }

    fn kv_get(&mut self, key: String) -> Result<Option<String>, String> {
        self.require(Capability::KeyValue)?;
        let value = self
            .storage()
            .kv_get(&key)
            .map_err(|error| error.to_string())?;
        Ok(value.map(|bytes| String::from_utf8_lossy(&bytes).into_owned()))
    }

    fn kv_put(
        &mut self,
        key: String,
        value: String,
        ttl_seconds: Option<u32>,
    ) -> Result<(), String> {
        self.require(Capability::KeyValue)?;
        let ttl = ttl_seconds.map(|seconds| Duration::from_secs(u64::from(seconds)));
        self.storage()
            .kv_put(&key, value.as_bytes(), ttl)
            .map_err(|error| error.to_string())
    }

    fn kv_delete(&mut self, key: String) -> Result<bool, String> {
        self.require(Capability::KeyValue)?;
        self.storage()
            .kv_delete(&key)
            .map_err(|error| error.to_string())
    }

    fn kv_incr(
        &mut self,
        key: String,
        delta: i64,
        ttl_seconds: Option<u32>,
    ) -> Result<i64, String> {
        self.require(Capability::KeyValue)?;
        let ttl = ttl_seconds.map(|seconds| Duration::from_secs(u64::from(seconds)));
        self.storage()
            .kv_incr(&key, delta, ttl)
            .map_err(|error| error.to_string())
    }

    fn entity_get(&mut self, entity: String, key_json: String) -> Result<Option<String>, String> {
        self.require(Capability::StorageEntities)?;
        let key = parse_json(&key_json)?;
        let value = self
            .storage()
            .get(&entity, &key)
            .map_err(|error| error.to_string())?;
        match value {
            Some(value) => Ok(Some(serde_json::to_string(&value).map_err(stringify)?)),
            None => Ok(None),
        }
    }

    fn entity_upsert(&mut self, entity: String, values_json: String) -> Result<(), String> {
        self.require(Capability::StorageEntities)?;
        let values = parse_json(&values_json)?;
        self.storage()
            .upsert(&entity, &values)
            .map_err(|error| error.to_string())
    }

    fn entity_increment(
        &mut self,
        entity: String,
        key_json: String,
        field: String,
        delta: i64,
        extra_json: Option<String>,
    ) -> Result<i64, String> {
        self.require(Capability::StorageEntities)?;
        let key = parse_json(&key_json)?;
        let extra = match extra_json {
            Some(raw) => Some(parse_json(&raw)?),
            None => None,
        };
        self.storage()
            .increment(&entity, &key, &field, delta, extra.as_ref())
            .map_err(|error| error.to_string())
    }

    fn entity_delete(&mut self, entity: String, key_json: String) -> Result<bool, String> {
        self.require(Capability::StorageEntities)?;
        let key = parse_json(&key_json)?;
        self.storage()
            .delete(&entity, &key)
            .map_err(|error| error.to_string())
    }

    fn entity_query(&mut self, entity: String, query_json: String) -> Result<String, String> {
        self.require(Capability::StorageEntities)?;
        let query: EntityQuery = serde_json::from_str(&query_json).map_err(stringify)?;
        let values = self
            .storage()
            .query(&entity, &query)
            .map_err(|error| error.to_string())?;
        serde_json::to_string(&values).map_err(stringify)
    }
}

fn parse_json(raw: &str) -> Result<serde_json::Value, String> {
    serde_json::from_str(raw).map_err(stringify)
}

fn stringify(error: serde_json::Error) -> String {
    error.to_string()
}

fn map_level(level: wit::LogLevel) -> ApiLogLevel {
    match level {
        wit::LogLevel::Trace => ApiLogLevel::Trace,
        wit::LogLevel::Debug => ApiLogLevel::Debug,
        wit::LogLevel::Info => ApiLogLevel::Info,
        wit::LogLevel::Warn => ApiLogLevel::Warn,
        wit::LogLevel::Error => ApiLogLevel::Error,
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use octra_api_plugin_types::EntityQuery;
    use serde_json::Value;

    struct TestHost {
        capabilities: Vec<Capability>,
    }

    struct EmptyStorage;

    impl PluginStorage for EmptyStorage {
        fn kv_get(
            &self,
            _key: &str,
        ) -> Result<Option<Vec<u8>>, octra_api_plugin_types::StorageError> {
            Err(octra_api_plugin_types::StorageError::Unavailable)
        }

        fn kv_put(
            &self,
            _key: &str,
            _value: &[u8],
            _ttl: Option<Duration>,
        ) -> Result<(), octra_api_plugin_types::StorageError> {
            Err(octra_api_plugin_types::StorageError::Unavailable)
        }

        fn kv_delete(&self, _key: &str) -> Result<bool, octra_api_plugin_types::StorageError> {
            Err(octra_api_plugin_types::StorageError::Unavailable)
        }

        fn kv_incr(
            &self,
            _key: &str,
            _delta: i64,
            _ttl: Option<Duration>,
        ) -> Result<i64, octra_api_plugin_types::StorageError> {
            Err(octra_api_plugin_types::StorageError::Unavailable)
        }

        fn get(
            &self,
            _entity: &str,
            _key: &Value,
        ) -> Result<Option<Value>, octra_api_plugin_types::StorageError> {
            Ok(None)
        }

        fn upsert(
            &self,
            _entity: &str,
            _values: &Value,
        ) -> Result<(), octra_api_plugin_types::StorageError> {
            Err(octra_api_plugin_types::StorageError::Unavailable)
        }

        fn increment(
            &self,
            _entity: &str,
            _key: &Value,
            _field: &str,
            _delta: i64,
            _extra: Option<&Value>,
        ) -> Result<i64, octra_api_plugin_types::StorageError> {
            Err(octra_api_plugin_types::StorageError::Unavailable)
        }

        fn delete(
            &self,
            _entity: &str,
            _key: &Value,
        ) -> Result<bool, octra_api_plugin_types::StorageError> {
            Err(octra_api_plugin_types::StorageError::Unavailable)
        }

        fn query(
            &self,
            _entity: &str,
            _query: &EntityQuery,
        ) -> Result<Vec<Value>, octra_api_plugin_types::StorageError> {
            Err(octra_api_plugin_types::StorageError::Unavailable)
        }
    }

    static EMPTY: EmptyStorage = EmptyStorage;

    impl PluginHost for TestHost {
        fn plugin_name(&self) -> &str {
            "test"
        }

        fn plugin_instance_id(&self) -> &str {
            "test-instance"
        }

        fn capabilities(&self) -> &[Capability] {
            &self.capabilities
        }

        fn storage(&self) -> &dyn PluginStorage {
            &EMPTY
        }

        fn log(&self, _level: ApiLogLevel, _message: &str) {}
    }

    fn state(capabilities: Vec<Capability>) -> HostState {
        HostState::new(Arc::new(TestHost { capabilities }), 1 << 20)
    }

    fn identity(consumer: &str) -> wit::AuthIdentity {
        wit::AuthIdentity {
            consumer_id: Some(consumer.to_string()),
            consumer_username: None,
            consumer_custom_id: None,
            credential_id: None,
            credential_type: None,
        }
    }

    #[test]
    fn authenticate_requires_the_auth_capability() {
        let mut state = state(Vec::new());
        state.begin_auth(None, false);
        assert!(bindings::Host::authenticate(&mut state, identity("c1")).is_err());
        assert!(state.take_pending_auth().is_none());
    }

    #[test]
    fn authenticate_is_only_available_during_the_request_phase() {
        let mut state = state(vec![Capability::Auth]);
        assert!(bindings::Host::authenticate(&mut state, identity("c1")).is_err());

        state.begin_auth(None, false);
        bindings::Host::authenticate(&mut state, identity("c1")).unwrap();
        assert!(bindings::Host::authenticated(&mut state));
        assert_eq!(
            bindings::Host::current_auth(&mut state)
                .unwrap()
                .consumer_id
                .as_deref(),
            Some("c1")
        );

        let pending = state.take_pending_auth().unwrap();
        assert_eq!(pending.consumer_id.as_deref(), Some("c1"));
        assert!(state.take_pending_auth().is_none());
        assert!(bindings::Host::authenticate(&mut state, identity("c1")).is_err());
    }

    #[test]
    fn seeds_a_previously_authenticated_request() {
        let mut state = state(vec![Capability::Auth]);
        state.begin_auth(Some(identity("c1")), true);
        assert!(bindings::Host::authenticated(&mut state));
        assert_eq!(
            bindings::Host::current_auth(&mut state)
                .unwrap()
                .consumer_id
                .as_deref(),
            Some("c1")
        );
        assert!(state.take_pending_auth().is_none());
    }
}
