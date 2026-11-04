//! Host state for one plugin instance.
//!
//! LLM plugins receive no network or storage handles in the first version;
//! the only host services are structured logging and a clock. A fresh WASI
//! context still backs components that import WASI for their own runtime.

use std::time::{SystemTime, UNIX_EPOCH};

use wasmtime::component::ResourceTable;
use wasmtime::{StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::octra::llm_plugin::{host as bindings, types as wit};

pub struct LlmHostState {
    pub limits: StoreLimits,
    plugin: String,
    wasi: WasiCtx,
    wasi_table: ResourceTable,
}

impl LlmHostState {
    pub fn new(plugin: &str, max_memory_bytes: usize) -> Self {
        Self {
            limits: StoreLimitsBuilder::new()
                .memory_size(max_memory_bytes)
                .build(),
            plugin: plugin.to_string(),
            wasi: WasiCtxBuilder::new().build(),
            wasi_table: ResourceTable::new(),
        }
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
}
