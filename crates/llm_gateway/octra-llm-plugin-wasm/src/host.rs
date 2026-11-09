//! Host state for one plugin instance.
//!
//! LLM plugins receive structured logging, a clock and, when the manifest
//! declares `KeyValue`, a host-owned per-plugin key-value store. Network and
//! secret handles remain native-only. A fresh WASI context still backs
//! components that import WASI for their own runtime.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use wasmtime::component::ResourceTable;
use wasmtime::{StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use octra_api_plugin_types::PluginStorage;

use crate::octra::llm_plugin::{host as bindings, types as wit};

pub struct LlmHostState {
    pub limits: StoreLimits,
    plugin: String,
    storage: Option<Arc<dyn PluginStorage>>,
    wasi: WasiCtx,
    wasi_table: ResourceTable,
}

impl LlmHostState {
    pub fn new(
        plugin: &str,
        max_memory_bytes: usize,
        storage: Option<Arc<dyn PluginStorage>>,
    ) -> Self {
        Self {
            limits: StoreLimitsBuilder::new()
                .memory_size(max_memory_bytes)
                .build(),
            plugin: plugin.to_string(),
            storage,
            wasi: WasiCtxBuilder::new().build(),
            wasi_table: ResourceTable::new(),
        }
    }

    fn storage(&self) -> Result<&Arc<dyn PluginStorage>, String> {
        self.storage
            .as_ref()
            .ok_or_else(|| "key-value capability is not enabled for this plugin".to_string())
    }
}

impl WasiView for LlmHostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.wasi_table,
        }
    }
}

impl wit::Host for LlmHostState {}

impl bindings::Host for LlmHostState {
    fn log(&mut self, level: wit::LogLevel, message: String) {
        match level {
            wit::LogLevel::Trace => tracing::trace!(plugin = %self.plugin, "{message}"),
            wit::LogLevel::Debug => tracing::debug!(plugin = %self.plugin, "{message}"),
            wit::LogLevel::Info => tracing::info!(plugin = %self.plugin, "{message}"),
            wit::LogLevel::Warn => tracing::warn!(plugin = %self.plugin, "{message}"),
            wit::LogLevel::Error => tracing::error!(plugin = %self.plugin, "{message}"),
        }
    }

    fn now_unix(&mut self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs().min(i64::MAX as u64) as i64)
            .unwrap_or(0)
    }

    fn kv_get(&mut self, key: String) -> Result<Option<String>, String> {
        let storage = self.storage()?;
        storage
            .kv_get(&key)
            .map_err(|error| error.to_string())?
            .map(|value| {
                String::from_utf8(value)
                    .map_err(|_| "stored key-value is not valid UTF-8".to_string())
            })
            .transpose()
    }

    fn kv_put(
        &mut self,
        key: String,
        value: String,
        ttl_seconds: Option<u32>,
    ) -> Result<(), String> {
        let storage = self.storage()?;
        storage
            .kv_put(
                &key,
                value.as_bytes(),
                ttl_seconds.map(|seconds| Duration::from_secs(u64::from(seconds))),
            )
            .map_err(|error| error.to_string())
    }

    fn kv_delete(&mut self, key: String) -> Result<bool, String> {
        let storage = self.storage()?;
        storage.kv_delete(&key).map_err(|error| error.to_string())
    }

    fn kv_incr(
        &mut self,
        key: String,
        delta: i64,
        ttl_seconds: Option<u32>,
    ) -> Result<i64, String> {
        let storage = self.storage()?;
        storage
            .kv_incr(
                &key,
                delta,
                ttl_seconds.map(|seconds| Duration::from_secs(u64::from(seconds))),
            )
            .map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use octra_plugin_storage::PluginStore;

    #[test]
    fn kv_round_trips_and_requires_the_capability() {
        let store = Arc::new(PluginStore::open_in_memory("kv-test", Vec::new()).unwrap());
        let mut state = LlmHostState::new("kv-test", 1 << 20, Some(store));
        assert_eq!(
            bindings::Host::kv_get(&mut state, "k".into()).unwrap(),
            None
        );
        bindings::Host::kv_put(&mut state, "k".into(), "v".into(), None).unwrap();
        assert_eq!(
            bindings::Host::kv_get(&mut state, "k".into()).unwrap(),
            Some("v".to_string())
        );
        assert_eq!(
            bindings::Host::kv_incr(&mut state, "n".into(), 2, None).unwrap(),
            2
        );
        assert_eq!(
            bindings::Host::kv_incr(&mut state, "n".into(), 3, None).unwrap(),
            5
        );
        assert!(bindings::Host::kv_delete(&mut state, "k".into()).unwrap());
        assert_eq!(
            bindings::Host::kv_get(&mut state, "k".into()).unwrap(),
            None
        );

        let mut denied = LlmHostState::new("kv-test", 1 << 20, None);
        assert!(bindings::Host::kv_get(&mut denied, "k".into()).is_err());
        assert!(bindings::Host::kv_put(&mut denied, "k".into(), "v".into(), None).is_err());
    }
}
