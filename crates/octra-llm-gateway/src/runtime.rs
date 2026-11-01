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

/// Configuration errors for one provider, reported by the control plane.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ProviderIssue {
    pub provider_id: String,
    pub message: String,
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
    pub fn build_with_pool(mut data: LlmRuntimeData, pool: &AccountPool) -> Self {
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
        for policy in &data.policies {
            match &policy.consumer_id {
                Some(consumer_id) => policy_consumers
                    .entry(consumer_id.clone())
                    .or_default()
                    .merge(&policy.document),
                None => policy_global.merge(&policy.document),
            }
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
        }
    }

    /// Whether the consumer may use the requested model. Global and
    /// consumer-scoped policies both apply; a model must pass every policy
    /// dimension that names models.
    pub fn permits_model(&self, consumer_id: Option<&str>, model: &str) -> bool {
        if !permits(&self.policy_global.models, &[model]) {
            return false;
        }
        consumer_id.is_none_or(|consumer_id| {
            self.policy_consumers
                .get(consumer_id)
                .is_none_or(|limits| permits(&limits.models, &[model]))
        })
    }

    /// Whether the consumer may route to a provider, matched by id or name.
    pub fn permits_provider(
        &self,
        consumer_id: Option<&str>,
        provider_id: &str,
        provider_name: &str,
    ) -> bool {
        let candidates = [provider_id, provider_name];
        if !permits(&self.policy_global.providers, &candidates) {
            return false;
        }
        consumer_id.is_none_or(|consumer_id| {
            self.policy_consumers
                .get(consumer_id)
                .is_none_or(|limits| permits(&limits.providers, &candidates))
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

    /// Parsed pricing document for a provider model, when one is configured.
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
        for model in &self.data.models {
            if !model.enabled {
                continue;
            }
            let Some(provider) = self.providers.get(&model.provider_id) else {
                continue;
            };
            let protocol = model
                .protocol
                .as_deref()
                .and_then(Protocol::parse)
                .unwrap_or(provider.protocol);
            listing
                .entry(model.model_id.clone())
                .or_insert_with(|| ModelListing {
                    id: model.model_id.clone(),
                    owned_by: provider.name.clone(),
                    protocol,
                    created: self.generated_at.timestamp(),
                    description: model
                        .display_name
                        .clone()
                        .unwrap_or_else(|| "Provider model".to_string()),
                });
        }
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
        let data = storage.llm_load_runtime_data().await?;
        persist_quota_resets(&storage, account_pool.sync(&data.accounts));
        let mut snapshot = RuntimeSnapshot::build_with_pool(data, &account_pool);
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
        let data = self.storage.llm_load_runtime_data().await?;
        persist_quota_resets(&self.storage, self.account_pool.sync(&data.accounts));
        let snapshot = Arc::new(RuntimeSnapshot::build_with_pool(data, &self.account_pool));
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

fn persist_quota_resets(storage: &Arc<Storage>, resets: Vec<crate::accounts::QuotaReset>) {
    if resets.is_empty() {
        return;
    }
    let storage = storage.clone();
    tokio::spawn(async move {
        for reset in resets {
            if let Err(error) = storage
                .llm_roll_account_quota(&reset.account_id, &reset.quota_reset_at)
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
}

impl LlmDataState {
    pub fn new(runtime: Arc<LlmRuntime>, storage: Arc<Storage>, config: Arc<OctraConfig>) -> Self {
        Self {
            runtime,
            storage,
            config,
            upstream: None,
        }
    }

    /// Overrides the upstream transport, for example to inject a test key.
    pub fn with_upstream(mut self, upstream: crate::upstream::Upstream) -> Self {
        self.upstream = Some(Arc::new(upstream));
        self
    }

    /// The LLM data plane router built from this state.
    pub fn into_router(self) -> axum::Router {
        crate::data_plane::router(self)
    }

    pub(crate) fn upstream(&self) -> Arc<crate::upstream::Upstream> {
        self.upstream
            .clone()
            .unwrap_or_else(|| Arc::new(crate::upstream::Upstream::new(&self.config)))
    }
}

#[derive(Clone)]
pub struct LlmControlState {
    pub runtime: Arc<LlmRuntime>,
    pub storage: Arc<Storage>,
}

impl LlmControlState {
    pub fn new(runtime: Arc<LlmRuntime>, storage: Arc<Storage>) -> Self {
        Self { runtime, storage }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use octra_storage::{LlmModelInput, LlmProviderInput, LlmRoutingTargetInput, LlmStorageError};

    async fn storage() -> Arc<Storage> {
        Arc::new(Storage::connect_in_memory().await.unwrap())
    }

    fn provider_input(name: &str, base_url: &str, enabled: bool) -> LlmProviderInput {
        LlmProviderInput {
            name: name.into(),
            kind: "openai".into(),
            protocol: "openai".into(),
            auth_type: "none".into(),
            base_url: Some(base_url.into()),
            secret_ref: None,
            priority: 0,
            config_json: "{}".into(),
            enabled,
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

        let provider = storage
            .llm_create_provider(&provider_input("local", "http://127.0.0.1:9/v1", true))
            .await
            .unwrap();
        storage
            .llm_create_model(&LlmModelInput {
                provider_id: provider.id.clone(),
                model_id: "m1".into(),
                display_name: Some("Model One".into()),
                protocol: None,
                context_window: None,
                max_output_tokens: None,
                capabilities: "{}".into(),
                pricing_json: "{}".into(),
                enabled: true,
            })
            .await
            .unwrap();
        let group = storage
            .llm_create_routing_group("g", "priority")
            .await
            .unwrap();
        storage
            .llm_create_routing_target(
                &group.id,
                &LlmRoutingTargetInput {
                    provider_id: provider.id.clone(),
                    model_id: "m1".into(),
                    account_id: None,
                    weight: 1,
                    priority: 0,
                    position: 0,
                },
            )
            .await
            .unwrap();
        storage
            .llm_create_route("octra-default", Some(&group.id))
            .await
            .unwrap();

        runtime.reload().await.unwrap();
        let snapshot = runtime.snapshot().await;
        let plan = snapshot.resolve("octra-default").expect("route");
        assert_eq!(plan.attempts(0).len(), 2);
        let listing = snapshot.model_listing();
        assert!(listing.iter().any(|model| model.id == "octra-default"));
        assert!(listing.iter().any(|model| model.id == "m1"));
    }

    #[tokio::test]
    async fn invalid_provider_config_is_reported_and_skipped() {
        let storage = storage().await;
        let bad = storage
            .llm_create_provider(&LlmProviderInput {
                protocol: "grpc".into(),
                ..provider_input("bad", "http://127.0.0.1:9/v1", true)
            })
            .await
            .unwrap();
        let snapshot = LlmRuntime::bootstrap(storage, Arc::new(OctraConfig::default()))
            .await
            .unwrap()
            .snapshot()
            .await;
        assert!(snapshot.provider(&bad.id).is_none());
        assert_eq!(snapshot.issues.len(), 1);
    }

    #[test]
    fn storage_errors_convert() {
        let error: octra_core::Error = LlmStorageError::Backend("x".to_string()).into_octra();
        assert!(matches!(error, octra_core::Error::Storage(_)));
    }
}
