//! Validation and compilation of flat control-plane records.
//!
//! The compiler validates every enabled record, reports all problems at once,
//! and then builds an indexed view for the data plane. Disabled records are
//! skipped entirely (they are drafts), except for duplicate ID/name checks,
//! which mirror the storage uniqueness constraints.

use std::collections::{BTreeMap, HashSet};

use serde_json::Value;

use crate::types::{
    AiControlPlaneData, AiModel, AiModelTarget, AiPolicy, AiPolicyAttachment, AiPolicyScope,
    AiProvider, AiProviderAccount,
};

/// Maximum number of nested model expansions allowed below a root model.
///
/// A root model has depth 0, so a chain that nests `MAX_GROUP_DEPTH + 1` models
/// below the root is rejected.
pub const MAX_GROUP_DEPTH: usize = 8;

/// A single compilation or validation failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {message}")]
pub struct CompileError {
    pub code: String,
    pub message: String,
}

/// Validated, indexed control plane consumed by the data plane.
#[derive(Debug, Clone, PartialEq)]
pub struct CompiledControlPlane {
    pub providers: BTreeMap<String, CompiledProvider>,
    pub models: BTreeMap<String, CompiledModel>,
    pub policies: BTreeMap<String, AiPolicy>,
    pub attachments: Vec<AiPolicyAttachment>,
}

/// An enabled provider together with its enabled accounts.
#[derive(Debug, Clone, PartialEq)]
pub struct CompiledProvider {
    pub provider: AiProvider,
    pub accounts: Vec<AiProviderAccount>,
}

/// An enabled model with its ordered targets and effective policy chain.
#[derive(Debug, Clone, PartialEq)]
pub struct CompiledModel {
    pub model: AiModel,
    pub targets: Vec<CompiledTarget>,
    pub policy_ids: Vec<String>,
}

/// One resolved expansion of a model.
#[derive(Debug, Clone, PartialEq)]
pub struct CompiledTarget {
    pub target: AiModelTarget,
    pub provider: Option<CompiledProviderRef>,
    pub nested_model_id: Option<String>,
    pub policy_ids: Vec<String>,
}

/// Provider endpoint that a target ultimately dispatches to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledProviderRef {
    pub provider_id: String,
    pub upstream_model: String,
    pub account_id: Option<String>,
}

impl CompiledControlPlane {
    /// Returns enabled policy IDs for the supplied scopes, most specific first.
    ///
    /// Scope rank: virtual_key < consumer < consumer_group < target < model <
    /// provider < global. Within one scope, attachments are ordered by
    /// `order` ascending and then by attachment ID. A policy attached at
    /// multiple scopes appears once, at its most specific position.
    pub fn policy_chain_for(&self, scopes: &[(AiPolicyScope, Option<String>)]) -> Vec<String> {
        policy_chain(&self.policies, &self.attachments, scopes)
    }
}

/// Compiles flat control-plane records into an indexed data-plane view.
///
/// All enabled records are validated; every problem is collected before an
/// error is returned.
pub fn compile(data: &AiControlPlaneData) -> Result<CompiledControlPlane, Vec<CompileError>> {
    let mut errors = ErrorSink::default();

    validate_unique_ids(data, &mut errors);
    validate_unique_names(data, &mut errors);

    let providers = index_providers(data);
    let accounts = index_accounts(data);
    let models = index_models(data);
    let policies = index_policies(data);
    let targets = index_targets(data);

    validate_providers(&providers, &mut errors);
    validate_accounts(data, &providers, &mut errors);
    validate_models(data, &mut errors);
    validate_policies(data, &mut errors);
    let invalid_targets = validate_targets(data, &providers, &accounts, &models, &mut errors);
    validate_attachments(data, &providers, &models, &targets, &policies, &mut errors);
    validate_nesting(data, &models, &invalid_targets, &mut errors);
    validate_models_have_targets(data, &invalid_targets, &mut errors);

    if !errors.errors.is_empty() {
        return Err(errors.errors);
    }

    Ok(build_compiled(data))
}

/// Collects errors without duplicating identical `(code, message)` pairs.
#[derive(Default)]
struct ErrorSink {
    errors: Vec<CompileError>,
    seen: HashSet<(String, String)>,
}

impl ErrorSink {
    fn push(&mut self, code: &str, message: impl Into<String>) {
        let message = message.into();
        let key = (code.to_string(), message.clone());
        if self.seen.insert(key) {
            self.errors.push(CompileError {
                code: code.to_string(),
                message,
            });
        }
    }
}

fn index_providers(data: &AiControlPlaneData) -> BTreeMap<&str, &AiProvider> {
    let mut index = BTreeMap::new();
    for provider in &data.providers {
        index.entry(provider.id.as_str()).or_insert(provider);
    }
    index
}

fn index_accounts(data: &AiControlPlaneData) -> BTreeMap<&str, &AiProviderAccount> {
    let mut index = BTreeMap::new();
    for account in &data.accounts {
        index.entry(account.id.as_str()).or_insert(account);
    }
    index
}

fn index_models(data: &AiControlPlaneData) -> BTreeMap<&str, &AiModel> {
    let mut index = BTreeMap::new();
    for model in &data.models {
        index.entry(model.id.as_str()).or_insert(model);
    }
    index
}

fn index_targets(data: &AiControlPlaneData) -> BTreeMap<&str, &AiModelTarget> {
    let mut index = BTreeMap::new();
    for target in &data.targets {
        index.entry(target.id.as_str()).or_insert(target);
    }
    index
}

fn index_policies(data: &AiControlPlaneData) -> BTreeMap<&str, &AiPolicy> {
    let mut index = BTreeMap::new();
    for policy in &data.policies {
        index.entry(policy.id.as_str()).or_insert(policy);
    }
    index
}

fn validate_unique_ids(data: &AiControlPlaneData, errors: &mut ErrorSink) {
    check_duplicate_ids(
        "provider",
        data.providers.iter().map(|item| item.id.as_str()),
        errors,
    );
    check_duplicate_ids(
        "account",
        data.accounts.iter().map(|item| item.id.as_str()),
        errors,
    );
    check_duplicate_ids(
        "model",
        data.models.iter().map(|item| item.id.as_str()),
        errors,
    );
    check_duplicate_ids(
        "target",
        data.targets.iter().map(|item| item.id.as_str()),
        errors,
    );
    check_duplicate_ids(
        "policy",
        data.policies.iter().map(|item| item.id.as_str()),
        errors,
    );
    check_duplicate_ids(
        "attachment",
        data.attachments.iter().map(|item| item.id.as_str()),
        errors,
    );
}

fn check_duplicate_ids<'a>(kind: &str, ids: impl Iterator<Item = &'a str>, errors: &mut ErrorSink) {
    let mut seen = HashSet::new();
    let mut reported = HashSet::new();
    for id in ids {
        if !seen.insert(id) && reported.insert(id) {
            errors.push("duplicate_id", format!("duplicate {kind} id '{id}'"));
        }
    }
}

fn validate_unique_names(data: &AiControlPlaneData, errors: &mut ErrorSink) {
    check_duplicate_names(
        "provider",
        data.providers.iter().map(|item| item.name.as_str()),
        errors,
    );
    check_duplicate_names(
        "model",
        data.models.iter().map(|item| item.name.as_str()),
        errors,
    );
}

fn check_duplicate_names<'a>(
    kind: &str,
    names: impl Iterator<Item = &'a str>,
    errors: &mut ErrorSink,
) {
    let mut seen = HashSet::new();
    let mut reported = HashSet::new();
    for name in names {
        if !seen.insert(name) && reported.insert(name) {
            errors.push("duplicate_name", format!("duplicate {kind} name '{name}'"));
        }
    }
}

fn validate_providers(providers: &BTreeMap<&str, &AiProvider>, errors: &mut ErrorSink) {
    for provider in providers.values().copied().filter(|item| item.enabled) {
        if provider.name.trim().is_empty() {
            errors.push(
                "invalid_provider",
                format!("provider '{}' has an empty name", provider.id),
            );
        }
        if provider.protocol.trim().is_empty() {
            errors.push(
                "invalid_provider",
                format!("provider '{}' has an empty protocol", provider.id),
            );
        }
        if provider.base_url.trim().is_empty() && !has_cli_config(&provider.config) {
            errors.push(
                "invalid_provider",
                format!(
                    "provider '{}' has an empty base_url and no non-empty `cli` config object",
                    provider.id
                ),
            );
        }
    }
}

fn has_cli_config(config: &Value) -> bool {
    config
        .get("cli")
        .and_then(Value::as_object)
        .is_some_and(|cli| !cli.is_empty())
}

fn validate_accounts(
    data: &AiControlPlaneData,
    providers: &BTreeMap<&str, &AiProvider>,
    errors: &mut ErrorSink,
) {
    for account in data.accounts.iter().filter(|item| item.enabled) {
        if account.name.trim().is_empty() {
            errors.push(
                "invalid_account",
                format!("account '{}' has an empty name", account.id),
            );
        }
        if account.credential_kind.trim().is_empty() {
            errors.push(
                "invalid_account",
                format!("account '{}' has an empty credential kind", account.id),
            );
        }
        if !providers.contains_key(account.provider_id.as_str()) {
            errors.push(
                "provider_not_found",
                format!(
                    "account '{}' references unknown provider '{}'",
                    account.id, account.provider_id
                ),
            );
        }
    }
}

fn validate_models(data: &AiControlPlaneData, errors: &mut ErrorSink) {
    for model in data.models.iter().filter(|item| item.enabled) {
        if model.name.trim().is_empty() {
            errors.push(
                "invalid_model",
                format!("model '{}' has an empty name", model.id),
            );
        }
        if model.capabilities.is_empty() {
            errors.push(
                "invalid_model",
                format!("model '{}' has no capabilities", model.id),
            );
        }
        if model.formats.is_empty() {
            errors.push(
                "invalid_model",
                format!("model '{}' has no formats", model.id),
            );
        }
        if model.balancer.strategy.trim().is_empty() {
            errors.push(
                "invalid_model",
                format!("model '{}' has an empty balancer strategy", model.id),
            );
        }
    }
}

fn validate_policies(data: &AiControlPlaneData, errors: &mut ErrorSink) {
    for policy in data.policies.iter().filter(|item| item.enabled) {
        if !policy.config.is_object() {
            errors.push(
                "invalid_policy",
                format!("policy '{}' config must be a JSON object", policy.id),
            );
        }
    }
}

/// Validates enabled targets and returns the IDs of invalid ones.
fn validate_targets(
    data: &AiControlPlaneData,
    providers: &BTreeMap<&str, &AiProvider>,
    accounts: &BTreeMap<&str, &AiProviderAccount>,
    models: &BTreeMap<&str, &AiModel>,
    errors: &mut ErrorSink,
) -> HashSet<String> {
    let mut invalid = HashSet::new();

    for target in data.targets.iter().filter(|item| item.enabled) {
        let mut valid = true;

        if !models.contains_key(target.model_id.as_str()) {
            errors.push(
                "model_not_found",
                format!(
                    "model target '{}' references unknown model '{}'",
                    target.id, target.model_id
                ),
            );
            valid = false;
        }

        let nested_model_id = non_empty(target.target_model_id.as_deref());
        let provider_id = non_empty(target.provider_id.as_deref());
        let upstream_model = non_empty(target.upstream_model.as_deref());
        let account_id = non_empty(target.account_id.as_deref());

        match (nested_model_id, provider_id, upstream_model) {
            (Some(_), Some(_), _) => {
                errors.push(
                    "invalid_target",
                    format!(
                        "model target '{}' must set exactly one of `target_model_id` or `provider_id` + `upstream_model`, not both",
                        target.id
                    ),
                );
                valid = false;
            }
            (Some(_), None, Some(_)) => {
                errors.push(
                    "invalid_target",
                    format!(
                        "model target '{}' must set `provider_id` when `upstream_model` is present",
                        target.id
                    ),
                );
                valid = false;
            }
            (Some(nested_model_id), None, None) => {
                if account_id.is_some() {
                    errors.push(
                        "invalid_target",
                        format!(
                            "model target '{}' sets `account_id`, which is only valid for provider targets",
                            target.id
                        ),
                    );
                    valid = false;
                }
                match models.get(nested_model_id) {
                    None => {
                        errors.push(
                            "model_not_found",
                            format!(
                                "model target '{}' references unknown nested model '{}'",
                                target.id, nested_model_id
                            ),
                        );
                        valid = false;
                    }
                    Some(model) if !model.enabled => {
                        errors.push(
                            "model_disabled",
                            format!(
                                "model target '{}' references disabled nested model '{}'",
                                target.id, nested_model_id
                            ),
                        );
                        valid = false;
                    }
                    Some(_) => {}
                }
            }
            (None, Some(provider_id), Some(_)) => {
                match providers.get(provider_id) {
                    None => {
                        errors.push(
                            "provider_not_found",
                            format!(
                                "model target '{}' references unknown provider '{}'",
                                target.id, provider_id
                            ),
                        );
                        valid = false;
                    }
                    Some(provider) if !provider.enabled => {
                        errors.push(
                            "provider_disabled",
                            format!(
                                "model target '{}' references disabled provider '{}'",
                                target.id, provider_id
                            ),
                        );
                        valid = false;
                    }
                    Some(_) => {}
                }
                if let Some(account_id) = account_id {
                    match accounts.get(account_id) {
                        None => {
                            errors.push(
                                "account_not_found",
                                format!(
                                    "model target '{}' references unknown account '{}'",
                                    target.id, account_id
                                ),
                            );
                            valid = false;
                        }
                        Some(account) if account.provider_id != provider_id => {
                            errors.push(
                                "account_provider_mismatch",
                                format!(
                                    "model target '{}' references account '{}' of provider '{}', not '{}'",
                                    target.id, account_id, account.provider_id, provider_id
                                ),
                            );
                            valid = false;
                        }
                        Some(_) => {}
                    }
                }
            }
            (None, Some(_), None) => {
                errors.push(
                    "invalid_target",
                    format!(
                        "model target '{}' sets `provider_id` without `upstream_model`",
                        target.id
                    ),
                );
                valid = false;
            }
            (None, None, Some(_)) => {
                errors.push(
                    "invalid_target",
                    format!(
                        "model target '{}' sets `upstream_model` without `provider_id`",
                        target.id
                    ),
                );
                valid = false;
            }
            (None, None, None) => {
                errors.push(
                    "invalid_target",
                    format!(
                        "model target '{}' must set exactly one of `target_model_id` or `provider_id` + `upstream_model`",
                        target.id
                    ),
                );
                valid = false;
            }
        }

        if let Some(context_window) = target.context_window
            && context_window <= 0
        {
            errors.push(
                "invalid_target",
                format!(
                    "model target '{}' context_window must be greater than zero",
                    target.id
                ),
            );
            valid = false;
        }

        if let Some(max_output_tokens) = target.max_output_tokens
            && max_output_tokens <= 0
        {
            errors.push(
                "invalid_target",
                format!(
                    "model target '{}' max_output_tokens must be greater than zero",
                    target.id
                ),
            );
            valid = false;
        }

        if !target.capabilities.is_null() && !target.capabilities.is_object() {
            errors.push(
                "invalid_target",
                format!(
                    "model target '{}' capabilities must be a JSON object",
                    target.id
                ),
            );
            valid = false;
        }

        if !target.pricing.is_null() && !target.pricing.is_object() {
            errors.push(
                "invalid_target",
                format!("model target '{}' pricing must be a JSON object", target.id),
            );
            valid = false;
        }

        if !valid {
            invalid.insert(target.id.clone());
        }
    }

    invalid
}

fn validate_attachments(
    data: &AiControlPlaneData,
    providers: &BTreeMap<&str, &AiProvider>,
    models: &BTreeMap<&str, &AiModel>,
    targets: &BTreeMap<&str, &AiModelTarget>,
    policies: &BTreeMap<&str, &AiPolicy>,
    errors: &mut ErrorSink,
) {
    for attachment in data.attachments.iter().filter(|item| item.enabled) {
        match policies.get(attachment.policy_id.as_str()) {
            None => errors.push(
                "policy_not_found",
                format!(
                    "attachment '{}' references unknown policy '{}'",
                    attachment.id, attachment.policy_id
                ),
            ),
            Some(policy) if !policy.enabled => errors.push(
                "policy_disabled",
                format!(
                    "attachment '{}' references disabled policy '{}'",
                    attachment.id, attachment.policy_id
                ),
            ),
            Some(_) => {}
        }

        let scope_id = non_empty(attachment.scope_id.as_deref());
        match attachment.scope {
            AiPolicyScope::Global => {
                if scope_id.is_some() {
                    errors.push(
                        "invalid_attachment",
                        format!(
                            "attachment '{}' uses global scope but sets scope_id '{}'",
                            attachment.id,
                            attachment.scope_id.as_deref().unwrap_or_default()
                        ),
                    );
                }
            }
            AiPolicyScope::Provider => match scope_id {
                None => errors.push(
                    "invalid_attachment",
                    format!(
                        "attachment '{}' uses provider scope without a scope_id",
                        attachment.id
                    ),
                ),
                Some(id) if !providers.contains_key(id) => errors.push(
                    "provider_not_found",
                    format!(
                        "attachment '{}' references unknown provider scope '{}'",
                        attachment.id, id
                    ),
                ),
                Some(_) => {}
            },
            AiPolicyScope::Model => match scope_id {
                None => errors.push(
                    "invalid_attachment",
                    format!(
                        "attachment '{}' uses model scope without a scope_id",
                        attachment.id
                    ),
                ),
                Some(id) if !models.contains_key(id) => errors.push(
                    "model_not_found",
                    format!(
                        "attachment '{}' references unknown model scope '{}'",
                        attachment.id, id
                    ),
                ),
                Some(_) => {}
            },
            AiPolicyScope::Target => match scope_id {
                None => errors.push(
                    "invalid_attachment",
                    format!(
                        "attachment '{}' uses target scope without a scope_id",
                        attachment.id
                    ),
                ),
                Some(id) if !targets.contains_key(id) => errors.push(
                    "target_not_found",
                    format!(
                        "attachment '{}' references unknown target scope '{}'",
                        attachment.id, id
                    ),
                ),
                Some(_) => {}
            },
            AiPolicyScope::Consumer | AiPolicyScope::ConsumerGroup | AiPolicyScope::VirtualKey => {
                if scope_id.is_none() {
                    errors.push(
                        "invalid_attachment",
                        format!(
                            "attachment '{}' uses consumer scope without a scope_id",
                            attachment.id
                        ),
                    );
                }
            }
        }
    }
}

/// Rejects nesting cycles and expansion chains deeper than [`MAX_GROUP_DEPTH`].
fn validate_nesting(
    data: &AiControlPlaneData,
    models: &BTreeMap<&str, &AiModel>,
    invalid_targets: &HashSet<String>,
    errors: &mut ErrorSink,
) {
    let mut edges: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for target in data.targets.iter().filter(|item| item.enabled) {
        if invalid_targets.contains(&target.id) {
            continue;
        }
        let Some(nested_model_id) = non_empty(target.target_model_id.as_deref()) else {
            continue;
        };
        let Some(owner) = models.get(target.model_id.as_str()) else {
            continue;
        };
        if !owner.enabled {
            continue;
        }
        edges
            .entry(owner.id.as_str())
            .or_default()
            .push(nested_model_id);
    }

    let mut reported_cycles: HashSet<Vec<String>> = HashSet::new();
    let mut reported_depth: HashSet<String> = HashSet::new();

    for root in edges.keys().copied() {
        let mut path = Vec::new();
        visit_nesting(
            root,
            0,
            &edges,
            &mut path,
            &mut reported_cycles,
            &mut reported_depth,
            errors,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn visit_nesting(
    node: &str,
    depth: usize,
    edges: &BTreeMap<&str, Vec<&str>>,
    path: &mut Vec<String>,
    reported_cycles: &mut HashSet<Vec<String>>,
    reported_depth: &mut HashSet<String>,
    errors: &mut ErrorSink,
) {
    if depth > MAX_GROUP_DEPTH {
        if reported_depth.insert(node.to_string()) {
            errors.push(
                "max_depth",
                format!(
                    "model nesting exceeds MAX_GROUP_DEPTH ({MAX_GROUP_DEPTH}) at model '{node}'"
                ),
            );
        }
        return;
    }

    path.push(node.to_string());
    if let Some(children) = edges.get(node) {
        for child in children {
            if let Some(start) = path.iter().position(|item| item.as_str() == *child) {
                let mut members = path[start..].to_vec();
                members.sort();
                members.dedup();
                if reported_cycles.insert(members) {
                    let mut chain = path[start..].to_vec();
                    chain.push((*child).to_string());
                    errors.push(
                        "cycle",
                        format!("model nesting cycle detected: {}", chain.join(" -> ")),
                    );
                }
                continue;
            }
            visit_nesting(
                child,
                depth + 1,
                edges,
                path,
                reported_cycles,
                reported_depth,
                errors,
            );
        }
    }
    path.pop();
}

fn validate_models_have_targets(
    data: &AiControlPlaneData,
    invalid_targets: &HashSet<String>,
    errors: &mut ErrorSink,
) {
    let mut models_with_targets = HashSet::new();
    for target in data.targets.iter().filter(|item| item.enabled) {
        if !invalid_targets.contains(&target.id) {
            models_with_targets.insert(target.model_id.as_str());
        }
    }

    for model in data.models.iter().filter(|item| item.enabled) {
        if !models_with_targets.contains(model.id.as_str()) {
            errors.push(
                "missing_target",
                format!("model '{}' has no enabled valid targets", model.id),
            );
        }
    }
}

fn build_compiled(data: &AiControlPlaneData) -> CompiledControlPlane {
    let policies: BTreeMap<String, AiPolicy> = data
        .policies
        .iter()
        .filter(|policy| policy.enabled)
        .map(|policy| (policy.id.clone(), policy.clone()))
        .collect();

    let mut attachments: Vec<AiPolicyAttachment> = data
        .attachments
        .iter()
        .filter(|attachment| attachment.enabled)
        .map(normalize_attachment)
        .collect();
    attachments.sort_by(|left, right| {
        scope_rank(&left.scope)
            .cmp(&scope_rank(&right.scope))
            .then_with(|| {
                left.scope_id
                    .as_deref()
                    .unwrap_or_default()
                    .cmp(right.scope_id.as_deref().unwrap_or_default())
            })
            .then_with(|| left.order.cmp(&right.order))
            .then_with(|| left.id.cmp(&right.id))
    });

    let mut compiled = CompiledControlPlane {
        providers: BTreeMap::new(),
        models: BTreeMap::new(),
        policies,
        attachments,
    };

    for provider in data.providers.iter().filter(|item| item.enabled) {
        let mut accounts: Vec<AiProviderAccount> = data
            .accounts
            .iter()
            .filter(|account| account.enabled && account.provider_id == provider.id)
            .cloned()
            .collect();
        accounts.sort_by(|left, right| {
            left.priority
                .cmp(&right.priority)
                .then_with(|| left.id.cmp(&right.id))
        });
        compiled.providers.insert(
            provider.id.clone(),
            CompiledProvider {
                provider: provider.clone(),
                accounts,
            },
        );
    }

    for model in data.models.iter().filter(|item| item.enabled) {
        let mut targets: Vec<&AiModelTarget> = data
            .targets
            .iter()
            .filter(|target| target.enabled && target.model_id == model.id)
            .collect();
        targets.sort_by(|left, right| {
            left.priority
                .cmp(&right.priority)
                .then_with(|| left.position.cmp(&right.position))
                .then_with(|| left.id.cmp(&right.id))
        });

        let compiled_targets = targets
            .into_iter()
            .map(|target| compile_target(target, model, &compiled.policies, &compiled.attachments))
            .collect();
        let policy_ids = policy_chain(
            &compiled.policies,
            &compiled.attachments,
            &[
                (AiPolicyScope::Global, None),
                (AiPolicyScope::Model, Some(model.id.clone())),
            ],
        );

        compiled.models.insert(
            model.id.clone(),
            CompiledModel {
                model: model.clone(),
                targets: compiled_targets,
                policy_ids,
            },
        );
    }

    compiled
}

fn compile_target(
    target: &AiModelTarget,
    model: &AiModel,
    policies: &BTreeMap<String, AiPolicy>,
    attachments: &[AiPolicyAttachment],
) -> CompiledTarget {
    let nested_model_id = non_empty(target.target_model_id.as_deref()).map(str::to_string);

    let provider =
        non_empty(target.provider_id.as_deref()).map(|provider_id| CompiledProviderRef {
            provider_id: provider_id.to_string(),
            upstream_model: non_empty(target.upstream_model.as_deref())
                .unwrap_or_default()
                .to_string(),
            account_id: non_empty(target.account_id.as_deref()).map(str::to_string),
        });

    let mut scopes = vec![
        (AiPolicyScope::Global, None),
        (AiPolicyScope::Model, Some(model.id.clone())),
    ];
    if let Some(provider) = &provider {
        scopes.push((AiPolicyScope::Provider, Some(provider.provider_id.clone())));
    }
    scopes.push((AiPolicyScope::Target, Some(target.id.clone())));

    CompiledTarget {
        target: target.clone(),
        provider,
        nested_model_id,
        policy_ids: policy_chain(policies, attachments, &scopes),
    }
}

fn normalize_attachment(attachment: &AiPolicyAttachment) -> AiPolicyAttachment {
    let mut normalized = attachment.clone();
    normalized.scope_id = match normalized.scope_id.as_deref() {
        Some(scope_id) if scope_id.trim().is_empty() => None,
        Some(scope_id) => Some(scope_id.trim().to_string()),
        None => None,
    };
    normalized
}

fn policy_chain(
    policies: &BTreeMap<String, AiPolicy>,
    attachments: &[AiPolicyAttachment],
    scopes: &[(AiPolicyScope, Option<String>)],
) -> Vec<String> {
    let mut matched: Vec<(u8, i32, &str, &str)> = Vec::new();

    for (scope, scope_id) in scopes {
        let rank = scope_rank(scope);
        for attachment in attachments {
            if !attachment.enabled || &attachment.scope != scope {
                continue;
            }
            if !attachment_matches_scope(attachment, scope_id.as_deref()) {
                continue;
            }
            let Some(policy) = policies.get(&attachment.policy_id) else {
                continue;
            };
            if !policy.enabled {
                continue;
            }
            matched.push((
                rank,
                attachment.order,
                attachment.id.as_str(),
                attachment.policy_id.as_str(),
            ));
        }
    }

    matched.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then_with(|| left.1.cmp(&right.1))
            .then_with(|| left.2.cmp(right.2))
    });

    let mut seen = HashSet::new();
    let mut policy_ids = Vec::new();
    for (_, _, _, policy_id) in matched {
        if seen.insert(policy_id) {
            policy_ids.push(policy_id.to_string());
        }
    }
    policy_ids
}

fn attachment_matches_scope(attachment: &AiPolicyAttachment, requested: Option<&str>) -> bool {
    match attachment.scope {
        AiPolicyScope::Global => attachment
            .scope_id
            .as_deref()
            .is_none_or(|scope_id| scope_id.trim().is_empty()),
        _ => match requested {
            Some(scope_id) if !scope_id.trim().is_empty() => {
                attachment.scope_id.as_deref() == Some(scope_id)
            }
            _ => false,
        },
    }
}

fn scope_rank(scope: &AiPolicyScope) -> u8 {
    match scope {
        AiPolicyScope::VirtualKey => 0,
        AiPolicyScope::Consumer => 1,
        AiPolicyScope::ConsumerGroup => 2,
        AiPolicyScope::Target => 3,
        AiPolicyScope::Model => 4,
        AiPolicyScope::Provider => 5,
        AiPolicyScope::Global => 6,
    }
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}
