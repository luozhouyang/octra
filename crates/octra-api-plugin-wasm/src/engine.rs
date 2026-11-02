use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Context;
use parking_lot::RwLock;
use wasmtime::component::Component;
use wasmtime::{Config, Engine};

#[derive(Debug, Clone)]
pub struct WasmLimits {
    pub max_fuel: u64,
    pub max_memory_bytes: usize,
    pub timeout: Duration,
}

impl Default for WasmLimits {
    fn default() -> Self {
        Self {
            max_fuel: 10_000_000,
            max_memory_bytes: 64 * 1024 * 1024,
            timeout: Duration::from_millis(250),
        }
    }
}

struct EngineInner {
    engine: Engine,
    stop: AtomicBool,
}

pub struct WasmEngine {
    inner: Arc<EngineInner>,
    components: RwLock<HashMap<String, Arc<Component>>>,
    limits: WasmLimits,
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
        })
    }

    pub fn limits(&self) -> &WasmLimits {
        &self.limits
    }

    pub fn engine(&self) -> &Engine {
        &self.inner.engine
    }

    pub fn component(&self, hash: &str, path: &Path) -> anyhow::Result<Arc<Component>> {
        if let Some(component) = self.components.read().get(hash) {
            return Ok(component.clone());
        }
        let component = Arc::new(Component::from_file(self.engine(), path).map_err(|error| {
            anyhow::anyhow!(
                "cannot compile wasm component {}: {error}",
                path.display()
            )
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
