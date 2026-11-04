//! Built-in LLM middleware plugins.
//!
//! Each plugin performs one bounded rewrite of the canonical request or
//! stream and validates its configuration at chain-compile time.

mod budget_guard;
mod intent_hint;
mod model_map;
mod param_override;
mod rate_limit;
mod redaction;
mod request_audit;
mod system_prompt;
mod think_tags;

pub use budget_guard::BudgetGuardFactory;
pub use intent_hint::IntentHintFactory;
pub use model_map::ModelMapFactory;
pub use param_override::ParamOverrideFactory;
pub use rate_limit::RateLimitFactory;
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
    registry_with(None)
}

/// Like [`registry`], but lets rule-based plugins resolve host-managed
/// detection engines (for example a model sidecar) by configured name.
pub fn registry_with(
    engines: Option<std::sync::Arc<dyn octra_pii_types::PiiEngineResolver>>,
) -> LlmPluginRegistry {
    let registry = LlmPluginRegistry::new();
    registry.register(BudgetGuardFactory);
    registry.register(IntentHintFactory);
    registry.register(RequestAuditFactory);
    registry.register(ModelMapFactory);
    registry.register(ParamOverrideFactory);
    registry.register(RateLimitFactory);
    registry.register(RedactionFactory::new(engines));
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
        phases: Vec::new(),
        capabilities: vec![Capability::RequestBody],
        config_schema: None,
        api_version: "1".to_string(),
        entities: Vec::new(),
        migrations: Vec::new(),
        admin_routes: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn all_builtin_plugins_register_at_the_current_api_version() {
        let registry = super::registry();
        let manifests = registry.manifests();
        let names: Vec<&str> = manifests
            .iter()
            .map(|manifest| manifest.name.as_str())
            .collect();
        for expected in [
            "llm-budget-guard",
            "llm-intent-hint",
            "llm-model-map",
            "llm-param-override",
            "llm-pii-redaction",
            "llm-rate-limit",
            "llm-request-audit",
            "llm-system-prompt",
            "llm-think-tags",
        ] {
            assert!(names.contains(&expected), "missing plugin {expected}");
        }
    }

    #[test]
    fn builtin_plugins_declare_a_config_schema() {
        let registry = super::registry();
        let manifests = registry.manifests();
        assert!(!manifests.is_empty());
        for manifest in manifests {
            let schema = manifest
                .config_schema
                .as_ref()
                .unwrap_or_else(|| panic!("{} has no config schema", manifest.name));
            let schema = schema
                .as_object()
                .unwrap_or_else(|| panic!("{} config schema is not a JSON object", manifest.name));
            assert_eq!(
                schema.get("type").and_then(|value| value.as_str()),
                Some("object"),
                "{} config schema must declare type object",
                manifest.name
            );
        }
    }

    #[test]
    fn builtin_plugins_declare_their_phases() {
        use octra_api_plugin_types::PluginKind;
        use octra_llm_plugin_types::LlmScope;

        let registry = super::registry();
        let expected = [
            ("llm-rate-limit", 1000),
            ("llm-model-map", 900),
            ("llm-param-override", 850),
            ("llm-system-prompt", 800),
            ("llm-pii-redaction", 700),
            ("llm-budget-guard", 650),
            ("llm-intent-hint", 600),
            ("llm-request-audit", 0),
            ("llm-think-tags", 0),
        ];
        for (name, priority) in expected {
            let factory = registry.factory(name).expect("registered factory");
            let manifest = factory.manifest();
            assert_eq!(manifest.kind, PluginKind::Llm);
            assert_eq!(manifest.priority, priority, "priority for {name}");
        }

        // Request-stage transforms cannot honour provider scope: the request
        // chain is built before a provider is selected.
        for name in [
            "llm-budget-guard",
            "llm-intent-hint",
            "llm-model-map",
            "llm-param-override",
            "llm-pii-redaction",
            "llm-rate-limit",
            "llm-system-prompt",
        ] {
            let factory = registry.factory(name).expect("registered factory");
            assert!(
                !factory.supported_scopes().contains(&LlmScope::Provider),
                "{name} must not advertise provider scope"
            );
        }

        // Stream/log plugins do not need request-stage state and can be
        // configured per provider.
        for name in ["llm-request-audit", "llm-think-tags"] {
            let factory = registry.factory(name).expect("registered factory");
            assert!(
                factory.supported_scopes().contains(&LlmScope::Provider),
                "{name} should support provider scope"
            );
        }
    }
}
