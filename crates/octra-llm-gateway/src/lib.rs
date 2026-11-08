//! Gateways pass errors and responses by value through deeply nested async
//! calls; boxing them would only obscure the flow.
#![allow(clippy::result_large_err)]

//! LLM Gateway: canonical IR translation, provider routing, the OpenAI and
//! Anthropic compatible data plane, and the authenticated control plane.
//!
//! ```text
//! client ──▶ data_plane_router ──▶ virtual key check ──▶ LlmRuntime snapshot
//!        ◀── streamed SSE / JSON ◀── protocol adapter ◀── upstream adapter ◀── provider
//! ```

pub mod accounts;
pub mod adapter;
pub mod ai_control;
pub mod ai_policy_plugins;
pub mod ai_runtime;
pub mod cli;
pub mod config;
pub mod control;
pub mod cost;
pub mod credentials;
pub mod data_plane;
pub mod error;
pub mod keys;
pub mod pii;
pub mod routing;
pub mod runtime;
pub mod similarity;
pub mod upstream;
pub mod usage;
pub mod vk_policy;

pub use runtime::{LlmControlState, LlmDataState, LlmRuntime, RuntimeSnapshot};
pub use upstream::Upstream as LlmUpstream;

pub use octra_llm_plugin_runtime::LlmPluginRuntime;

use axum::Router;
use octra_config::OctraConfig;
use octra_core::Result;
use octra_storage::Storage;
use std::sync::Arc;

/// The LLM data plane: OpenAI-compatible and Anthropic-compatible endpoints.
///
/// Routes are registered on the shared data plane before the API gateway's
/// catch-all, so they take precedence.
pub fn data_plane_router(state: LlmDataState) -> Router {
    data_plane::router(state)
}

/// The LLM control plane, merged by the daemon under the global bearer layer.
pub fn control_router(state: LlmControlState) -> Router {
    control::router(state)
}

/// Builds a runtime for tests and embedders, accepting the already-shared
/// config handle.
pub async fn bootstrap_runtime(
    storage: Arc<Storage>,
    config: Arc<OctraConfig>,
) -> Result<LlmRuntime> {
    LlmRuntime::bootstrap(storage, config).await
}
