//! Immutable routing snapshot shared by the data plane and control plane.

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use octra_config::OctraConfig;
use octra_core::Result;
use octra_llm_core::Protocol;
use octra_storage::{LlmRuntimeData, Storage};
use tokio::sync::RwLock;

use crate::accounts::{AccountPool, AccountRuntime};
use crate::config::ProviderConfig;
use crate::routing::{RoundRobin, RoutePlan};
use crate::vk_policy::VirtualKeyPolicy;

/// Configuration errors for one provider, reported by the control plane.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ProviderIssue {
    pub provider_id: String,
    pub message: String,
}

/// Model capability flags used by intent-aware routing.
#[derive(Debug, Clone, Copy, Default)]
pub struct ModelProfile {
    pub context_window: Option<u64>,
    pub vision: Option<bool>,
    pub tools: Option<bool>,
    pub reasoning: Option<bool>,
}

/// Request identifiers used to evaluate scoped allowlists. Fields that are
/// not known yet are left empty; the corresponding scope is then skipped.
#[derive(Debug, Clone, Default)]
pub struct PolicyContext<'a> {
    pub consumer_id: Option<&'a str>,
    pub consumer_group_ids: &'a [String],
    pub virtual_key_id: Option<&'a str>,
    pub model: &'a str,
    pub provider_id: Option<&'a str>,
    pub provider_name: Option<&'a str>,
    pub target_id: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub struct RuntimeSnapshot {
    pub generated_at: DateTime<Utc>,
    pub data: LlmRuntimeData,
    providers: BTreeMap<String, Arc<ProviderConfig>>,
    routes: BTreeMap<String, Arc<RoutePlan>>,
    accounts_by_provider: BTreeMap<String, Vec<Arc<AccountRuntime>>>,
    round_robin: RoundRobin,
    pub issues: Vec<ProviderIssue>,
    policy_global: PolicyLimits,
    policy_consumers: BTreeMap<String, PolicyLimits>,
    policy_consumer_groups: BTreeMap<String, PolicyLimits>,
    policy_virtual_keys: BTreeMap<String, PolicyLimits>,
    policy_providers: BTreeMap<String, PolicyLimits>,
    policy_models: BTreeMap<String, PolicyLimits>,
    policy_targets: BTreeMap<String, PolicyLimits>,
    virtual_keys: BTreeMap<String, VirtualKeyPolicy>,
    consumer_groups: BTreeMap<String, Vec<String>>,
}

/// Model and provider allowances granted by applicable policies. A dimension
/// is unrestricted until some applicable policy names it.
#[derive(Debug, Clone, Default)]
struct PolicyLimits {
    models: Option<Vec<String>>,
    providers: Option<Vec<String>>,
}

impl PolicyLimits {
    fn merge(&mut self, document: &serde_json::Value) {
        extend(&mut self.models, document, "allowed_models");
        extend(&mut self.providers, document, "allowed_providers");
    }
}

fn extend(slot: &mut Option<Vec<String>>, document: &serde_json::Value, key: &str) {
    let Some(values) = document.get(key).and_then(serde_json::Value::as_array) else {
        return;
    };
    let values = values
        .iter()
        .filter_map(serde_json::Value::as_str)
        .map(str::to_string);
    slot.get_or_insert_with(Vec::new).extend(values);
}

fn permits(allowed: &Option<Vec<String>>, candidates: &[&str]) -> bool {
    match allowed {
        None => true,
        Some(allowed) => allowed
            .iter()
            .any(|value| value == "*" || candidates.contains(&value.as_str())),
    }
}

impl RuntimeSnapshot {
    pub fn build(data: LlmRuntimeData) -> Self {
        let pool = AccountPool::new();
        pool.sync(&data.accounts);
        Self::build_with_pool(data, &pool)
    }

    /// Builds a snapshot from data whose accounts are already in `pool`.
    pub fn build_with_pool(data: LlmRuntimeData, pool: &AccountPool) -> Self {
        Self::build_with_pool_and_groups(data, pool, BTreeMap::new())
    }

    /// Builds a snapshot with consumer-group memberships loaded from the API
    /// store. Group data is kept out of `LlmRuntimeData` because it belongs to
    /// the client-identity plane, not LLM routing.
    pub fn build_with_pool_and_groups(
        mut data: LlmRuntimeData,
        pool: &AccountPool,
        consumer_groups: BTreeMap<String, Vec<String>>,
    ) -> Self {
        data.providers.sort_by(|a, b| {
            a.priority
                .cmp(&b.priority)
                .then_with(|| a.name.cmp(&b.name))
        });
        let mut providers = BTreeMap::new();
        let mut issues = Vec::new();
        for record in &data.providers {
            if !record.enabled {
                continue;
            }
            match ProviderConfig::from_record(record) {
                Ok(config) => {
                    providers.insert(record.id.clone(), Arc::new(config));
                }
                Err(error) => {
                    let crate::config::ProviderConfigError::Invalid(message) = error;
                    issues.push(ProviderIssue {
                        provider_id: record.id.clone(),
                        message,
                    });
                }
            }
        }

        let mut policy_global = PolicyLimits::default();
        let mut policy_consumers: BTreeMap<String, PolicyLimits> = BTreeMap::new();
        let mut policy_consumer_groups: BTreeMap<String, PolicyLimits> = BTreeMap::new();
        let mut policy_virtual_keys: BTreeMap<String, PolicyLimits> = BTreeMap::new();
        let mut policy_providers: BTreeMap<String, PolicyLimits> = BTreeMap::new();
        let mut policy_models: BTreeMap<String, PolicyLimits> = BTreeMap::new();
        let mut policy_targets: BTreeMap<String, PolicyLimits> = BTreeMap::new();
        for policy in &data.policies {
            let scope = policy.scope.as_str();
            let target = match (scope, policy.consumer_id.as_deref()) {
                ("consumer", Some(id)) => Some((&mut policy_consumers, id)),
                ("consumer_group", Some(id)) => Some((&mut policy_consumer_groups, id)),
                ("virtual_key", Some(id)) => Some((&mut policy_virtual_keys, id)),
                ("provider", Some(id)) => Some((&mut policy_providers, id)),
                ("model", Some(id)) => Some((&mut policy_models, id)),
                ("target", Some(id)) => Some((&mut policy_targets, id)),
                _ => None,
            };
            match target {
                Some((map, id)) => map
                    .entry(id.to_string())
                    .or_default()
                    .merge(&policy.document),
                None => policy_global.merge(&policy.document),
            }
        }

        let mut virtual_keys = BTreeMap::new();
        for record in &data.virtual_keys {
            let policy = match VirtualKeyPolicy::parse(&record.permissions_json) {
                Ok(policy) => policy,
                Err(error) => {
                    tracing::warn!(
                        virtual_key_id = %record.id,
                        %error,
                        "invalid virtual key permissions; denying all requests"
                    );
                    VirtualKeyPolicy::deny_all()
                }
            };
            virtual_keys.insert(record.id.clone(), policy);
        }

        let mut accounts_by_provider: BTreeMap<String, Vec<Arc<AccountRuntime>>> = BTreeMap::new();
        for record in &data.accounts {
            let Some(entry) = pool.get(&record.id) else {
                tracing::warn!(
                    account_id = %record.id,
                    "provider account is missing from the account pool"
                );
                continue;
            };
            accounts_by_provider
                .entry(record.provider_id.clone())
                .or_default()
                .push(entry);
        }

        let mut routes = BTreeMap::new();
        for route in &data.routes {
            if let Ok(plan) = RoutePlan::compile(&data, route, &providers) {
                routes.insert(route.virtual_model_id.clone(), Arc::new(plan));
            }
        }

        Self {
            generated_at: Utc::now(),
            data,
            providers,
            routes,
            accounts_by_provider,
            round_robin: RoundRobin::new(),
            issues,
            policy_global,
            policy_consumers,
            policy_consumer_groups,
            policy_virtual_keys,
            policy_providers,
            policy_models,
            policy_targets,
            virtual_keys,
            consumer_groups,
        }
    }

    /// Consumer groups the given consumer belongs to.
    pub fn consumer_group_ids(&self, consumer_id: &str) -> &[String] {
        self.consumer_groups
            .get(consumer_id)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Whether the consumer may use the requested model, using every scope
    /// whose identifiers are known at this point in the request.
    pub fn permits_model(&self, consumer_id: Option<&str>, model: &str) -> bool {
        self.permits_model_in(&PolicyContext {
            consumer_id,
            model,
            ..PolicyContext::default()
        })
    }

    /// Model permission with consumer-group, virtual-key, provider and model
    /// scopes applied when present.
    pub fn permits_model_in(&self, context: &PolicyContext<'_>) -> bool {
        if !permits(&self.policy_global.models, &[context.model]) {
            return false;
        }
        if let Some(consumer_id) = context.consumer_id
            && !self
                .policy_consumers
                .get(consumer_id)
                .is_none_or(|limits| permits(&limits.models, &[context.model]))
        {
            return false;
        }
        for group_id in context.consumer_group_ids {
            if !self
                .policy_consumer_groups
                .get(group_id)
                .is_none_or(|limits| permits(&limits.models, &[context.model]))
            {
                return false;
            }
        }
        if let Some(key_id) = context.virtual_key_id
            && !self
                .policy_virtual_keys
                .get(key_id)
                .is_none_or(|limits| permits(&limits.models, &[context.model]))
        {
            return false;
        }
        if !self
            .policy_models
            .get(context.model)
            .is_none_or(|limits| permits(&limits.models, &[context.model]))
        {
            return false;
        }
        if let Some(provider_id) = context.provider_id
            && !self
                .policy_providers
                .get(provider_id)
                .is_none_or(|limits| permits(&limits.models, &[context.model]))
        {
            return false;
        }
        if let Some(target_id) = context.target_id
            && !self
                .policy_targets
                .get(target_id)
                .is_none_or(|limits| permits(&limits.models, &[context.model]))
        {
            return false;
        }
        true
    }

    /// Whether the consumer may route to a provider, matched by id or name.
    pub fn permits_provider(
        &self,
        consumer_id: Option<&str>,
        provider_id: &str,
        provider_name: &str,
    ) -> bool {
        self.permits_provider_in(&PolicyContext {
            consumer_id,
            provider_id: Some(provider_id),
            provider_name: Some(provider_name),
            ..PolicyContext::default()
        })
    }

    /// Provider permission with consumer-group, virtual-key, model and
    /// provider scopes applied when present.
    pub fn permits_provider_in(&self, context: &PolicyContext<'_>) -> bool {
        let provider_id = context.provider_id.unwrap_or_default();
        let provider_name = context.provider_name.unwrap_or_default();
        let candidates = [provider_id, provider_name];
        if !permits(&self.policy_global.providers, &candidates) {
            return false;
        }
        if let Some(consumer_id) = context.consumer_id
            && !self
                .policy_consumers
                .get(consumer_id)
                .is_none_or(|limits| permits(&limits.providers, &candidates))
        {
            return false;
        }
        for group_id in context.consumer_group_ids {
            if !self
                .policy_consumer_groups
                .get(group_id)
                .is_none_or(|limits| permits(&limits.providers, &candidates))
            {
                return false;
            }
        }
        if let Some(key_id) = context.virtual_key_id
            && !self
                .policy_virtual_keys
                .get(key_id)
                .is_none_or(|limits| permits(&limits.providers, &candidates))
        {
            return false;
        }
        if !context.model.is_empty()
            && !self
                .policy_models
                .get(context.model)
                .is_none_or(|limits| permits(&limits.providers, &candidates))
        {
            return false;
        }
        if let Some(id) = context.provider_id
            && !self.policy_providers.get(id).is_none_or(|limits| {
                permits(&limits.providers, &candidates)
                    && (context.model.is_empty() || permits(&limits.models, &[context.model]))
            })
        {
            return false;
        }
        if let Some(target_id) = context.target_id
            && !self.policy_targets.get(target_id).is_none_or(|limits| {
                permits(&limits.providers, &candidates)
                    && (context.model.is_empty() || permits(&limits.models, &[context.model]))
            })
        {
            return false;
        }
        true
    }

    pub fn virtual_key_policy(&self, id: &str) -> Option<&VirtualKeyPolicy> {
        self.virtual_keys.get(id)
    }

    /// Whether the virtual key may use a model. Anonymous requests are not
    /// narrowed; a key with malformed permissions denies everything.
    pub fn virtual_key_permits_model(&self, virtual_key_id: Option<&str>, model: &str) -> bool {
        virtual_key_id.is_none_or(|id| {
            self.virtual_keys
                .get(id)
                .is_none_or(|policy| policy.permits_model(model))
        })
    }

    pub fn virtual_key_permits_provider(
        &self,
        virtual_key_id: Option<&str>,
        provider_id: &str,
        provider_name: &str,
    ) -> bool {
        virtual_key_id.is_none_or(|id| {
            self.virtual_keys
                .get(id)
                .is_none_or(|policy| policy.permits_provider(provider_id, provider_name))
        })
    }

    pub fn provider(&self, id: &str) -> Option<Arc<ProviderConfig>> {
        self.providers.get(id).cloned()
    }

    /// Accounts configured for a provider, with live quota/cooldown state.
    pub fn accounts_for(&self, provider_id: &str) -> &[Arc<AccountRuntime>] {
        self.accounts_by_provider
            .get(provider_id)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Declared capabilities of a provider model, used for intent ranking.
    /// Missing declarations stay `None` so they never penalize a target.
    pub fn model_profile(&self, provider_id: &str, model_id: &str) -> ModelProfile {
        let Some(model) = self
            .data
            .models
            .iter()
            .find(|model| model.provider_id == provider_id && model.model_id == model_id)
        else {
            return ModelProfile::default();
        };
        let capabilities: serde_json::Value =
            serde_json::from_str(&model.capabilities).unwrap_or(serde_json::Value::Null);
        let flag = |key: &str| capabilities.get(key).and_then(serde_json::Value::as_bool);
        ModelProfile {
            context_window: model
                .context_window
                .and_then(|value| u64::try_from(value).ok()),
            vision: flag("vision"),
            tools: flag("tools"),
            reasoning: flag("reasoning"),
        }
    }

    /// Parsed pricing document for a provider model, when one is configured.
    /// Worst-case input/output price across a route's targets. Used to reserve
    /// a pessimistic cost budget before the target is selected.
    pub fn pricing_ceiling(&self, model: &str) -> (f64, f64) {
        let Some(plan) = self.routes.get(model) else {
            return (0.0, 0.0);
        };
        plan.targets()
            .filter_map(|target| self.model_pricing(&target.provider_id, &target.model_id))
            .fold((0.0, 0.0), |(input, output), pricing| {
                (
                    input.max(
                        pricing
                            .get("input_per_million")
                            .and_then(serde_json::Value::as_f64)
                            .unwrap_or(0.0),
                    ),
                    output.max(
                        pricing
                            .get("output_per_million")
                            .and_then(serde_json::Value::as_f64)
                            .unwrap_or(0.0),
                    ),
                )
            })
    }

    pub fn model_pricing(&self, provider_id: &str, model_id: &str) -> Option<serde_json::Value> {
        let model = self
            .data
            .models
            .iter()
            .find(|model| model.provider_id == provider_id && model.model_id == model_id)?;
        let value: serde_json::Value = serde_json::from_str(&model.pricing_json).ok()?;
        if value.as_object().is_some_and(|object| !object.is_empty()) {
            Some(value)
        } else {
            None
        }
    }

    pub fn enabled_providers(&self) -> impl Iterator<Item = &Arc<ProviderConfig>> {
        self.providers.values()
    }

    /// Resolves a requested model name to its routing plan.
    pub fn resolve(&self, virtual_model_id: &str) -> Option<Arc<RoutePlan>> {
        self.routes.get(virtual_model_id).cloned()
    }

    pub fn round_robin(&self) -> RoundRobin {
        self.round_robin.clone()
    }

    /// Models exposed by `GET /v1/models`: route names plus provider models.
    pub fn model_listing(&self) -> Vec<ModelListing> {
        let mut listing: BTreeMap<String, ModelListing> = BTreeMap::new();
        for route in self.routes.keys() {
            listing.insert(
                route.clone(),
                ModelListing {
                    id: route.clone(),
                    owned_by: "octra".to_string(),
                    protocol: Protocol::Openai,
                    created: self.generated_at.timestamp(),
                    description: "Octra virtual model".to_string(),
                },
            );
        }
        // Only names that resolve to a routing plan are advertised. Raw
        // provider model ids are not directly routable and would produce
        // `model_not_found` if a client selected one from this list.
        listing.into_values().collect()
    }
}

#[derive(Debug, Clone)]
pub struct ModelListing {
    pub id: String,
    pub owned_by: String,
    pub protocol: Protocol,
    pub created: i64,
    pub description: String,
}

pub struct LlmRuntime {
    pub storage: Arc<Storage>,
    pub config: Arc<OctraConfig>,
    account_pool: AccountPool,
    snapshot: RwLock<Arc<RuntimeSnapshot>>,
}

impl LlmRuntime {
    pub async fn bootstrap(storage: Arc<Storage>, config: Arc<OctraConfig>) -> Result<Self> {
        let account_pool = AccountPool::new();
        let data = load_runtime_data(&storage).await?;
        persist_quota_resets(&storage, account_pool.sync(&data.accounts));
        let consumer_groups = load_consumer_groups(&storage).await?;
        let mut snapshot =
            RuntimeSnapshot::build_with_pool_and_groups(data, &account_pool, consumer_groups);
        for issue in &snapshot.issues {
            tracing::warn!(
                provider_id = %issue.provider_id,
                message = %issue.message,
                "llm provider configuration ignored"
            );
        }
        snapshot.issues.shrink_to_fit();
        Ok(Self {
            storage,
            config,
            account_pool,
            snapshot: RwLock::new(Arc::new(snapshot)),
        })
    }

    /// Reloads configuration and atomically swaps the snapshot. In-flight
    /// requests keep the snapshot they started with.
    pub async fn reload(&self) -> Result<()> {
        let data = load_runtime_data(&self.storage).await?;
        persist_quota_resets(&self.storage, self.account_pool.sync(&data.accounts));
        let consumer_groups = load_consumer_groups(&self.storage).await?;
        let snapshot = Arc::new(RuntimeSnapshot::build_with_pool_and_groups(
            data,
            &self.account_pool,
            consumer_groups,
        ));
        let mut guard = self.snapshot.write().await;
        *guard = snapshot;
        Ok(())
    }

    pub async fn snapshot(&self) -> Arc<RuntimeSnapshot> {
        self.snapshot.read().await.clone()
    }

    /// Live account state, for explicit quota resets from the control plane.
    pub fn accounts(&self) -> &AccountPool {
        &self.account_pool
    }
}

/// Loads the routing data from the AI-native control plane.
///
/// The compiler maps `ai_*` entities onto the internal `LlmRuntimeData`
/// snapshot. Virtual keys remain in `virtual_keys` and are passed through.
/// Any failure leaves the caller's previous snapshot intact.
async fn load_runtime_data(storage: &Arc<Storage>) -> Result<LlmRuntimeData> {
    let data = storage.ai.load_control_plane().await?;
    let virtual_keys = storage
        .llm
        .list_virtual_keys()
        .await
        .map_err(|error| error.into_octra())?;
    crate::ai_runtime::runtime_data_from_ai(&data, virtual_keys)
}

async fn load_consumer_groups(storage: &Storage) -> Result<BTreeMap<String, Vec<String>>> {
    let memberships = storage.api.list_consumer_group_memberships().await?;
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (consumer_id, group_id) in memberships {
        groups.entry(consumer_id).or_default().push(group_id);
    }
    Ok(groups)
}

fn persist_quota_resets(storage: &Arc<Storage>, resets: Vec<crate::accounts::QuotaReset>) {
    if resets.is_empty() {
        return;
    }
    let storage = storage.clone();
    tokio::spawn(async move {
        for reset in resets {
            if let Err(error) = storage
                .ai
                .roll_account_quota(&reset.account_id, &reset.quota_reset_at)
                .await
            {
                tracing::warn!(
                    account_id = %reset.account_id,
                    error = ?error,
                    "cannot persist quota period rollover"
                );
            }
        }
    });
}

#[derive(Clone)]
pub struct LlmDataState {
    pub runtime: Arc<LlmRuntime>,
    pub storage: Arc<Storage>,
    pub config: Arc<OctraConfig>,
    pub upstream: Option<Arc<crate::upstream::Upstream>>,
    pub plugins: Option<Arc<crate::LlmPluginRuntime>>,
    pub oauth: Arc<crate::credentials::OAuthTokenCache>,
    pub semantic_embedding: Option<Arc<crate::embedding::EmbeddingRuntime>>,
    pub(crate) vk_reservations: Arc<crate::vk_reservation::VkReservationLedger>,
}

impl LlmDataState {
    pub fn new(runtime: Arc<LlmRuntime>, storage: Arc<Storage>, config: Arc<OctraConfig>) -> Self {
        let semantic_embedding = match crate::embedding::EmbeddingRuntime::from_config(
            config.llm.semantic_embedding.as_ref(),
        ) {
            Ok(embedding) => embedding,
            Err(error) => {
                tracing::warn!(%error, "semantic embedding runtime disabled");
                None
            }
        };
        Self {
            runtime,
            storage: storage.clone(),
            config,
            upstream: None,
            plugins: None,
            oauth: Arc::new(crate::credentials::OAuthTokenCache::new(storage)),
            semantic_embedding,
            vk_reservations: crate::vk_reservation::VkReservationLedger::new(),
        }
    }

    /// Overrides the upstream transport, for example to inject a test key.
    pub fn with_upstream(mut self, upstream: crate::upstream::Upstream) -> Self {
        self.upstream = Some(Arc::new(upstream));
        self
    }

    pub fn with_plugins(mut self, plugins: Arc<crate::LlmPluginRuntime>) -> Self {
        self.plugins = Some(plugins);
        self
    }

    /// The LLM data plane router built from this state.
    pub fn into_router(self) -> axum::Router {
        crate::data_plane::router(self)
    }

    pub(crate) fn upstream(&self) -> Arc<crate::upstream::Upstream> {
        self.upstream.clone().unwrap_or_else(|| {
            Arc::new(crate::upstream::Upstream::new(&self.config).with_oauth(self.oauth.clone()))
        })
    }
}

#[derive(Clone)]
pub struct LlmControlState {
    pub runtime: Arc<LlmRuntime>,
    pub storage: Arc<Storage>,
    pub plugins: Option<Arc<crate::LlmPluginRuntime>>,
    pub pii: Option<Arc<crate::pii::PiiEngineRuntime>>,
    pub oauth: Arc<crate::credentials::OAuthTokenCache>,
    pub oauth_flows: Arc<crate::credentials::OAuthFlowStore>,
}

impl LlmControlState {
    pub fn new(runtime: Arc<LlmRuntime>, storage: Arc<Storage>) -> Self {
        Self {
            runtime,
            storage: storage.clone(),
            plugins: None,
            pii: None,
            oauth: Arc::new(crate::credentials::OAuthTokenCache::new(storage)),
            oauth_flows: Arc::new(crate::credentials::OAuthFlowStore::new()),
        }
    }

    pub fn with_plugins(mut self, plugins: Arc<crate::LlmPluginRuntime>) -> Self {
        self.plugins = Some(plugins);
        self
    }

    pub fn with_pii_engines(mut self, pii: Arc<crate::pii::PiiEngineRuntime>) -> Self {
        self.pii = Some(pii);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use octra_ai_config::{AiBalancer, AiModel, AiModelKind, AiModelTarget, AiProvider};
    use octra_storage::LlmStorageError;
    use serde_json::json;

    async fn storage() -> Arc<Storage> {
        Arc::new(Storage::connect_in_memory().await.unwrap())
    }

    fn ai_provider(id: &str, protocol: &str) -> AiProvider {
        AiProvider {
            id: id.into(),
            name: id.into(),
            protocol: protocol.into(),
            base_url: "http://127.0.0.1:9/v1".into(),
            auth_ref: None,
            capabilities: vec!["chat".into()],
            config: json!({}),
            enabled: true,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn ai_model(id: &str, name: &str) -> AiModel {
        AiModel {
            id: id.into(),
            name: name.into(),
            kind: AiModelKind::Model,
            capabilities: vec!["chat".into()],
            formats: vec!["openai".into()],
            route_paths: Vec::new(),
            balancer: AiBalancer::default(),
            enabled: true,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn ai_target(
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

    #[tokio::test]
    async fn bootstrap_builds_and_reload_swaps_routes() {
        let storage = storage().await;
        let config = Arc::new(OctraConfig::default());
        let runtime = LlmRuntime::bootstrap(storage.clone(), config.clone())
            .await
            .unwrap();
        assert!(runtime.snapshot().await.resolve("octra-default").is_none());

        storage
            .ai
            .upsert_provider(&ai_provider("provider-1", "openai"))
            .await
            .unwrap();
        storage
            .ai
            .upsert_model(&ai_model("model-1", "octra-default"))
            .await
            .unwrap();
        storage
            .ai
            .upsert_target(&ai_target("target-1", "model-1", "provider-1", "m1"))
            .await
            .unwrap();

        runtime.reload().await.unwrap();
        let snapshot = runtime.snapshot().await;
        let plan = snapshot.resolve("octra-default").expect("route");
        assert_eq!(plan.attempts(0).len(), 2);
        let listing = snapshot.model_listing();
        assert!(listing.iter().any(|model| model.id == "octra-default"));
        assert!(
            !listing.iter().any(|model| model.id == "m1"),
            "provider model ids are not routable and must not be advertised"
        );
    }

    #[tokio::test]
    async fn invalid_provider_config_is_reported_and_skipped() {
        let storage = storage().await;
        storage
            .ai
            .upsert_provider(&ai_provider("bad", "grpc"))
            .await
            .unwrap();
        let snapshot = LlmRuntime::bootstrap(storage, Arc::new(OctraConfig::default()))
            .await
            .unwrap()
            .snapshot()
            .await;
        assert!(snapshot.provider("bad").is_none());
        assert_eq!(snapshot.issues.len(), 1);
    }

    #[test]
    fn storage_errors_convert() {
        let error: octra_core::Error = LlmStorageError::Backend("x".to_string()).into_octra();
        assert!(matches!(error, octra_core::Error::Storage(_)));
    }
}
