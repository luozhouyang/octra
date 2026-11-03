use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Context;
use parking_lot::RwLock;
use wasmtime::component::Component;
use wasmtime::{Config, Engine};

use crate::gate::InflightGate;
use crate::metrics::PluginMetrics;

#[derive(Debug, Clone)]
pub struct WasmLimits {
    pub max_fuel: u64,
    pub max_memory_bytes: usize,
    pub timeout: Duration,
    /// Largest request or response body handed to the component.
    pub max_body_bytes: usize,
    /// Concurrent invocations across every plugin; zero means unbounded.
    pub max_inflight: usize,
}

impl Default for WasmLimits {
    fn default() -> Self {
        Self {
            max_fuel: 10_000_000,
            max_memory_bytes: 64 * 1024 * 1024,
            timeout: Duration::from_millis(250),
            max_body_bytes: 1024 * 1024,
            max_inflight: 64,
        }
    }
}

struct EngineInner {
    engine: Engine,
    stop: AtomicBool,
    gate: Arc<InflightGate>,
}

pub struct WasmEngine {
    inner: Arc<EngineInner>,
    components: RwLock<HashMap<String, Arc<Component>>>,
    limits: WasmLimits,
    metrics: Arc<PluginMetrics>,
}

impl WasmEngine {
    pub fn new(limits: WasmLimits) -> anyhow::Result<Self> {
        let mut config = Config::new();
        config.epoch_interruption(true);
        config.consume_fuel(true);
        let engine = Engine::new(&config)
            .map_err(|error| anyhow::anyhow!("cannot create the wasm engine: {error}"))?;
        let inner = Arc::new(EngineInner {
            engine,
            stop: AtomicBool::new(false),
            gate: InflightGate::new(limits.max_inflight),
        });
        let weak = Arc::downgrade(&inner);
        std::thread::Builder::new()
            .name("octra-wasm-epoch".to_string())
            .spawn(move || {
                while let Some(inner) = weak.upgrade() {
                    if inner.stop.load(Ordering::Relaxed) {
                        break;
                    }
                    inner.engine.increment_epoch();
                    drop(inner);
                    std::thread::sleep(Duration::from_millis(10));
                }
            })
            .context("cannot spawn the wasm epoch thread")?;
        Ok(Self {
            inner,
            components: RwLock::new(HashMap::new()),
            limits,
            metrics: Arc::new(PluginMetrics::default()),
        })
    }

    pub fn limits(&self) -> &WasmLimits {
        &self.limits
    }

    pub fn metrics(&self) -> &Arc<PluginMetrics> {
        &self.metrics
    }

    pub(crate) fn gate(&self) -> Arc<InflightGate> {
        self.inner.gate.clone()
    }

    pub fn engine(&self) -> &Engine {
        &self.inner.engine
    }

    pub fn component(&self, hash: &str, path: &Path) -> anyhow::Result<Arc<Component>> {
        if let Some(component) = self.components.read().get(hash) {
            return Ok(component.clone());
        }
        let component = Arc::new(Component::from_file(self.engine(), path).map_err(|error| {
            anyhow::anyhow!("cannot compile wasm component {}: {error}", path.display())
        })?);
        self.components
            .write()
            .insert(hash.to_string(), component.clone());
        Ok(component)
    }

    pub fn remove_component(&self, hash: &str) {
        self.components.write().remove(hash);
    }
}

impl Drop for WasmEngine {
    fn drop(&mut self) {
        self.inner.stop.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_limits_match_the_documented_values() {
        let limits = WasmLimits::default();
        assert_eq!(limits.max_fuel, 10_000_000);
        assert_eq!(limits.max_memory_bytes, 64 * 1024 * 1024);
        assert_eq!(limits.timeout, Duration::from_millis(250));
        assert_eq!(limits.max_body_bytes, 1024 * 1024);
        assert_eq!(limits.max_inflight, 64);
    }

    #[test]
    fn new_engines_expose_their_limits_and_empty_metrics() {
        let limits = WasmLimits {
            timeout: Duration::from_millis(5),
            ..WasmLimits::default()
        };
        let engine = WasmEngine::new(limits).unwrap();
        assert_eq!(engine.limits().timeout, Duration::from_millis(5));
        assert!(engine.metrics().snapshots().is_empty());
        assert!(engine.metrics().snapshot("missing").is_none());
    }

    #[test]
    fn caches_compiled_components_by_hash() {
        let engine = WasmEngine::new(WasmLimits::default()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plugin.wasm");
        std::fs::write(&path, [0x00, 0x61, 0x73, 0x6d, 0x0d, 0x00, 0x01, 0x00]).unwrap();

        let first = engine.component("hash", &path).unwrap();
        let second = engine.component("hash", &path).unwrap();
        assert!(Arc::ptr_eq(&first, &second));

        engine.remove_component("hash");
        let third = engine.component("hash", &path).unwrap();
        assert!(!Arc::ptr_eq(&first, &third));

        assert!(
            engine
                .component("missing", &dir.path().join("absent.wasm"))
                .is_err()
        );
    }
}
