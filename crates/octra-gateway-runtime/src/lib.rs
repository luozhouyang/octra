//! Runtime snapshot management for the API gateway.
//!
//! The proxy hot path must not query SQLite per request. Configuration is
//! compiled once into an immutable snapshot and swapped atomically on reload.
//! In-flight requests keep the snapshot they started with.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Utc};
use octra_api_core::{ApiCredential, LoadBalanceAlgorithm, Route, Service, Target};
use octra_core::{Result, SnapshotCounts, SnapshotInfo};
use octra_storage::Storage;
use octra_storage::api_gateway::ApiRuntimeData;
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;

pub mod balancer;
pub mod matcher;

pub use balancer::TargetPool;
pub use matcher::{
    CompiledRoute, CompiledService, MatchInput, RouteMatch, RouteMatcher, ServiceLimits,
    strip_matched_prefix,
};

/// SHA-256 hex digest of a plaintext API key.
pub fn hash_api_key(key: &str) -> String {
    hex::encode(Sha256::digest(key.as_bytes()))
}

/// A credential as seen by the request hot path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialEntry {
    pub id: String,
    pub consumer_id: String,
    pub key_hash: String,
    pub enabled: bool,
    pub expires_at: Option<DateTime<Utc>>,
}

impl CredentialEntry {
    /// A credential is usable when enabled and permanent or not yet expired.
    pub fn is_active(&self, now: DateTime<Utc>) -> bool {
        self.enabled && self.expires_at.is_none_or(|expires| expires > now)
    }
}

/// Immutable compiled view of the API gateway configuration.
pub struct RuntimeSnapshot {
    pub info: SnapshotInfo,
    pub services: Vec<Service>,
    pub routes: Vec<Route>,
    pub matcher: RouteMatcher,
    pub credentials: Vec<CredentialEntry>,
    credential_index: HashMap<String, usize>,
    /// Upstreams per service id, in name-then-id order.
    service_upstreams: HashMap<String, Vec<String>>,
    target_pools: HashMap<String, Arc<TargetPool>>,
    /// Route ids granted by consumer-scoped policies.
    consumer_policy_routes: HashMap<String, Vec<String>>,
    /// Route ids granted by policies that apply to every consumer.
    global_policy_routes: Option<Vec<String>>,
}

impl std::fmt::Debug for RuntimeSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeSnapshot")
            .field("info", &self.info)
            .field("services", &self.services.len())
            .field("routes", &self.routes.len())
            .field("credentials", &self.credentials.len())
            .field("target_pools", &self.target_pools.len())
            .finish()
    }
}

impl RuntimeSnapshot {
    /// Compiles raw storage rows into a snapshot.
    pub fn compile(data: ApiRuntimeData) -> Self {
        let matcher = RouteMatcher::compile(&data.services, &data.routes);

        let mut credential_index = HashMap::new();
        let mut credentials = Vec::with_capacity(data.credentials.len());
        for credential in &data.credentials {
            if let Some(hash) = &credential.key_hash {
                credential_index.insert(hash.clone(), credentials.len());
                credentials.push(to_credential_entry(credential));
            }
        }

        let target_pools = compile_pools(&data);

        let mut consumer_policy_routes: HashMap<String, Vec<String>> = HashMap::new();
        let mut global_policy_routes: Option<Vec<String>> = None;
        for policy in &data.policies {
            let routes = allowed_routes(&policy.document);
            match &policy.consumer_id {
                Some(consumer_id) => consumer_policy_routes
                    .entry(consumer_id.clone())
                    .or_default()
                    .extend(routes),
                None => global_policy_routes
                    .get_or_insert_with(Vec::new)
                    .extend(routes),
            }
        }

        let mut service_upstreams: HashMap<String, Vec<String>> = HashMap::new();
        for upstream in &data.upstreams {
            service_upstreams
                .entry(upstream.service_id.clone())
                .or_default()
                .push(upstream.id.clone());
        }

        Self {
            info: SnapshotInfo {
                generated_at: Utc::now(),
                counts: SnapshotCounts {
                    services: data.counts.services,
                    routes: data.counts.routes,
                    upstreams: data.counts.upstreams,
                    targets: data.counts.targets,
                },
            },
            services: data.services,
            routes: data.routes,
            matcher,
            credentials,
            credential_index,
            service_upstreams,
            target_pools,
            consumer_policy_routes,
            global_policy_routes,
        }
    }

    /// Whether the consumer may reach the route. With no applicable policy the
    /// route is allowed; once a policy applies, only granted routes pass.
    pub fn permits_route(&self, consumer_id: Option<&str>, route_id: &str) -> bool {
        let mut restricted = false;
        if let Some(allowed) = &self.global_policy_routes {
            restricted = true;
            if route_listed(allowed, route_id) {
                return true;
            }
        }
        if let Some(consumer_id) = consumer_id
            && let Some(allowed) = self.consumer_policy_routes.get(consumer_id)
        {
            restricted = true;
            if route_listed(allowed, route_id) {
                return true;
            }
        }
        !restricted
    }

    /// Resolves a plaintext API key to an active credential.
    pub fn authenticate(&self, key: &str, now: DateTime<Utc>) -> Option<&CredentialEntry> {
        let hash = hash_api_key(key);
        let entry = self.credentials.get(*self.credential_index.get(&hash)?)?;
        entry.is_active(now).then_some(entry)
    }

    pub fn target_pool(&self, upstream_id: &str) -> Option<&Arc<TargetPool>> {
        self.target_pools.get(upstream_id)
    }

    /// The first upstream configured for a service.
    pub fn upstream_for_service(&self, service_id: &str) -> Option<&str> {
        self.service_upstreams
            .get(service_id)?
            .first()
            .map(String::as_str)
    }

    /// The first upstream pool for a service that has eligible targets.
    /// Falls back to the first configured pool so exhaustion is reported by
    /// the proxy as an unavailable upstream rather than a missing one.
    pub fn pool_for_service(&self, service_id: &str) -> Option<&Arc<TargetPool>> {
        let mut fallback = None;
        for upstream_id in self.service_upstreams.get(service_id)? {
            let Some(pool) = self.target_pools.get(upstream_id) else {
                continue;
            };
            if pool.has_eligible() {
                return Some(pool);
            }
            fallback.get_or_insert(pool);
        }
        fallback
    }

    pub fn pools(&self) -> impl Iterator<Item = (&str, &Arc<TargetPool>)> {
        self.target_pools
            .iter()
            .map(|(id, pool)| (id.as_str(), pool))
    }
}

fn allowed_routes(document: &serde_json::Value) -> Vec<String> {
    document
        .get("allowed_routes")
        .and_then(serde_json::Value::as_array)
        .map(|routes| {
            routes
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn route_listed(allowed: &[String], route_id: &str) -> bool {
    allowed
        .iter()
        .any(|route| route == "*" || route == route_id)
}

fn to_credential_entry(credential: &ApiCredential) -> CredentialEntry {
    CredentialEntry {
        id: credential.id.clone(),
        consumer_id: credential.consumer_id.clone(),
        key_hash: credential.key_hash.clone().unwrap_or_default(),
        enabled: credential.enabled,
        expires_at: credential.expires_at,
    }
}

fn compile_pools(data: &ApiRuntimeData) -> HashMap<String, Arc<TargetPool>> {
    let algorithms: HashMap<&str, LoadBalanceAlgorithm> = data
        .upstreams
        .iter()
        .map(|upstream| (upstream.id.as_str(), upstream.algorithm))
        .collect();

    let mut grouped: HashMap<&str, Vec<&Target>> = HashMap::new();
    for target in &data.targets {
        if algorithms.contains_key(target.upstream_id.as_str()) {
            grouped
                .entry(target.upstream_id.as_str())
                .or_default()
                .push(target);
        }
    }

    grouped
        .into_iter()
        .map(|(upstream_id, mut targets)| {
            targets.sort_by(|a, b| a.id.cmp(&b.id));
            let algorithm = algorithms[upstream_id];
            let seed = pool_seed(upstream_id);
            (
                upstream_id.to_string(),
                Arc::new(TargetPool::new(algorithm, &targets, seed)),
            )
        })
        .collect()
}

/// Per-upstream rotation offset so independent pools do not march in lockstep.
fn pool_seed(upstream_id: &str) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    let mut hasher = DefaultHasher::new();
    upstream_id.hash(&mut hasher);
    hasher.finish() % 64
}

/// Owns the current snapshot and reloads it from storage.
pub struct GatewayRuntime {
    storage: Arc<Storage>,
    snapshot: RwLock<Arc<RuntimeSnapshot>>,
    reload_generation: AtomicU64,
}

impl GatewayRuntime {
    pub async fn bootstrap(storage: Arc<Storage>) -> Result<Self> {
        let data = storage.load_api_runtime_data().await?;
        Ok(Self {
            storage,
            snapshot: RwLock::new(Arc::new(RuntimeSnapshot::compile(data))),
            reload_generation: AtomicU64::new(0),
        })
    }

    /// Loads a fresh snapshot and atomically replaces the current one.
    /// In-flight requests keep the snapshot they started with.
    pub async fn reload(&self) -> Result<SnapshotInfo> {
        let data = self.storage.load_api_runtime_data().await?;
        let next = Arc::new(RuntimeSnapshot::compile(data));
        let info = next.info.clone();

        let mut guard = self.snapshot.write().await;
        *guard = next;
        let generation = self.reload_generation.fetch_add(1, Ordering::Relaxed) + 1;

        tracing::info!(
            generation,
            services = info.counts.services,
            routes = info.counts.routes,
            "api gateway runtime snapshot reloaded"
        );

        Ok(info)
    }

    pub async fn snapshot(&self) -> Arc<RuntimeSnapshot> {
        self.snapshot.read().await.clone()
    }

    pub fn storage(&self) -> &Storage {
        &self.storage
    }

    pub fn generation(&self) -> u64 {
        self.reload_generation.load(Ordering::Relaxed)
    }
}

impl std::fmt::Debug for GatewayRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewayRuntime")
            .field("generation", &self.generation())
            .finish_non_exhaustive()
    }
}

/// Normalizes a request host: lowercased, without user info or port.
pub fn normalize_host(value: &str) -> String {
    let host = value.rsplit_once('@').map_or(value, |(_, host)| host);
    let host = match host.strip_prefix('[') {
        Some(rest) => rest.split_once(']').map_or("", |(inside, _)| inside),
        None => host.split(':').next().unwrap_or_default(),
    };
    host.trim().trim_end_matches('.').to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable_sha256_hex() {
        // SHA-256("hello")
        assert_eq!(
            hash_api_key("hello"),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        assert_eq!(hash_api_key("hello").len(), 64);
    }

    #[test]
    fn host_normalization() {
        assert_eq!(normalize_host("Example.COM:8080"), "example.com");
        assert_eq!(normalize_host("example.com"), "example.com");
        assert_eq!(normalize_host("[::1]:8080"), "::1");
        assert_eq!(normalize_host("user@host.test:1"), "host.test");
    }

    #[tokio::test]
    async fn reload_reflects_new_configuration() {
        let storage = Arc::new(Storage::connect_in_memory().await.unwrap());
        let runtime = GatewayRuntime::bootstrap(storage.clone()).await.unwrap();
        assert_eq!(runtime.snapshot().await.info.counts.services, 0);

        sqlx::query(
            "INSERT INTO services (id, name, protocol, enabled) VALUES ('svc-1', 'demo', 'http', 1)",
        )
        .execute(storage.pool())
        .await
        .unwrap();

        let info = runtime.reload().await.unwrap();
        assert_eq!(info.counts.services, 1);
        assert_eq!(runtime.generation(), 1);

        let snapshot = runtime.snapshot().await;
        assert_eq!(snapshot.services.len(), 1);
        assert_eq!(snapshot.services[0].name, "demo");
    }
}
