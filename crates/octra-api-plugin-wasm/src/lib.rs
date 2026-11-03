//! WASM component runtime for third-party API Gateway plugins.
//!
//! Components target the WIT world in `wit/plugin.wit`. The host applies
//! resource limits, exposes plugin-scoped storage, and adapts the component's
//! request/response decisions to the existing `ApiGatewayPlugin` chain.

mod adapter;
mod catalog;
mod engine;
mod factory;
mod gate;
mod host;
mod host_provider;
mod metrics;

pub use adapter::{AdminDispatch, WasmApiGatewayPlugin};
pub use catalog::WasmPluginCatalog;
pub use engine::{WasmEngine, WasmLimits};
pub use factory::WasmPluginFactory;
pub use host_provider::{InstalledWasmPlugin, WasmHostProvider};
pub use metrics::{PluginMetrics, PluginStatsSnapshot};

#[doc(hidden)]
pub mod testing;

wasmtime::component::bindgen!({
    path: "wit",
    world: "plugin",
});
