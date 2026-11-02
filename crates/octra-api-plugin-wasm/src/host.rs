use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use octra_api_plugin_types::{
    Capability, EntityQuery, LogLevel as ApiLogLevel, PluginHost, PluginStorage,
};
use wasmtime::{StoreLimits, StoreLimitsBuilder};

use crate::octra::api_plugin::{host as bindings, types as wit};

pub struct HostState {
    pub host: Arc<dyn PluginHost>,
    pub request_id: String,
    pub limits: StoreLimits,
}

impl HostState {
    pub fn new(host: Arc<dyn PluginHost>, max_memory_bytes: usize) -> Self {
        Self {
            host,
            request_id: String::new(),
            limits: StoreLimitsBuilder::new()
                .memory_size(max_memory_bytes)
                .build(),
        }
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

    fn kv_get(&mut self, key: String) -> Result<Option<String>, String> {
        self.require(Capability::KeyValue)?;
        let value = self.storage().kv_get(&key).map_err(|error| error.to_string())?;
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

    fn entity_get(
        &mut self,
        entity: String,
        key_json: String,
    ) -> Result<Option<String>, String> {
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
