//! Maps AI-native policy records onto synthetic LLM plugin records.
//!
//! AI policies are stored as flat entities and compiled by `octra-ai-config`
//! for the LLM runtime. Executable policies are also translated into the
//! generic plugin catalog shape so the existing
//! [`octra_llm_plugin_runtime::LlmPluginRuntime`] can compile and run them
//! without a second execution path.
//!
//! The mapping is pure: it never touches storage and produces deterministic
//! ids derived from the AI entity ids.

use chrono::{SecondsFormat, Utc};
use octra_ai_config::{AiControlPlaneData, AiPolicyAttachment, AiPolicyScope, AiPolicyType};
use octra_llm_plugin_runtime::LlmPluginRegistry;
use octra_storage::plugins::{PluginConfigRecord, PluginData, PluginRecord};

/// Built-in plugin that implements an AI policy type, if one exists.
///
/// `Allowlist` is handled by the AI runtime's policy mapping and has no
/// built-in plugin. Every other AI policy type maps to an LLM plugin.
fn plugin_name(policy_type: &AiPolicyType) -> Option<&'static str> {
    match policy_type {
        AiPolicyType::RateLimit => Some("llm-rate-limit"),
        AiPolicyType::BudgetGuard => Some("llm-budget-guard"),
        AiPolicyType::PiiRedaction => Some("llm-pii-redaction"),
        AiPolicyType::PromptGuard => Some("llm-prompt-guard"),
        AiPolicyType::PromptCompress => Some("llm-prompt-compress"),
        AiPolicyType::ModelMap => Some("llm-model-map"),
        AiPolicyType::ParamOverride => Some("llm-param-override"),
        AiPolicyType::SystemPrompt => Some("llm-system-prompt"),
        AiPolicyType::RequestAudit => Some("llm-request-audit"),
        AiPolicyType::ThinkTags => Some("llm-think-tags"),
        AiPolicyType::SemanticCache => Some("llm-semantic-cache"),
        AiPolicyType::Allowlist => None,
    }
}

fn synthetic_plugin_id(policy_id: &str) -> String {
    format!("ai-policy-plugin:{policy_id}")
}

fn synthetic_config_id(attachment_id: &str) -> String {
    format!("ai-policy-config:{attachment_id}")
}

fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Builds plugin records for every enabled AI policy that has at least one
/// enabled attachment.
pub fn plugin_data_from_ai(data: &AiControlPlaneData, registry: &LlmPluginRegistry) -> PluginData {
    let timestamp = now();
    let mut result = PluginData::default();

    for policy in data.policies.iter().filter(|policy| policy.enabled) {
        let attachments: Vec<&AiPolicyAttachment> = data
            .attachments
            .iter()
            .filter(|attachment| attachment.enabled && attachment.policy_id == policy.id)
            .collect();
        if attachments.is_empty() {
            continue;
        }

        let Some(name) = plugin_name(&policy.policy_type) else {
            match &policy.policy_type {
                AiPolicyType::Allowlist => {}
                unsupported => tracing::debug!(
                    policy_id = %policy.id,
                    policy_type = ?unsupported,
                    "AI policy has no built-in LLM plugin; skipping"
                ),
            }
            continue;
        };
        let Some(factory) = registry.factory(name) else {
            tracing::warn!(
                policy_id = %policy.id,
                plugin = name,
                "AI policy plugin is not registered; skipping policy"
            );
            continue;
        };

        let manifest = factory.manifest();
        let plugin_id = synthetic_plugin_id(&policy.id);
        let manifest_json = serde_json::to_string(&manifest).unwrap_or_else(|_| "{}".to_string());
        let config_schema_json = manifest
            .config_schema
            .as_ref()
            .and_then(|schema| serde_json::to_string(schema).ok())
            .unwrap_or_else(|| "{}".to_string());
        result.plugins.push(PluginRecord {
            id: plugin_id.clone(),
            name: manifest.name.clone(),
            version: manifest.version.clone(),
            kind: "llm".to_string(),
            runtime_type: "builtin".to_string(),
            enabled: true,
            priority: i64::from(manifest.priority),
            description: manifest.description.clone(),
            manifest_json,
            config_schema_json,
            artifact_path: None,
            artifact_hash: None,
            source: "ai-policy".to_string(),
            artifact_id: None,
            storage_path: None,
            approved_capabilities: manifest.capabilities.clone(),
            updated_at: timestamp.clone(),
        });

        for attachment in attachments {
            let Some((scope, scope_id)) = attachment_scope(attachment, data) else {
                tracing::warn!(
                    policy_id = %policy.id,
                    attachment_id = %attachment.id,
                    scope = ?attachment.scope,
                    scope_id = ?attachment.scope_id,
                    "AI policy attachment scope cannot be resolved; skipping attachment"
                );
                continue;
            };
            let config_json =
                serde_json::to_string(&policy.config).unwrap_or_else(|_| "{}".to_string());
            result.configs.push(PluginConfigRecord {
                id: synthetic_config_id(&attachment.id),
                plugin_id: plugin_id.clone(),
                scope: scope.to_string(),
                scope_id,
                enabled: true,
                config_json,
                created_at: timestamp.clone(),
                updated_at: timestamp.clone(),
            });
        }
    }

    result
}

/// Resolves an attachment to the plugin scope pair.
///
/// Plugin chains are resolved with AI model *names* and provider ids, so model
/// and target attachments are translated to the owning model name.
fn attachment_scope(
    attachment: &AiPolicyAttachment,
    data: &AiControlPlaneData,
) -> Option<(&'static str, String)> {
    match attachment.scope {
        AiPolicyScope::Global => Some(("global", String::new())),
        AiPolicyScope::Consumer => {
            Some(("consumer", attachment.scope_id.clone().unwrap_or_default()))
        }
        AiPolicyScope::ConsumerGroup => Some((
            "consumer_group",
            attachment.scope_id.clone().unwrap_or_default(),
        )),
        AiPolicyScope::VirtualKey => Some((
            "virtual_key",
            attachment.scope_id.clone().unwrap_or_default(),
        )),
        AiPolicyScope::Provider => {
            Some(("provider", attachment.scope_id.clone().unwrap_or_default()))
        }
        AiPolicyScope::Model => {
            let model = data
                .models
                .iter()
                .find(|model| Some(&model.id) == attachment.scope_id.as_ref())?;
            Some(("model", model.name.clone()))
        }
        AiPolicyScope::Target => {
            let target = data
                .targets
                .iter()
                .find(|target| Some(&target.id) == attachment.scope_id.as_ref())?;
            let model = data
                .models
                .iter()
                .find(|model| model.id == target.model_id)?;
            Some(("model", model.name.clone()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use octra_ai_config::{AiModelKind, AiPolicy};
    use octra_llm_plugin_builtin::registry;
    use serde_json::json;

    fn model(id: &str, name: &str) -> octra_ai_config::AiModel {
        octra_ai_config::AiModel {
            id: id.to_string(),
            name: name.to_string(),
            kind: AiModelKind::Model,
            capabilities: vec!["chat".to_string()],
            formats: vec!["openai".to_string()],
            route_paths: Vec::new(),
            balancer: Default::default(),
            enabled: true,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn policy(id: &str, policy_type: AiPolicyType) -> AiPolicy {
        AiPolicy {
            id: id.to_string(),
            name: id.to_string(),
            policy_type,
            config: json!({}),
            enabled: true,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn attachment(
        id: &str,
        policy_id: &str,
        scope: AiPolicyScope,
        scope_id: Option<&str>,
    ) -> AiPolicyAttachment {
        AiPolicyAttachment {
            id: id.to_string(),
            policy_id: policy_id.to_string(),
            scope,
            scope_id: scope_id.map(str::to_string),
            order: 0,
            enabled: true,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn model_scope_uses_model_name() {
        let data = AiControlPlaneData {
            models: vec![model("model-1", "chat")],
            policies: vec![policy("policy-1", AiPolicyType::SystemPrompt)],
            attachments: vec![attachment(
                "attachment-1",
                "policy-1",
                AiPolicyScope::Model,
                Some("model-1"),
            )],
            ..AiControlPlaneData::default()
        };
        let plugin_data = plugin_data_from_ai(&data, &registry());
        assert_eq!(plugin_data.configs.len(), 1);
        assert_eq!(plugin_data.configs[0].scope, "model");
        assert_eq!(plugin_data.configs[0].scope_id, "chat");
    }

    #[test]
    fn prompt_compress_maps_to_the_builtin_plugin() {
        let data = AiControlPlaneData {
            policies: vec![policy("policy-compress", AiPolicyType::PromptCompress)],
            attachments: vec![attachment(
                "attachment-compress",
                "policy-compress",
                AiPolicyScope::Global,
                None,
            )],
            ..AiControlPlaneData::default()
        };
        let plugin_data = plugin_data_from_ai(&data, &registry());
        assert_eq!(plugin_data.plugins.len(), 1);
        assert_eq!(plugin_data.plugins[0].name, "llm-prompt-compress");
        assert_eq!(plugin_data.configs.len(), 1);
        assert_eq!(plugin_data.configs[0].scope, "global");
    }

    #[test]
    fn unresolvable_target_is_skipped() {
        let data = AiControlPlaneData {
            models: vec![model("model-1", "chat")],
            policies: vec![policy("policy-1", AiPolicyType::SystemPrompt)],
            attachments: vec![attachment(
                "attachment-1",
                "policy-1",
                AiPolicyScope::Target,
                Some("missing-target"),
            )],
            ..AiControlPlaneData::default()
        };
        let plugin_data = plugin_data_from_ai(&data, &registry());
        assert_eq!(plugin_data.plugins.len(), 1);
        assert!(plugin_data.configs.is_empty());
    }
}
