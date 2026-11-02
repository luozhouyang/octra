//! Built-in LLM middleware plugins.
//!
//! Each plugin performs one bounded rewrite of the canonical request or
//! stream and validates its configuration at chain-compile time.

mod budget_guard;
mod intent_hint;
mod model_map;
mod param_override;
mod redaction;
mod request_audit;
mod system_prompt;
mod think_tags;

pub use budget_guard::BudgetGuardFactory;
pub use intent_hint::IntentHintFactory;
pub use model_map::ModelMapFactory;
pub use param_override::ParamOverrideFactory;
pub use redaction::RedactionFactory;
pub use request_audit::RequestAuditFactory;
pub use system_prompt::SystemPromptFactory;
pub use think_tags::ThinkTagsFactory;

use octra_api_plugin_types::{
    Capability, FailPolicy, PluginKind, PluginManifest, PluginRuntimeKind,
};
use octra_llm_plugin_runtime::LlmPluginRegistry;

/// Builds a registry with every built-in LLM plugin. The API Gateway keeps
/// its own registry; the two are separate catalogs filtered by kind.
pub fn registry() -> LlmPluginRegistry {
    let mut registry = LlmPluginRegistry::new();
    registry.register(BudgetGuardFactory);
    registry.register(IntentHintFactory);
    registry.register(RequestAuditFactory);
    registry.register(ModelMapFactory);
    registry.register(ParamOverrideFactory);
    registry.register(RedactionFactory);
    registry.register(SystemPromptFactory);
    registry.register(ThinkTagsFactory);
    registry
}

pub(crate) fn manifest(name: &str, description: &str) -> PluginManifest {
    PluginManifest {
        name: name.to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        kind: PluginKind::Llm,
        runtime: PluginRuntimeKind::Builtin,
        priority: 0,
        description: description.to_string(),
        timeout_ms: 100,
        fail_policy: FailPolicy::Open,
        capabilities: vec![Capability::RequestBody],
        config_schema: None,
    }
}
