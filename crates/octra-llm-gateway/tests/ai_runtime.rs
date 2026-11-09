//! Tests for mapping AI-native control-plane data onto the LLM runtime.

use std::sync::Arc;

use octra_ai_config::{
    AiBalancer, AiControlPlaneData, AiModel, AiModelKind, AiModelTarget, AiPolicy,
    AiPolicyAttachment, AiPolicyScope, AiPolicyType, AiProvider, AiProviderAccount,
};
use octra_config::OctraConfig;
use octra_core::Error;
use octra_llm_gateway::ai_runtime::runtime_data_from_ai;
use octra_llm_gateway::{LlmRuntime, RuntimeSnapshot};
use octra_storage::Storage;
use serde_json::json;

fn provider(id: &str, name: &str, protocol: &str) -> AiProvider {
    AiProvider {
        id: id.into(),
        name: name.into(),
        protocol: protocol.into(),
        base_url: "http://127.0.0.1:9/v1".into(),
        auth_ref: Some("env:TEST_KEY".into()),
        capabilities: vec!["chat".into()],
        config: json!({}),
        enabled: true,
        created_at: 0,
        updated_at: 0,
    }
}

fn account(id: &str, provider_id: &str, name: &str) -> AiProviderAccount {
    AiProviderAccount {
        id: id.into(),
        provider_id: provider_id.into(),
        name: name.into(),
        credential_kind: "api_key".into(),
        secret_ref: Some("env:TEST_KEY".into()),
        oauth_ref: None,
        weight: 1,
        priority: 0,
        quota_limit: None,
        quota_unit: "requests".into(),
        quota_period: "month".into(),
        quota_used: 0,
        cooldown_until: None,
        enabled: true,
        oauth_status: "none".into(),
        oauth_expires_at: None,
        config: json!({}),
        created_at: 0,
        updated_at: 0,
    }
}

fn model(id: &str, name: &str, kind: AiModelKind) -> AiModel {
    AiModel {
        id: id.into(),
        name: name.into(),
        kind,
        capabilities: vec!["chat".into()],
        formats: vec!["openai".into()],
        route_paths: Vec::new(),
        balancer: AiBalancer {
            strategy: "priority".into(),
            ..AiBalancer::default()
        },
        enabled: true,
        created_at: 0,
        updated_at: 0,
    }
}

fn provider_target(
    id: &str,
    model_id: &str,
    provider_id: &str,
    upstream_model: &str,
) -> AiModelTarget {
    AiModelTarget {
        id: id.into(),
        model_id: model_id.into(),
        target_model_id: None,
        provider_id: Some(provider_id.into()),
        upstream_model: Some(upstream_model.into()),
        account_id: Some("account-1".into()),
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

fn nested_target(id: &str, model_id: &str, nested_model_id: &str) -> AiModelTarget {
    AiModelTarget {
        id: id.into(),
        model_id: model_id.into(),
        target_model_id: Some(nested_model_id.into()),
        provider_id: None,
        upstream_model: None,
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

fn describe(mut target: AiModelTarget, description: &str) -> AiModelTarget {
    target.semantic_description = Some(description.to_string());
    target
}

fn config() -> Arc<OctraConfig> {
    Arc::new(OctraConfig::default())
}

fn allowlist_policy(id: &str, name: &str, models: &[&str]) -> AiPolicy {
    AiPolicy {
        id: id.into(),
        name: name.into(),
        policy_type: AiPolicyType::Allowlist,
        config: json!({ "allowed_models": models }),
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
        id: id.into(),
        policy_id: policy_id.into(),
        scope,
        scope_id: scope_id.map(str::to_string),
        order: 0,
        enabled: true,
        created_at: 0,
        updated_at: 0,
    }
}

async fn storage() -> Arc<Storage> {
    Arc::new(Storage::connect_in_memory().await.unwrap())
}

async fn seed_provider_and_account(storage: &Storage) {
    storage
        .ai
        .upsert_provider(&provider("provider-1", "Provider One", "openai"))
        .await
        .unwrap();
    storage
        .ai
        .upsert_account(&account("account-1", "provider-1", "Account One"))
        .await
        .unwrap();
}

#[tokio::test]
async fn bootstrap_serves_ai_native_model() {
    let storage = storage().await;
    seed_provider_and_account(&storage).await;
    storage
        .ai
        .upsert_model(&model("model-1", "chat-model", AiModelKind::Model))
        .await
        .unwrap();
    storage
        .ai
        .upsert_target(&provider_target(
            "target-1",
            "model-1",
            "provider-1",
            "upstream-1",
        ))
        .await
        .unwrap();

    let runtime = LlmRuntime::bootstrap(storage, config()).await.unwrap();
    let snapshot = runtime.snapshot().await;
    let plan = snapshot.resolve("chat-model").expect("AI model route");
    let attempts = plan.attempts(0);
    assert!(!attempts.is_empty());
    assert_eq!(attempts[0].provider_id, "provider-1");
    assert_eq!(attempts[0].model_id, "upstream-1");
    assert_eq!(attempts[0].account_id.as_deref(), Some("account-1"));

    let provider = snapshot.provider("provider-1").expect("provider");
    assert_eq!(provider.auth, octra_llm_gateway::config::AuthType::Bearer);
}

#[tokio::test]
async fn account_oauth_state_maps_to_the_runtime() {
    let storage = storage().await;
    storage
        .ai
        .upsert_provider(&provider("provider-1", "Provider One", "openai"))
        .await
        .unwrap();
    let mut oauth_account = account("account-1", "provider-1", "Account One");
    oauth_account.credential_kind = "oauth".into();
    oauth_account.oauth_status = "authorized".into();
    oauth_account.oauth_expires_at = Some("2030-01-01T00:00:00.000Z".into());
    storage.ai.upsert_account(&oauth_account).await.unwrap();

    let runtime = LlmRuntime::bootstrap(storage, config()).await.unwrap();
    let snapshot = runtime.snapshot().await;
    let accounts = snapshot.accounts_for("provider-1");
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].record.oauth_status, "authorized");
    assert_eq!(
        accounts[0].record.oauth_expires_at.as_deref(),
        Some("2030-01-01T00:00:00.000Z")
    );
}

#[tokio::test]
async fn semantic_strategy_maps_target_descriptions() {
    let storage = storage().await;
    seed_provider_and_account(&storage).await;

    let mut semantic_model = model("semantic-model", "semantic-model", AiModelKind::Model);
    semantic_model.balancer.strategy = "semantic".into();
    storage.ai.upsert_model(&semantic_model).await.unwrap();

    let mut code_target = provider_target(
        "target-code",
        "semantic-model",
        "provider-1",
        "upstream-code",
    );
    code_target.position = 0;
    let code_target = describe(code_target, "code generation and debugging");
    let mut legal_target = provider_target(
        "target-legal",
        "semantic-model",
        "provider-1",
        "upstream-legal",
    );
    legal_target.position = 1;
    let legal_target = describe(legal_target, "legal contract review");
    storage.ai.upsert_target(&code_target).await.unwrap();
    storage.ai.upsert_target(&legal_target).await.unwrap();

    let runtime = LlmRuntime::bootstrap(storage, config()).await.unwrap();
    let snapshot = runtime.snapshot().await;

    let plan = snapshot.resolve("semantic-model").expect("semantic route");
    assert_eq!(plan.strategy, "semantic");
    assert_eq!(
        snapshot
            .data
            .target_semantic_descriptions
            .get("ai-target:semantic-model:0")
            .map(String::as_str),
        Some("code generation and debugging")
    );
    assert_eq!(
        snapshot
            .data
            .target_semantic_descriptions
            .get("ai-target:semantic-model:1")
            .map(String::as_str),
        Some("legal contract review")
    );

    let attempts = plan.attempts(0);
    let descriptions: Vec<Option<&str>> = attempts
        .iter()
        .map(|target| target.semantic_description.as_deref())
        .collect();
    assert_eq!(
        descriptions,
        vec![
            Some("code generation and debugging"),
            Some("legal contract review"),
            Some("code generation and debugging"),
        ]
    );
}

#[tokio::test]
async fn nested_group_models_flatten_to_provider_targets() {
    let storage = storage().await;
    seed_provider_and_account(&storage).await;

    storage
        .ai
        .upsert_model(&model("route-model", "route-model", AiModelKind::Model))
        .await
        .unwrap();
    storage
        .ai
        .upsert_target(&provider_target(
            "route-target",
            "route-model",
            "provider-1",
            "upstream-1",
        ))
        .await
        .unwrap();

    storage
        .ai
        .upsert_model(&model("group-model", "group-model", AiModelKind::Group))
        .await
        .unwrap();
    storage
        .ai
        .upsert_target(&nested_target("group-target", "group-model", "route-model"))
        .await
        .unwrap();

    storage
        .ai
        .upsert_model(&model("root-model", "root-model", AiModelKind::Model))
        .await
        .unwrap();
    storage
        .ai
        .upsert_target(&nested_target("root-target", "root-model", "group-model"))
        .await
        .unwrap();

    let runtime = LlmRuntime::bootstrap(storage, config()).await.unwrap();
    let snapshot = runtime.snapshot().await;

    let plan = snapshot.resolve("root-model").expect("flattened route");
    let attempts = plan.attempts(0);
    assert_eq!(attempts[0].provider_id, "provider-1");
    assert_eq!(attempts[0].model_id, "upstream-1");

    assert!(snapshot.resolve("route-model").is_some());
    // Group-kind models are routing constructs, not exposed routes.
    assert!(snapshot.resolve("group-model").is_none());
}

#[tokio::test]
async fn missing_provider_returns_config_error() {
    let data = AiControlPlaneData {
        providers: Vec::new(),
        accounts: Vec::new(),
        models: vec![model("model-1", "chat-model", AiModelKind::Model)],
        targets: vec![provider_target(
            "target-1",
            "model-1",
            "missing-provider",
            "upstream-1",
        )],
        policies: Vec::new(),
        attachments: Vec::new(),
    };

    let error = runtime_data_from_ai(&data, Vec::new()).unwrap_err();
    let Error::Config(message) = error else {
        panic!("expected a config error, got {error:?}");
    };
    assert!(message.contains("provider_not_found"), "{message}");
}

#[tokio::test]
async fn reload_picks_up_new_ai_routes() {
    let storage = storage().await;
    seed_provider_and_account(&storage).await;
    storage
        .ai
        .upsert_model(&model("model-1", "first-model", AiModelKind::Model))
        .await
        .unwrap();
    storage
        .ai
        .upsert_target(&provider_target(
            "target-1",
            "model-1",
            "provider-1",
            "upstream-1",
        ))
        .await
        .unwrap();

    let runtime = LlmRuntime::bootstrap(storage.clone(), config())
        .await
        .unwrap();
    assert!(runtime.snapshot().await.resolve("first-model").is_some());

    storage
        .ai
        .upsert_model(&model("model-2", "second-model", AiModelKind::Model))
        .await
        .unwrap();
    storage
        .ai
        .upsert_target(&provider_target(
            "target-2",
            "model-2",
            "provider-1",
            "upstream-2",
        ))
        .await
        .unwrap();
    assert!(runtime.snapshot().await.resolve("second-model").is_none());

    runtime.reload().await.unwrap();
    assert!(runtime.snapshot().await.resolve("second-model").is_some());
}

#[tokio::test]
async fn scoped_allowlists_apply_to_groups_keys_models_and_providers() {
    let data = AiControlPlaneData {
        providers: vec![provider("provider-1", "Provider One", "openai")],
        accounts: vec![account("account-1", "provider-1", "Account One")],
        models: vec![model("model-1", "chat-model", AiModelKind::Model)],
        targets: vec![provider_target(
            "target-1",
            "model-1",
            "provider-1",
            "upstream-1",
        )],
        policies: vec![
            allowlist_policy("group-allow", "Group Allow", &["other-model"]),
            allowlist_policy("key-allow", "Key Allow", &["other-model"]),
            allowlist_policy("provider-allow", "Provider Allow", &["chat-model"]),
            {
                let mut policy = allowlist_policy("model-allow", "Model Allow", &["chat-model"]);
                policy.config = json!({"allowed_providers": ["provider-1"]});
                policy
            },
        ],
        attachments: vec![
            attachment(
                "group-attachment",
                "group-allow",
                AiPolicyScope::ConsumerGroup,
                Some("group-1"),
            ),
            attachment(
                "key-attachment",
                "key-allow",
                AiPolicyScope::VirtualKey,
                Some("key-1"),
            ),
            attachment(
                "provider-attachment",
                "provider-allow",
                AiPolicyScope::Provider,
                Some("provider-1"),
            ),
            attachment(
                "model-attachment",
                "model-allow",
                AiPolicyScope::Model,
                Some("model-1"),
            ),
        ],
    };

    let runtime = runtime_data_from_ai(&data, Vec::new()).unwrap();
    assert_eq!(runtime.policies.len(), 4);
    let snapshot = RuntimeSnapshot::build(runtime);
    let base = octra_llm_gateway::runtime::PolicyContext {
        consumer_id: None,
        consumer_group_ids: &[],
        virtual_key_id: Some("key-2"),
        model: "chat-model",
        provider_id: Some("provider-1"),
        provider_name: Some("Provider One"),
    };
    assert!(snapshot.permits_model_in(&base));
    assert!(snapshot.permits_provider_in(&base));

    let restricted_group = octra_llm_gateway::runtime::PolicyContext {
        consumer_group_ids: &["group-1".to_string()],
        ..base.clone()
    };
    assert!(!snapshot.permits_model_in(&restricted_group));

    let other_group = octra_llm_gateway::runtime::PolicyContext {
        consumer_group_ids: &["group-2".to_string()],
        ..base.clone()
    };
    assert!(snapshot.permits_model_in(&other_group));

    let restricted_key = octra_llm_gateway::runtime::PolicyContext {
        virtual_key_id: Some("key-1"),
        ..base.clone()
    };
    assert!(!snapshot.permits_model_in(&restricted_key));

    let wrong_provider = octra_llm_gateway::runtime::PolicyContext {
        provider_id: Some("provider-2"),
        provider_name: Some("Provider Two"),
        ..base.clone()
    };
    assert!(!snapshot.permits_provider_in(&wrong_provider));
}

#[tokio::test]
async fn provider_target_metadata_reaches_runtime_profile_and_pricing() {
    let mut target = provider_target("target-1", "model-1", "provider-1", "upstream-1");
    target.context_window = Some(128_000);
    target.max_output_tokens = Some(8_192);
    target.capabilities = json!({"vision": true, "tools": true, "reasoning": true});
    target.pricing = json!({"input_per_million": 3.0, "output_per_million": 15.0});

    let data = AiControlPlaneData {
        providers: vec![provider("provider-1", "Provider One", "openai")],
        accounts: vec![account("account-1", "provider-1", "Account One")],
        models: vec![model("model-1", "chat-model", AiModelKind::Model)],
        targets: vec![target],
        ..Default::default()
    };

    let runtime = runtime_data_from_ai(&data, Vec::new()).unwrap();
    assert_eq!(runtime.models.len(), 1);
    let record = &runtime.models[0];
    assert_eq!(record.context_window, Some(128_000));
    assert_eq!(record.max_output_tokens, Some(8_192));
    let capabilities: serde_json::Value = serde_json::from_str(&record.capabilities).unwrap();
    assert_eq!(
        capabilities,
        json!({"vision": true, "tools": true, "reasoning": true})
    );
    let pricing: serde_json::Value = serde_json::from_str(&record.pricing_json).unwrap();
    assert_eq!(
        pricing,
        json!({"input_per_million": 3.0, "output_per_million": 15.0})
    );

    let snapshot = RuntimeSnapshot::build(runtime);
    let profile = snapshot.model_profile("provider-1", "upstream-1");
    assert_eq!(profile.context_window, Some(128_000));
    assert_eq!(profile.vision, Some(true));
    assert_eq!(profile.tools, Some(true));
    assert_eq!(profile.reasoning, Some(true));
    assert_eq!(
        snapshot.model_pricing("provider-1", "upstream-1"),
        Some(json!({"input_per_million": 3.0, "output_per_million": 15.0}))
    );
}

#[tokio::test]
async fn nested_group_metadata_does_not_override_leaf_metadata() {
    let mut leaf = provider_target("leaf-target", "leaf-model", "provider-1", "upstream-1");
    leaf.context_window = Some(64_000);
    leaf.max_output_tokens = Some(4_096);
    leaf.capabilities = json!({"vision": true});
    leaf.pricing = json!({"input_per_million": 1.0});

    let mut nested = nested_target("nested-target", "group-model", "leaf-model");
    nested.context_window = Some(1);
    nested.max_output_tokens = Some(1);
    nested.capabilities = json!({"vision": false, "tools": false});
    nested.pricing = json!({"input_per_million": 999.0});

    let data = AiControlPlaneData {
        providers: vec![provider("provider-1", "Provider One", "openai")],
        accounts: vec![account("account-1", "provider-1", "Account One")],
        models: vec![
            model("root-model", "root-model", AiModelKind::Model),
            model("group-model", "group-model", AiModelKind::Group),
            model("leaf-model", "leaf-model", AiModelKind::Model),
        ],
        targets: vec![
            nested_target("root-target", "root-model", "group-model"),
            nested,
            leaf,
        ],
        ..Default::default()
    };

    let runtime = runtime_data_from_ai(&data, Vec::new()).unwrap();
    let record = runtime
        .models
        .iter()
        .find(|record| record.model_id == "upstream-1")
        .expect("leaf endpoint should be mapped");
    assert_eq!(record.context_window, Some(64_000));
    assert_eq!(record.max_output_tokens, Some(4_096));
    let capabilities: serde_json::Value = serde_json::from_str(&record.capabilities).unwrap();
    assert_eq!(capabilities, json!({"vision": true}));
    let pricing: serde_json::Value = serde_json::from_str(&record.pricing_json).unwrap();
    assert_eq!(pricing, json!({"input_per_million": 1.0}));
}

#[tokio::test]
async fn allowlist_policies_map_to_legacy_scopes() {
    let data = AiControlPlaneData {
        providers: vec![provider("provider-1", "Provider One", "openai")],
        accounts: vec![account("account-1", "provider-1", "Account One")],
        models: vec![model("model-1", "chat-model", AiModelKind::Model)],
        targets: vec![provider_target(
            "target-1",
            "model-1",
            "provider-1",
            "upstream-1",
        )],
        policies: vec![
            allowlist_policy(
                "global-allow",
                "Global Allow",
                &["chat-model", "other-model"],
            ),
            allowlist_policy("consumer-allow", "Consumer Allow", &["chat-model"]),
            AiPolicy {
                id: "rate-limit".into(),
                name: "Rate Limit".into(),
                policy_type: AiPolicyType::RateLimit,
                config: json!({ "requests_per_minute": 10 }),
                enabled: true,
                created_at: 0,
                updated_at: 0,
            },
        ],
        attachments: vec![
            attachment("attachment-1", "global-allow", AiPolicyScope::Global, None),
            attachment(
                "attachment-2",
                "consumer-allow",
                AiPolicyScope::Consumer,
                Some("consumer-1"),
            ),
            attachment("attachment-3", "rate-limit", AiPolicyScope::Global, None),
        ],
    };

    let runtime = runtime_data_from_ai(&data, Vec::new()).unwrap();
    assert_eq!(runtime.policies.len(), 2);
    let snapshot = RuntimeSnapshot::build(runtime);
    assert!(snapshot.permits_model(None, "chat-model"));
    assert!(snapshot.permits_model(None, "other-model"));
    assert!(snapshot.permits_model(Some("consumer-1"), "chat-model"));
    assert!(!snapshot.permits_model(Some("consumer-1"), "other-model"));
    // Consumers without an attachment only see the global allowlist.
    assert!(snapshot.permits_model(Some("consumer-2"), "other-model"));
}
