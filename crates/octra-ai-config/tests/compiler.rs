//! Compiler contract tests: validation, expansion, ordering and skipping.

use octra_ai_config::{
    AiBalancer, AiControlPlaneData, AiModel, AiModelKind, AiModelTarget, AiPolicy,
    AiPolicyAttachment, AiPolicyScope, AiPolicyType, AiProvider, AiProviderAccount,
    CompiledControlPlane, MAX_GROUP_DEPTH, compile,
};
use serde_json::json;

fn provider(id: &str) -> AiProvider {
    AiProvider {
        id: id.to_string(),
        name: format!("provider-{id}"),
        protocol: "openai".to_string(),
        base_url: "https://example.test/v1".to_string(),
        auth_ref: None,
        capabilities: vec!["chat".to_string()],
        config: json!({}),
        enabled: true,
        created_at: 0,
        updated_at: 0,
    }
}

fn account(id: &str, provider_id: &str) -> AiProviderAccount {
    AiProviderAccount {
        id: id.to_string(),
        provider_id: provider_id.to_string(),
        name: format!("account-{id}"),
        credential_kind: "static".to_string(),
        secret_ref: None,
        oauth_ref: None,
        weight: 1,
        priority: 0,
        quota_limit: None,
        quota_unit: "requests".to_string(),
        quota_period: "none".to_string(),
        quota_used: 0,
        cooldown_until: None,
        enabled: true,
        oauth_status: "none".to_string(),
        oauth_expires_at: None,
        config: json!({}),
        created_at: 0,
        updated_at: 0,
    }
}

fn model(id: &str, kind: AiModelKind) -> AiModel {
    AiModel {
        id: id.to_string(),
        name: format!("model-{id}"),
        kind,
        capabilities: vec!["chat".to_string()],
        formats: vec!["openai".to_string()],
        route_paths: Vec::new(),
        balancer: AiBalancer::default(),
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
        id: id.to_string(),
        model_id: model_id.to_string(),
        target_model_id: None,
        provider_id: Some(provider_id.to_string()),
        upstream_model: Some(upstream_model.to_string()),
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

fn nested_target(id: &str, model_id: &str, target_model_id: &str) -> AiModelTarget {
    AiModelTarget {
        id: id.to_string(),
        model_id: model_id.to_string(),
        target_model_id: Some(target_model_id.to_string()),
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

fn policy(id: &str) -> AiPolicy {
    AiPolicy {
        id: id.to_string(),
        name: format!("policy-{id}"),
        policy_type: AiPolicyType::SystemPrompt,
        config: json!({ "prompt": "Be concise." }),
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
    order: i32,
) -> AiPolicyAttachment {
    AiPolicyAttachment {
        id: id.to_string(),
        policy_id: policy_id.to_string(),
        scope,
        scope_id: scope_id.map(str::to_string),
        order,
        enabled: true,
        created_at: 0,
        updated_at: 0,
    }
}

fn compile_ok(data: &AiControlPlaneData) -> CompiledControlPlane {
    compile(data).expect("configuration should compile")
}

#[test]
fn compiles_provider_account_model_and_provider_target() {
    let mut target = provider_target("target-1", "route", "provider-1", "gpt-4o");
    target.account_id = Some("account-1".to_string());

    let data = AiControlPlaneData {
        providers: vec![provider("provider-1")],
        accounts: vec![account("account-1", "provider-1")],
        models: vec![model("route", AiModelKind::Model)],
        targets: vec![target],
        ..Default::default()
    };

    let compiled = compile_ok(&data);

    let compiled_provider = compiled.providers.get("provider-1").expect("provider");
    assert_eq!(compiled_provider.provider.id, "provider-1");
    assert_eq!(compiled_provider.accounts.len(), 1);
    assert_eq!(compiled_provider.accounts[0].id, "account-1");

    let compiled_model = compiled.models.get("route").expect("model");
    assert_eq!(compiled_model.targets.len(), 1);
    let compiled_target = &compiled_model.targets[0];
    assert_eq!(compiled_target.target.id, "target-1");
    assert!(compiled_target.nested_model_id.is_none());
    let provider_ref = compiled_target.provider.as_ref().expect("provider ref");
    assert_eq!(provider_ref.provider_id, "provider-1");
    assert_eq!(provider_ref.upstream_model, "gpt-4o");
    assert_eq!(provider_ref.account_id.as_deref(), Some("account-1"));
}

#[test]
fn expands_route_model_into_group_model_and_provider_target() {
    let data = AiControlPlaneData {
        providers: vec![provider("provider-1")],
        models: vec![
            model("route", AiModelKind::Model),
            model("group", AiModelKind::Group),
        ],
        targets: vec![
            nested_target("route-target", "route", "group"),
            provider_target("group-target", "group", "provider-1", "gpt-4o"),
        ],
        ..Default::default()
    };

    let compiled = compile_ok(&data);

    let route = compiled.models.get("route").expect("route");
    assert_eq!(route.targets.len(), 1);
    assert_eq!(route.targets[0].nested_model_id.as_deref(), Some("group"));
    assert!(route.targets[0].provider.is_none());

    let group = compiled.models.get("group").expect("group");
    assert_eq!(group.targets.len(), 1);
    assert_eq!(group.targets[0].target.id, "group-target");
    let provider_ref = group.targets[0].provider.as_ref().expect("provider ref");
    assert_eq!(provider_ref.provider_id, "provider-1");
    assert_eq!(provider_ref.upstream_model, "gpt-4o");
}

#[test]
fn rejects_missing_provider_reference() {
    let data = AiControlPlaneData {
        providers: vec![provider("provider-1")],
        models: vec![model("route", AiModelKind::Model)],
        targets: vec![provider_target("target-1", "route", "missing", "gpt-4o")],
        ..Default::default()
    };

    let errors = compile(&data).expect_err("missing provider must fail");
    assert!(
        errors
            .iter()
            .any(|error| error.code == "provider_not_found" && error.message.contains("missing")),
        "unexpected errors: {errors:?}"
    );
}

#[test]
fn rejects_target_with_both_nested_and_provider_fields() {
    let mut target = provider_target("target-1", "route", "provider-1", "gpt-4o");
    target.target_model_id = Some("group".to_string());

    let data = AiControlPlaneData {
        providers: vec![provider("provider-1")],
        models: vec![
            model("route", AiModelKind::Model),
            model("group", AiModelKind::Group),
        ],
        targets: vec![
            target,
            provider_target("group-target", "group", "provider-1", "gpt-4o"),
        ],
        ..Default::default()
    };

    let errors = compile(&data).expect_err("ambiguous target must fail");
    assert!(
        errors
            .iter()
            .any(|error| error.code == "invalid_target" && error.message.contains("not both")),
        "unexpected errors: {errors:?}"
    );
}

#[test]
fn rejects_target_without_nested_or_provider_fields() {
    let target = AiModelTarget {
        id: "target-1".to_string(),
        model_id: "route".to_string(),
        target_model_id: None,
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
    };

    let data = AiControlPlaneData {
        providers: vec![provider("provider-1")],
        models: vec![model("route", AiModelKind::Model)],
        targets: vec![target],
        ..Default::default()
    };

    let errors = compile(&data).expect_err("empty target must fail");
    assert!(
        errors
            .iter()
            .any(|error| error.code == "invalid_target" && error.message.contains("exactly one")),
        "unexpected errors: {errors:?}"
    );
}

#[test]
fn rejects_nesting_cycle() {
    let data = AiControlPlaneData {
        providers: vec![provider("provider-1")],
        models: vec![
            model("model-a", AiModelKind::Group),
            model("model-b", AiModelKind::Group),
        ],
        targets: vec![
            nested_target("target-a", "model-a", "model-b"),
            nested_target("target-b", "model-b", "model-a"),
        ],
        ..Default::default()
    };

    let errors = compile(&data).expect_err("cycle must fail");
    assert!(
        errors.iter().any(|error| error.code == "cycle"),
        "unexpected errors: {errors:?}"
    );
}

#[test]
fn rejects_nesting_deeper_than_max_group_depth() {
    let last = MAX_GROUP_DEPTH + 1;
    let mut models = Vec::new();
    let mut targets = Vec::new();

    for index in 0..=last {
        models.push(model(&format!("model-{index}"), AiModelKind::Group));
    }
    for index in 0..last {
        targets.push(nested_target(
            &format!("target-{index}"),
            &format!("model-{index}"),
            &format!("model-{}", index + 1),
        ));
    }
    targets.push(provider_target(
        "target-leaf",
        &format!("model-{last}"),
        "provider-1",
        "gpt-4o",
    ));

    let data = AiControlPlaneData {
        providers: vec![provider("provider-1")],
        models,
        targets,
        ..Default::default()
    };

    let errors = compile(&data).expect_err("depth overflow must fail");
    assert!(
        errors.iter().any(|error| error.code == "max_depth"),
        "unexpected errors: {errors:?}"
    );
}

#[test]
fn orders_policies_by_scope_specificity_and_attachment_order() {
    let data = AiControlPlaneData {
        providers: vec![provider("provider-1")],
        models: vec![model("route", AiModelKind::Model)],
        targets: vec![provider_target("target-1", "route", "provider-1", "gpt-4o")],
        policies: vec![
            policy("policy-target-first"),
            policy("policy-target-second"),
            policy("policy-model"),
            policy("policy-provider"),
            policy("policy-global-early"),
            policy("policy-global-a"),
            policy("policy-global-b"),
        ],
        attachments: vec![
            attachment(
                "attachment-global-b",
                "policy-global-b",
                AiPolicyScope::Global,
                None,
                0,
            ),
            attachment(
                "attachment-global-early",
                "policy-global-early",
                AiPolicyScope::Global,
                None,
                -1,
            ),
            attachment(
                "attachment-global-a",
                "policy-global-a",
                AiPolicyScope::Global,
                None,
                0,
            ),
            attachment(
                "attachment-provider",
                "policy-provider",
                AiPolicyScope::Provider,
                Some("provider-1"),
                0,
            ),
            attachment(
                "attachment-model",
                "policy-model",
                AiPolicyScope::Model,
                Some("route"),
                0,
            ),
            attachment(
                "attachment-target-second",
                "policy-target-second",
                AiPolicyScope::Target,
                Some("target-1"),
                0,
            ),
            attachment(
                "attachment-target-first",
                "policy-target-first",
                AiPolicyScope::Target,
                Some("target-1"),
                -1,
            ),
        ],
        ..Default::default()
    };

    let compiled = compile_ok(&data);

    let expected = vec![
        "policy-target-first".to_string(),
        "policy-target-second".to_string(),
        "policy-model".to_string(),
        "policy-provider".to_string(),
        "policy-global-early".to_string(),
        "policy-global-a".to_string(),
        "policy-global-b".to_string(),
    ];
    let route = compiled.models.get("route").expect("route");
    assert_eq!(route.targets[0].policy_ids, expected);

    assert_eq!(
        route.policy_ids,
        vec![
            "policy-model".to_string(),
            "policy-global-early".to_string(),
            "policy-global-a".to_string(),
            "policy-global-b".to_string(),
        ]
    );

    assert_eq!(
        compiled.policy_chain_for(&[(AiPolicyScope::Global, None)]),
        vec![
            "policy-global-early".to_string(),
            "policy-global-a".to_string(),
            "policy-global-b".to_string(),
        ]
    );
}

#[test]
fn skips_disabled_entities() {
    let mut disabled_account = account("account-off", "provider-1");
    disabled_account.enabled = false;

    let mut disabled_model = model("model-off", AiModelKind::Group);
    disabled_model.enabled = false;

    let mut disabled_target = provider_target("target-off", "model-off", "provider-1", "gpt-4o");
    disabled_target.enabled = false;

    let mut disabled_policy = policy("policy-off");
    disabled_policy.enabled = false;

    let mut disabled_attachment = attachment(
        "attachment-off",
        "policy-off",
        AiPolicyScope::Model,
        Some("route"),
        0,
    );
    disabled_attachment.enabled = false;

    let data = AiControlPlaneData {
        providers: vec![provider("provider-1")],
        accounts: vec![account("account-on", "provider-1"), disabled_account],
        models: vec![model("route", AiModelKind::Model), disabled_model],
        targets: vec![
            provider_target("target-on", "route", "provider-1", "gpt-4o"),
            disabled_target,
        ],
        policies: vec![policy("policy-on"), disabled_policy],
        attachments: vec![
            attachment(
                "attachment-on",
                "policy-on",
                AiPolicyScope::Model,
                Some("route"),
                0,
            ),
            disabled_attachment,
        ],
    };

    let compiled = compile_ok(&data);

    let accounts = &compiled
        .providers
        .get("provider-1")
        .expect("provider")
        .accounts;
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].id, "account-on");

    assert!(!compiled.models.contains_key("model-off"));
    assert_eq!(compiled.policies.len(), 1);
    assert!(compiled.policies.contains_key("policy-on"));
    assert_eq!(compiled.attachments.len(), 1);
    assert_eq!(compiled.attachments[0].id, "attachment-on");
}

#[test]
fn rejects_policy_config_that_is_not_an_object() {
    let mut invalid_policy = policy("policy-invalid");
    invalid_policy.config = json!("not-an-object");

    let data = AiControlPlaneData {
        providers: vec![provider("provider-1")],
        models: vec![model("route", AiModelKind::Model)],
        targets: vec![provider_target("target-1", "route", "provider-1", "gpt-4o")],
        policies: vec![invalid_policy],
        ..Default::default()
    };

    let errors = compile(&data).expect_err("non-object policy config must fail");
    assert!(
        errors
            .iter()
            .any(|error| error.code == "invalid_policy" && error.message.contains("policy-invalid")),
        "unexpected errors: {errors:?}"
    );
}

#[test]
fn serializes_enums_as_snake_case_strings() {
    assert_eq!(
        serde_json::to_string(&AiModelKind::Group).expect("serialize model kind"),
        "\"group\""
    );
    assert_eq!(
        serde_json::to_string(&AiPolicyScope::ConsumerGroup).expect("serialize policy scope"),
        "\"consumer_group\""
    );
    assert_eq!(
        serde_json::to_string(&AiPolicyType::PiiRedaction).expect("serialize policy type"),
        "\"pii_redaction\""
    );
}

#[test]
fn rejects_enabled_model_without_enabled_targets() {
    let mut disabled_target = provider_target("target-off", "route", "provider-1", "gpt-4o");
    disabled_target.enabled = false;

    let data = AiControlPlaneData {
        providers: vec![provider("provider-1")],
        models: vec![model("route", AiModelKind::Model)],
        targets: vec![disabled_target],
        ..Default::default()
    };

    let errors = compile(&data).expect_err("model without targets must fail");
    assert!(
        errors
            .iter()
            .any(|error| error.code == "missing_target" && error.message.contains("route")),
        "unexpected errors: {errors:?}"
    );
}

#[test]
fn accepts_valid_target_model_metadata() {
    let mut target = provider_target("target-1", "route", "provider-1", "gpt-4o");
    target.context_window = Some(128_000);
    target.max_output_tokens = Some(16_384);
    target.capabilities = json!({"vision": true, "tools": true, "reasoning": true});
    target.pricing = json!({"input_per_million": 3.0, "output_per_million": 15.0});

    let data = AiControlPlaneData {
        providers: vec![provider("provider-1")],
        models: vec![model("route", AiModelKind::Model)],
        targets: vec![target],
        ..Default::default()
    };

    let compiled = compile_ok(&data);
    let compiled_target = &compiled.models.get("route").expect("route").targets[0];
    assert_eq!(compiled_target.target.context_window, Some(128_000));
    assert_eq!(compiled_target.target.max_output_tokens, Some(16_384));
    assert_eq!(
        compiled_target.target.capabilities,
        json!({"vision": true, "tools": true, "reasoning": true})
    );
    assert_eq!(
        compiled_target.target.pricing,
        json!({"input_per_million": 3.0, "output_per_million": 15.0})
    );
}

#[test]
fn rejects_invalid_target_model_metadata() {
    let mut target = provider_target("target-1", "route", "provider-1", "gpt-4o");
    target.context_window = Some(0);
    target.max_output_tokens = Some(-1);
    target.capabilities = json!(["vision"]);
    target.pricing = json!("free");

    let data = AiControlPlaneData {
        providers: vec![provider("provider-1")],
        models: vec![model("route", AiModelKind::Model)],
        targets: vec![target],
        ..Default::default()
    };

    let errors = compile(&data).expect_err("invalid metadata must fail");
    for field in [
        "context_window",
        "max_output_tokens",
        "capabilities",
        "pricing",
    ] {
        assert!(
            errors.iter().any(|error| {
                error.code == "invalid_target"
                    && error.message.contains("target-1")
                    && error.message.contains(field)
            }),
            "missing invalid_target error for {field}: {errors:?}"
        );
    }
}

#[test]
fn target_deserialization_defaults_missing_metadata() {
    let target: AiModelTarget = serde_json::from_value(json!({
        "id": "target-1",
        "model_id": "route",
        "target_model_id": null,
        "provider_id": "provider-1",
        "upstream_model": "gpt-4o",
        "account_id": null,
        "weight": 1,
        "priority": 0,
        "position": 0,
        "config": {},
        "semantic_description": null,
        "enabled": true,
        "created_at": 0,
        "updated_at": 0
    }))
    .expect("target without metadata should deserialize");

    assert_eq!(target.context_window, None);
    assert_eq!(target.max_output_tokens, None);
    assert_eq!(target.capabilities, json!({}));
    assert_eq!(target.pricing, json!({}));
}

#[test]
fn empty_balancer_object_uses_defaults() {
    let balancer: AiBalancer = serde_json::from_str("{}").expect("empty balancer object");
    assert_eq!(balancer, AiBalancer::default());
    assert_eq!(balancer.strategy, "priority");
    assert_eq!(balancer.retries, 0);
    assert!(balancer.failover_criteria.is_empty());
}
