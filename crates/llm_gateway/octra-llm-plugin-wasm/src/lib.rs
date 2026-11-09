//! WASM component runtime for third-party LLM Gateway plugins.
//!
//! Components target the JSON envelope in `wit/llm-plugin.wit`. The host
//! serializes canonical gateway types, calls one `on-stage` export per stage,
//! and applies the returned mutations. The host exposes structured logging, a
//! clock, and capability-gated per-plugin key-value storage; network and secret
//! handles remain native-only.

mod adapter;
mod factory;
mod host;

pub use adapter::{LlmWasmPlugin, LlmWasmPluginFactory};

wasmtime::component::bindgen!({
    path: "wit",
    world: "llm-plugin",
});
