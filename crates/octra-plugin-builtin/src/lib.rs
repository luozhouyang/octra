//! Built-in API Gateway plugins.
//!
//! These plugins validate their JSON configuration at creation time and keep
//! the hot path allocation-light. They are trusted, in-process Rust code;
//! third-party extensions will use the WASM runtime instead.

mod circuit_breaker;
mod cors;
mod header_rewrite;
mod rate_limit;
mod request_id;

use octra_plugin_api::{Capability, PluginKind, PluginManifest, PluginRuntimeKind};
use octra_plugin_runtime::PluginRegistry;
use serde_json::Value;

pub use circuit_breaker::CircuitBreakerFactory;
pub use cors::CorsFactory;
pub use header_rewrite::HeaderRewriteFactory;
pub use rate_limit::RateLimitFactory;
pub use request_id::RequestIdFactory;

pub fn register(registry: &mut PluginRegistry) {
    registry.register(RequestIdFactory);
    registry.register(CorsFactory);
    registry.register(RateLimitFactory);
    registry.register(CircuitBreakerFactory);
    registry.register(HeaderRewriteFactory);
}

pub(crate) fn manifest(
    name: &str,
    priority: i32,
    capabilities: Vec<Capability>,
    description: &str,
    config_schema: Value,
) -> PluginManifest {
    PluginManifest {
        name: name.to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        kind: PluginKind::Api,
        runtime: PluginRuntimeKind::Builtin,
        priority,
        description: description.to_string(),
        timeout_ms: 250,
        capabilities,
        config_schema: Some(config_schema),
    }
}
