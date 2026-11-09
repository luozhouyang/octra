//! Coverage for the AI policy -> LLM plugin adapter.
//!
//! The adapter must translate executable AI policies into synthetic plugin
//! records, skip policies without a built-in plugin, and produce chains the
//! runtime can execute.

use std::sync::Arc;

use octra_ai_config::{
    AiBalancer, AiControlPlaneData, AiModel, AiModelKind, AiModelTarget, AiPolicy,
    AiPolicyAttachment, AiPolicyScope, AiPolicyType, AiProvider,
};
use octra_llm_core::{ChatRequest, Message, Protocol};
use octra_llm_gateway::LlmPluginRuntime;
use octra_llm_gateway::ai_policy_plugins::plugin_data_from_ai;
use octra_llm_plugin_types::{LlmRequestMeta, LlmRequestState, LlmResponseHeaders};
use octra_storage::Storage;
use serde_json::json;

fn provider() -> AiProvider {
    AiProvider {
        id: "provider-1".to_string(),
        name: "openai".to_string(),
        protocol: "openai".to_string(),
        base_url: "https://api.example.com".to_string(),
        auth_ref: None,
        capabilities: vec!["chat".to_string()],
        config: json!({}),
        enabled: true,
        created_at: 0,
        updated_at: 0,
    }
}

fn model() -> AiModel {
    AiModel {
        id: "model-1".to_string(),
        name: "chat".to_string(),
        kind: AiModelKind::Model,
        capabilities: vec!["chat".to_string()],
        formats: vec!["openai".to_string()],
        route_paths: vec!["/v1/chat/completions".to_string()],
        balancer: AiBalancer::default(),
        enabled: true,
        created_at: 0,
        updated_at: 0,
    }
}

fn target() -> AiModelTarget {
    AiModelTarget {
        id: "target-1".to_string(),
        model_id: "model-1".to_string(),
        target_model_id: None,
        provider_id: Some("provider-1".to_string()),
        upstream_model: Some("gpt-4o".to_string()),
        account_id: None,
        weight: 1,
        priority: 0,
        position: 0,
        config: json!({}),
        context_window: None,
        max_output_tokens: None,
        capabilities: json!({}),
        pricing: json!({}),
        semantic_description: None,
        enabled: true,
        created_at: 0,
        updated_at: 0,
    }
}

fn policy(id: &str, policy_type: AiPolicyType, config: serde_json::Value) -> AiPolicy {
    AiPolicy {
        id: id.to_string(),
        name: id.to_string(),
        policy_type,
        config,
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

/// Provider, model and target plus one policy per adapter outcome:
/// mapped global, consumer and model policies, and the single policy with no
/// built-in plugin (`Allowlist`).
fn control_plane() -> AiControlPlaneData {
    AiControlPlaneData {
        providers: vec![provider()],
        accounts: Vec::new(),
        models: vec![model()],
        targets: vec![target()],
        policies: vec![
            policy(
                "policy-rate",
                AiPolicyType::RateLimit,
                json!({"minute": 60, "limit_by": "global"}),
            ),
            policy(
                "policy-pii",
                AiPolicyType::PiiRedaction,
                json!({"rules": {"email": true}}),
            ),
            policy(
                "policy-system",
                AiPolicyType::SystemPrompt,
                json!({"prompt": "Be concise."}),
            ),
            policy(
                "policy-guard",
                AiPolicyType::PromptGuard,
                json!({"keywords": ["forbidden"]}),
            ),
            policy(
                "policy-allow",
                AiPolicyType::Allowlist,
                json!({"allowed_models": ["chat"]}),
            ),
            policy("policy-compress", AiPolicyType::PromptCompress, json!({})),
            policy("policy-cache", AiPolicyType::SemanticCache, json!({})),
        ],
        attachments: vec![
            attachment(
                "attachment-rate",
                "policy-rate",
                AiPolicyScope::Global,
                None,
            ),
            attachment(
                "attachment-pii",
                "policy-pii",
                AiPolicyScope::Consumer,
                Some("consumer-1"),
            ),
            attachment(
                "attachment-system",
                "policy-system",
                AiPolicyScope::Model,
                Some("model-1"),
            ),
            attachment(
                "attachment-guard",
                "policy-guard",
                AiPolicyScope::Global,
                None,
            ),
            attachment(
                "attachment-allow",
                "policy-allow",
                AiPolicyScope::Global,
                None,
            ),
            attachment(
                "attachment-compress",
                "policy-compress",
                AiPolicyScope::Global,
                None,
            ),
            attachment(
                "attachment-cache",
                "policy-cache",
                AiPolicyScope::Model,
                Some("model-1"),
            ),
        ],
    }
}

#[test]
fn maps_supported_policies_and_skips_the_rest() {
    let data = control_plane();
    let registry = octra_llm_plugin_builtin::registry();
    let plugin_data = plugin_data_from_ai(&data, &registry);

    assert_eq!(plugin_data.plugins.len(), 6);
    assert_eq!(plugin_data.configs.len(), 6);

    let names: Vec<&str> = plugin_data
        .plugins
        .iter()
        .map(|plugin| plugin.name.as_str())
        .collect();
    assert_eq!(
        names,
        vec![
            "llm-rate-limit",
            "llm-pii-redaction",
            "llm-system-prompt",
            "llm-prompt-guard",
            "llm-prompt-compress",
            "llm-semantic-cache"
        ]
    );
    for plugin in &plugin_data.plugins {
        assert_eq!(plugin.kind, "llm");
        assert_eq!(plugin.runtime_type, "builtin");
        assert_eq!(plugin.source, "ai-policy");
        assert!(plugin.enabled);
        assert!(plugin.id.starts_with("ai-policy-plugin:"));
        assert!(plugin.artifact_path.is_none());
        assert!(plugin.artifact_hash.is_none());
        assert!(plugin.artifact_id.is_none());
        assert!(plugin.storage_path.is_none());
    }

    let rate = plugin_data
        .configs
        .iter()
        .find(|config| config.id == "ai-policy-config:attachment-rate")
        .expect("rate limit config");
    assert_eq!(rate.plugin_id, "ai-policy-plugin:policy-rate");
    assert_eq!(rate.scope, "global");
    assert_eq!(rate.scope_id, "");

    let pii = plugin_data
        .configs
        .iter()
        .find(|config| config.id == "ai-policy-config:attachment-pii")
        .expect("PII config");
    assert_eq!(pii.plugin_id, "ai-policy-plugin:policy-pii");
    assert_eq!(pii.scope, "consumer");
    assert_eq!(pii.scope_id, "consumer-1");

    // Model attachments are keyed by the AI model name, not its id, because
    // plugin chains resolve with the requested model name.
    let system = plugin_data
        .configs
        .iter()
        .find(|config| config.id == "ai-policy-config:attachment-system")
        .expect("system prompt config");
    assert_eq!(system.plugin_id, "ai-policy-plugin:policy-system");
    assert_eq!(system.scope, "model");
    assert_eq!(system.scope_id, "chat");

    let guard = plugin_data
        .configs
        .iter()
        .find(|config| config.id == "ai-policy-config:attachment-guard")
        .expect("prompt guard config");
    assert_eq!(guard.plugin_id, "ai-policy-plugin:policy-guard");
    assert_eq!(guard.scope, "global");
    assert_eq!(guard.scope_id, "");

    let compress = plugin_data
        .configs
        .iter()
        .find(|config| config.id == "ai-policy-config:attachment-compress")
        .expect("prompt compress config");
    assert_eq!(compress.plugin_id, "ai-policy-plugin:policy-compress");
    assert_eq!(compress.scope, "global");
    assert_eq!(compress.scope_id, "");

    let cache = plugin_data
        .configs
        .iter()
        .find(|config| config.id == "ai-policy-config:attachment-cache")
        .expect("semantic cache config");
    assert_eq!(cache.plugin_id, "ai-policy-plugin:policy-cache");
    assert_eq!(cache.scope, "model");
    assert_eq!(cache.scope_id, "chat");

    // `Allowlist` is the only AI policy type without a built-in plugin.
    assert!(
        !plugin_data
            .configs
            .iter()
            .any(|config| config.id.contains("attachment-allow"))
    );
}

#[tokio::test]
async fn reload_with_extra_compiles_executable_chains() {
    let storage = Arc::new(Storage::connect_in_memory().await.unwrap());
    let registry = Arc::new(octra_llm_plugin_builtin::registry());
    let runtime = LlmPluginRuntime::bootstrap(storage, registry)
        .await
        .unwrap();

    let data = control_plane();
    let extra = plugin_data_from_ai(&data, runtime.registry());
    runtime.reload_with_extra(extra).await.unwrap();

    let chain = runtime.effective_chain(Some("consumer-1"), None, &[], "chat", None);
    let names: Vec<&str> = chain.iter().map(|entry| entry.name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "llm-rate-limit",
            "llm-system-prompt",
            "llm-prompt-guard",
            "llm-pii-redaction",
            "llm-prompt-compress",
            "llm-semantic-cache"
        ]
    );
    assert_eq!(chain[0].scope, octra_llm_plugin_types::LlmScope::Global);
    assert_eq!(chain[1].scope, octra_llm_plugin_types::LlmScope::Model);
    assert_eq!(chain[1].scope_id, "chat");
    assert_eq!(chain[2].scope, octra_llm_plugin_types::LlmScope::Global);
    assert_eq!(chain[3].scope, octra_llm_plugin_types::LlmScope::Consumer);
    assert_eq!(chain[3].scope_id, "consumer-1");
    assert_eq!(chain[4].scope, octra_llm_plugin_types::LlmScope::Global);
    assert_eq!(chain[5].scope, octra_llm_plugin_types::LlmScope::Model);
    assert_eq!(chain[5].scope_id, "chat");

    // A caller outside the consumer scope only sees global and model rules.
    let unbound = runtime.effective_chain(None, None, &[], "chat", None);
    let unbound_names: Vec<&str> = unbound.iter().map(|entry| entry.name.as_str()).collect();
    assert_eq!(
        unbound_names,
        vec![
            "llm-rate-limit",
            "llm-system-prompt",
            "llm-prompt-guard",
            "llm-prompt-compress",
            "llm-semantic-cache"
        ]
    );
}

#[tokio::test]
async fn runtime_chain_enforces_the_rate_limit_policy() {
    let storage = Arc::new(Storage::connect_in_memory().await.unwrap());
    let registry = Arc::new(octra_llm_plugin_builtin::registry());
    let runtime = LlmPluginRuntime::bootstrap(storage, registry)
        .await
        .unwrap();

    let mut data = control_plane();
    data.policies[0].config = json!({"minute": 1, "limit_by": "global"});
    let extra = plugin_data_from_ai(&data, runtime.registry());
    runtime.reload_with_extra(extra).await.unwrap();

    let chain = runtime.request_chain(None, None, &[], "chat");
    let meta = LlmRequestMeta {
        request_id: "request-1",
        ingress: Protocol::Openai,
        consumer_id: None,
        virtual_key_id: None,
        virtual_key_family: None,
        consumer_group_ids: &[],
        session_id: None,
        client_ip: None,
        user_agent: None,
        intent: None,
    };
    let mut request = ChatRequest::new("chat", Vec::new());
    let mut headers = LlmResponseHeaders::default();
    let mut state = LlmRequestState::new();
    assert!(
        chain
            .on_request(&meta, &mut headers, &mut request, &mut state)
            .await
            .unwrap()
            .is_none()
    );

    let mut request = ChatRequest::new("chat", Vec::new());
    let mut state = LlmRequestState::new();
    let rejection = chain
        .on_request(&meta, &mut headers, &mut request, &mut state)
        .await
        .unwrap()
        .expect("second request exceeds the one-per-minute limit");
    assert_eq!(rejection.status, 429);
}

#[tokio::test]
async fn runtime_chain_enforces_the_prompt_guard_policy() {
    let storage = Arc::new(Storage::connect_in_memory().await.unwrap());
    let registry = Arc::new(octra_llm_plugin_builtin::registry());
    let runtime = LlmPluginRuntime::bootstrap(storage, registry)
        .await
        .unwrap();

    let data = control_plane();
    let extra = plugin_data_from_ai(&data, runtime.registry());
    runtime.reload_with_extra(extra).await.unwrap();

    let chain = runtime.request_chain(None, None, &[], "chat");
    let meta = LlmRequestMeta {
        request_id: "request-1",
        ingress: Protocol::Openai,
        consumer_id: None,
        virtual_key_id: None,
        virtual_key_family: None,
        consumer_group_ids: &[],
        session_id: None,
        client_ip: None,
        user_agent: None,
        intent: None,
    };
    let mut headers = LlmResponseHeaders::default();

    let mut request = ChatRequest::new("chat", vec![Message::user("this is forbidden")]);
    let mut state = LlmRequestState::new();
    let rejection = chain
        .on_request(&meta, &mut headers, &mut request, &mut state)
        .await
        .unwrap()
        .expect("a matching prompt is blocked");
    assert_eq!(rejection.status, 403);
    assert_eq!(rejection.code.as_deref(), Some("prompt_blocked"));

    let mut request = ChatRequest::new("chat", vec![Message::user("hello")]);
    let mut state = LlmRequestState::new();
    assert!(
        chain
            .on_request(&meta, &mut headers, &mut request, &mut state)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn consumer_group_policies_compile_and_apply_to_group_members() {
    let storage = Arc::new(Storage::connect_in_memory().await.unwrap());
    let registry = Arc::new(octra_llm_plugin_builtin::registry());
    let runtime = LlmPluginRuntime::bootstrap(storage, registry)
        .await
        .unwrap();

    let mut data = control_plane();
    data.policies.push(policy(
        "policy-group",
        AiPolicyType::RequestAudit,
        json!({}),
    ));
    data.attachments.push(attachment(
        "attachment-group",
        "policy-group",
        AiPolicyScope::ConsumerGroup,
        Some("group-1"),
    ));

    let extra = plugin_data_from_ai(&data, runtime.registry());
    // Before ConsumerGroup existed in LlmScope this reload failed and silently
    // disabled every other AI policy.
    runtime.reload_with_extra(extra).await.unwrap();

    let member = runtime.effective_chain(None, None, &["group-1".to_string()], "chat", None);
    let member_names: Vec<&str> = member.iter().map(|entry| entry.name.as_str()).collect();
    assert!(
        member_names.contains(&"llm-request-audit"),
        "group policy must be visible: {member_names:?}"
    );

    let outsider = runtime.effective_chain(None, None, &["group-2".to_string()], "chat", None);
    let outsider_names: Vec<&str> = outsider.iter().map(|entry| entry.name.as_str()).collect();
    assert!(
        !outsider_names.contains(&"llm-request-audit"),
        "outsider must not gain the group policy: {outsider_names:?}"
    );
}
