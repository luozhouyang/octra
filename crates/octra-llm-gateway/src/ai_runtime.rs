//! Maps AI-native control-plane entities onto the compiled `LlmRuntimeData`
//! runtime model.
//!
//! The data plane is built around `LlmRuntimeData`. In `on` mode the control
//! plane stores [`octra_ai_config::AiControlPlaneData`] instead, so this module
//! compiles the AI-native records and synthesizes the equivalent providers,
//! accounts, models, routing groups, routes, targets, and allowlist policies.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use octra_ai_config::{
    AiControlPlaneData, AiModelKind, AiPolicyScope, AiPolicyType, CompiledControlPlane, compile,
};
use octra_api_core::Policy;
use octra_core::{Error, Result};
use octra_storage::{
    LlmModelRecord, LlmProviderAccountRecord, LlmProviderRecord, LlmRouteRecord,
    LlmRoutingGroupRecord, LlmRoutingTargetRecord, LlmRuntimeData, LlmVirtualKeyRecord,
};
use serde_json::Value;

/// One provider dispatch target produced by flattening nested model groups.
struct FlattenedTarget {
    provider_id: String,
    upstream_model: String,
    account_id: Option<String>,
    weight: i64,
    priority: i64,
    position: i64,
    context_window: Option<i64>,
    max_output_tokens: Option<i32>,
    capabilities: Value,
    pricing: Value,
    semantic_description: Option<String>,
}

/// Model metadata retained while collapsing provider targets by endpoint.
#[derive(Default)]
struct TargetMetadata {
    context_window: Option<i64>,
    max_output_tokens: Option<i64>,
    capabilities: Option<Value>,
    pricing: Option<Value>,
}

impl TargetMetadata {
    fn absorb(&mut self, target: &FlattenedTarget) {
        if self.context_window.is_none() {
            self.context_window = target.context_window;
        }
        if self.max_output_tokens.is_none() {
            self.max_output_tokens = target.max_output_tokens.map(i64::from);
        }
        if self.capabilities.is_none() && is_non_empty_object(&target.capabilities) {
            self.capabilities = Some(target.capabilities.clone());
        }
        if self.pricing.is_none() && is_non_empty_object(&target.pricing) {
            self.pricing = Some(target.pricing.clone());
        }
    }
}

/// Compiles AI-native entities and maps them onto `LlmRuntimeData`.
///
/// Virtual keys are not part of the AI-native control plane, so they are passed
/// through unchanged.
pub fn runtime_data_from_ai(
    data: &AiControlPlaneData,
    virtual_keys: Vec<LlmVirtualKeyRecord>,
) -> Result<LlmRuntimeData> {
    let compiled = compile(data).map_err(|errors| {
        let detail = errors
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ");
        Error::Config(format!("AI control plane compile failed: {detail}"))
    })?;

    let now = Utc::now();
    let timestamp = now.to_rfc3339();
    let mut runtime = LlmRuntimeData {
        providers: Vec::new(),
        policies: Vec::new(),
        models: Vec::new(),
        groups: Vec::new(),
        routes: Vec::new(),
        targets: Vec::new(),
        target_semantic_descriptions: std::collections::BTreeMap::new(),
        accounts: Vec::new(),
        virtual_keys,
    };

    map_providers(data, &timestamp, &mut runtime);
    map_accounts(data, &timestamp, &mut runtime);
    map_models(&compiled, &timestamp, &mut runtime)?;
    map_policies(&compiled, now, &mut runtime);

    Ok(runtime)
}

fn map_providers(data: &AiControlPlaneData, timestamp: &str, runtime: &mut LlmRuntimeData) {
    for provider in data.providers.iter().filter(|provider| provider.enabled) {
        runtime.providers.push(LlmProviderRecord {
            id: provider.id.clone(),
            name: provider.name.clone(),
            kind: provider.protocol.clone(),
            protocol: provider.protocol.clone(),
            auth_type: auth_type_for(&provider.protocol).to_string(),
            base_url: Some(provider.base_url.clone()),
            secret_ref: provider.auth_ref.clone(),
            priority: 0,
            config_json: provider.config.to_string(),
            enabled: true,
            created_at: timestamp.to_string(),
            updated_at: timestamp.to_string(),
        });
    }
}

fn map_accounts(data: &AiControlPlaneData, timestamp: &str, runtime: &mut LlmRuntimeData) {
    for account in data.accounts.iter().filter(|account| account.enabled) {
        runtime.accounts.push(LlmProviderAccountRecord {
            id: account.id.clone(),
            provider_id: account.provider_id.clone(),
            name: account.name.clone(),
            secret_ref: account.secret_ref.clone(),
            weight: account.weight,
            priority: account.priority,
            enabled: true,
            quota_limit: account.quota_limit,
            quota_unit: account.quota_unit.clone(),
            quota_period: account.quota_period.clone(),
            quota_used: account.quota_used,
            quota_reset_at: None,
            cooldown_until: account.cooldown_until.clone(),
            last_error: None,
            credential_kind: account.credential_kind.clone(),
            oauth_status: account.oauth_status.clone(),
            oauth_expires_at: account.oauth_expires_at.clone(),
            config_json: account.config.to_string(),
            created_at: timestamp.to_string(),
            updated_at: timestamp.to_string(),
        });
    }
}

fn map_models(
    compiled: &CompiledControlPlane,
    timestamp: &str,
    runtime: &mut LlmRuntimeData,
) -> Result<()> {
    let mut provider_models: BTreeMap<(String, String), TargetMetadata> = BTreeMap::new();

    for compiled_model in compiled
        .models
        .values()
        .filter(|model| model.model.kind == AiModelKind::Model)
    {
        let model_id = &compiled_model.model.id;
        let group_id = format!("ai-group:{model_id}");
        runtime.groups.push(LlmRoutingGroupRecord {
            id: group_id.clone(),
            name: compiled_model.model.name.clone(),
            strategy: compiled_model.model.balancer.strategy.clone(),
            created_at: timestamp.to_string(),
        });
        runtime.routes.push(LlmRouteRecord {
            id: format!("ai-route:{model_id}"),
            virtual_model_id: compiled_model.model.name.clone(),
            routing_group_id: Some(group_id.clone()),
            created_at: timestamp.to_string(),
            updated_at: timestamp.to_string(),
        });

        let mut flattened = Vec::new();
        let mut path = Vec::new();
        flatten_targets(compiled, model_id, 1, 0, None, &mut path, &mut flattened)?;

        for (index, target) in flattened.into_iter().enumerate() {
            let target_id = format!("ai-target:{model_id}:{index}");
            if let Some(description) = target
                .semantic_description
                .as_deref()
                .map(str::trim)
                .filter(|description| !description.is_empty())
            {
                runtime
                    .target_semantic_descriptions
                    .insert(target_id.clone(), description.to_string());
            }
            provider_models
                .entry((target.provider_id.clone(), target.upstream_model.clone()))
                .or_default()
                .absorb(&target);
            runtime.targets.push(LlmRoutingTargetRecord {
                id: target_id,
                group_id: group_id.clone(),
                provider_id: target.provider_id,
                model_id: target.upstream_model,
                account_id: target.account_id,
                weight: target.weight,
                priority: target.priority,
                position: target.position,
                created_at: timestamp.to_string(),
                updated_at: timestamp.to_string(),
            });
        }
    }

    for ((provider_id, upstream_model), metadata) in provider_models {
        runtime.models.push(LlmModelRecord {
            id: format!("ai-model:{provider_id}:{upstream_model}"),
            provider_id,
            model_id: upstream_model,
            display_name: None,
            protocol: None,
            context_window: metadata.context_window,
            max_output_tokens: metadata.max_output_tokens,
            capabilities: metadata
                .capabilities
                .unwrap_or_else(empty_object)
                .to_string(),
            pricing_json: metadata.pricing.unwrap_or_else(empty_object).to_string(),
            enabled: true,
            created_at: timestamp.to_string(),
        });
    }

    Ok(())
}

fn map_policies(compiled: &CompiledControlPlane, now: DateTime<Utc>, runtime: &mut LlmRuntimeData) {
    for attachment in &compiled.attachments {
        let Some(policy) = compiled.policies.get(&attachment.policy_id) else {
            continue;
        };
        if policy.policy_type != AiPolicyType::Allowlist {
            continue;
        }
        let (scope, consumer_id) = match attachment.scope {
            AiPolicyScope::Global => ("global", None),
            AiPolicyScope::Consumer => ("consumer", attachment.scope_id.clone()),
            _ => continue,
        };
        runtime.policies.push(Policy {
            id: format!("ai-policy:{}", policy.id),
            name: policy.name.clone(),
            scope: scope.to_string(),
            consumer_id,
            document: policy.config.clone(),
            created_at: now,
            updated_at: now,
        });
    }
}

/// Recursively expands a model into provider targets.
///
/// Nested expansions multiply weights and scale priorities so outer targets
/// retain precedence over inner fallbacks. A provider target inherits the
/// enclosing nested target's semantic description unless it declares its own.
fn flatten_targets(
    compiled: &CompiledControlPlane,
    model_id: &str,
    weight: i64,
    priority: i64,
    inherited_description: Option<&str>,
    path: &mut Vec<String>,
    out: &mut Vec<FlattenedTarget>,
) -> Result<()> {
    if path.iter().any(|visited| visited == model_id) {
        let mut chain = path.clone();
        chain.push(model_id.to_string());
        return Err(Error::Config(format!(
            "AI control plane model nesting cycle detected: {}",
            chain.join(" -> ")
        )));
    }
    let model = compiled.models.get(model_id).ok_or_else(|| {
        Error::Config(format!(
            "AI control plane model '{model_id}' is not part of the compiled control plane"
        ))
    })?;

    path.push(model_id.to_string());
    for target in &model.targets {
        let target_weight = weight
            .saturating_mul(target.target.weight.max(1))
            .clamp(1, 1000);
        let target_priority = priority
            .saturating_mul(1000)
            .saturating_add(target.target.priority);
        let description = target
            .target
            .semantic_description
            .as_deref()
            .map(str::trim)
            .filter(|description| !description.is_empty())
            .or(inherited_description);

        if let Some(provider) = &target.provider {
            out.push(FlattenedTarget {
                provider_id: provider.provider_id.clone(),
                upstream_model: provider.upstream_model.clone(),
                account_id: provider.account_id.clone(),
                weight: target_weight,
                priority: target_priority,
                position: target.target.position,
                context_window: target.target.context_window,
                max_output_tokens: target.target.max_output_tokens,
                capabilities: target.target.capabilities.clone(),
                pricing: target.target.pricing.clone(),
                semantic_description: description.map(str::to_string),
            });
        } else if let Some(nested_model_id) = &target.nested_model_id {
            flatten_targets(
                compiled,
                nested_model_id,
                target_weight,
                target_priority,
                description,
                path,
                out,
            )?;
        }
    }
    path.pop();

    Ok(())
}

fn auth_type_for(protocol: &str) -> &'static str {
    match protocol.trim().to_ascii_lowercase().as_str() {
        "anthropic" | "claude" => "anthropic",
        "gemini" | "google" => "header",
        "ollama" | "vllm" | "lmstudio" | "lm-studio" | "groq" | "openrouter" | "deepseek"
        | "together" | "fireworks" | "openai-compatible" | "openai-chat" | "bedrock"
        | "aws-bedrock" | "bedrock-runtime" | "bedrock-converse" | "bedrock_converse" => "bearer",
        _ => "bearer",
    }
}

fn is_non_empty_object(value: &Value) -> bool {
    value.as_object().is_some_and(|object| !object.is_empty())
}

fn empty_object() -> Value {
    Value::Object(serde_json::Map::new())
}
