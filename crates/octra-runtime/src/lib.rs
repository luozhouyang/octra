//! Runtime snapshot management for the API gateway.
//!
//! The proxy hot path never queries SQLite. Configuration is compiled once
//! into an immutable snapshot and swapped atomically on reload. In-flight
//! requests keep the snapshot they started with.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;
use octra_api_core::{
    Acl, BasicauthCredential, Consumer, JwtSecret, KeyauthCredential, Route, Service, Target,
    Upstream,
};
use octra_core::{Result, SnapshotCounts, SnapshotInfo};
use octra_storage::Storage;
use octra_storage::api::ApiRuntimeData;
use tokio::sync::RwLock;

pub mod balancer;
pub mod matcher;

pub use balancer::{
    Endpoint, HashConfig, HashInput, HealthConfig, HealthStore, RequestGuard, StickyCookie,
    TargetPool, algorithm_from_str,
};
pub use matcher::{
    CompiledRoute, CompiledService, MatchInput, RouteMatch, RouteMatcher, ServiceLimits,
    strip_matched_prefix,
};

/// SHA-256 hex digest of a plaintext API key.
pub fn hash_api_key(key: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(key.as_bytes()))
}

#[derive(Debug, Clone, Default)]
pub struct ConsumerRecord {
    pub id: String,
    pub username: Option<String>,
    pub custom_id: Option<String>,
    pub acl_groups: Vec<String>,
    pub group_ids: Vec<String>,
    pub groups: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct KeyauthRef {
    pub id: String,
    pub consumer_id: String,
    pub key: String,
}

#[derive(Debug, Clone)]
pub struct BasicAuthRef {
    pub id: String,
    pub consumer_id: String,
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone)]
pub struct JwtRef {
    pub id: String,
    pub consumer_id: String,
    pub key: String,
    pub secret: String,
    pub algorithm: String,
    pub rsa_public_key: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct AuthSnapshot {
    pub consumers: HashMap<String, ConsumerRecord>,
    pub consumer_by_username: HashMap<String, String>,
    pub consumer_by_custom_id: HashMap<String, String>,
    pub keyauth: HashMap<String, KeyauthRef>,
    pub basicauth: HashMap<String, BasicAuthRef>,
    pub jwt: HashMap<String, JwtRef>,
}

#[derive(Debug, Clone)]
pub enum ServiceEndpoint {
    Pool(Arc<TargetPool>),
    Direct(Arc<Endpoint>),
}

impl ServiceEndpoint {
    pub fn attempts(&self, hash: Option<&HashInput<'_>>) -> Vec<Arc<Endpoint>> {
        match self {
            Self::Pool(pool) => pool.ordered_attempts(hash),
            Self::Direct(endpoint) => vec![Arc::clone(endpoint)],
        }
    }

    pub fn has_eligible(&self) -> bool {
        match self {
            Self::Pool(pool) => pool.has_eligible(),
            Self::Direct(endpoint) => endpoint.eligible,
        }
    }

    /// Sticky cookie metadata for pooled endpoints; direct endpoints never
    /// participate in cookie hashing.
    pub fn sticky_cookie(&self) -> Option<StickyCookie> {
        match self {
            Self::Pool(pool) => pool.sticky_cookie(),
            Self::Direct(_) => None,
        }
    }

    /// Configured upstream `Host` header; pooled upstreams may override the
    /// target authority. Direct services have no upstream row.
    pub fn host_header(&self) -> Option<&str> {
        match self {
            Self::Pool(pool) => pool.host_header(),
            Self::Direct(_) => None,
        }
    }
}

/// Immutable compiled view of the API gateway configuration.
pub struct RuntimeSnapshot {
    pub info: SnapshotInfo,
    pub services: Vec<Service>,
    pub routes: Vec<Route>,
    pub matcher: RouteMatcher,
    endpoints: HashMap<String, ServiceEndpoint>,
    consumers: HashMap<String, ConsumerRecord>,
    consumer_by_username: HashMap<String, String>,
    consumer_by_custom_id: HashMap<String, String>,
    keyauth: HashMap<String, KeyauthRef>,
    basicauth: HashMap<String, BasicAuthRef>,
    jwt: HashMap<String, JwtRef>,
}

impl std::fmt::Debug for RuntimeSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeSnapshot")
            .field("info", &self.info)
            .field("services", &self.services.len())
            .field("routes", &self.routes.len())
            .field("endpoints", &self.endpoints.len())
            .field("consumers", &self.consumers.len())
            .finish()
    }
}

impl RuntimeSnapshot {
    pub fn compile(data: ApiRuntimeData) -> Self {
        Self::compile_with_health(data, &HealthStore::default())
    }

    pub fn compile_with_health(data: ApiRuntimeData, health_store: &HealthStore) -> Self {
        let matcher = RouteMatcher::compile(&data.services, &data.routes);
        let endpoints =
            compile_endpoints(&data.services, &data.upstreams, &data.targets, health_store);

        let acl_groups = group_acls(&data.acls);
        let consumer_groups =
            group_consumer_groups(&data.consumer_groups, &data.consumer_group_members);
        let mut consumers = HashMap::new();
        let mut consumer_by_username = HashMap::new();
        let mut consumer_by_custom_id = HashMap::new();
        for consumer in &data.consumers {
            let record = consumer_record(consumer, &acl_groups, &consumer_groups);
            if let Some(username) = &record.username {
                consumer_by_username.insert(username.clone(), record.id.clone());
            }
            if let Some(custom_id) = &record.custom_id {
                consumer_by_custom_id.insert(custom_id.clone(), record.id.clone());
            }
            consumers.insert(record.id.clone(), record);
        }

        let keyauth = data
            .keyauth
            .iter()
            .map(keyauth_ref)
            .map(|entry| (entry.key.clone(), entry))
            .collect();
        let basicauth = data
            .basicauth
            .iter()
            .map(basicauth_ref)
            .map(|entry| (entry.username.clone(), entry))
            .collect();
        let jwt = data
            .jwt
            .iter()
            .map(jwt_ref)
            .map(|entry| (entry.key.clone(), entry))
            .collect();

        Self {
            info: SnapshotInfo {
                generated_at: Utc::now(),
                counts: SnapshotCounts {
                    services: data.services.len() as i64,
                    routes: data.routes.len() as i64,
                    upstreams: data.upstreams.len() as i64,
                    targets: data.targets.len() as i64,
                },
            },
            services: data.services,
            routes: data.routes,
            matcher,
            endpoints,
            consumers,
            consumer_by_username,
            consumer_by_custom_id,
            keyauth,
            basicauth,
            jwt,
        }
    }

    pub fn auth_snapshot(&self) -> AuthSnapshot {
        AuthSnapshot {
            consumers: self.consumers.clone(),
            consumer_by_username: self.consumer_by_username.clone(),
            consumer_by_custom_id: self.consumer_by_custom_id.clone(),
            keyauth: self.keyauth.clone(),
            basicauth: self.basicauth.clone(),
            jwt: self.jwt.clone(),
        }
    }

    pub fn endpoint_for_service(&self, service_id: &str) -> Option<ServiceEndpoint> {
        self.endpoints.get(service_id).cloned()
    }

    /// Pools with active health checks enabled. Used by the background
    /// health-check loop.
    pub fn pools(&self) -> Vec<Arc<TargetPool>> {
        let mut pools: Vec<Arc<TargetPool>> = Vec::new();
        for endpoint in self.endpoints.values() {
            if let ServiceEndpoint::Pool(pool) = endpoint
                && !pools.iter().any(|existing| Arc::ptr_eq(existing, pool))
            {
                pools.push(Arc::clone(pool));
            }
        }
        pools
    }

    pub fn service(&self, service_id: &str) -> Option<&Service> {
        self.services
            .iter()
            .find(|service| service.id == service_id)
    }

    pub fn consumer(&self, consumer_id: &str) -> Option<&ConsumerRecord> {
        self.consumers.get(consumer_id)
    }

    pub fn consumer_id_by_username(&self, username: &str) -> Option<&str> {
        self.consumer_by_username.get(username).map(String::as_str)
    }

    pub fn consumer_id_by_custom_id(&self, custom_id: &str) -> Option<&str> {
        self.consumer_by_custom_id
            .get(custom_id)
            .map(String::as_str)
    }

    pub fn keyauth(&self, key: &str) -> Option<&KeyauthRef> {
        self.keyauth.get(key)
    }

    pub fn basicauth(&self, username: &str) -> Option<&BasicAuthRef> {
        self.basicauth.get(username)
    }

    pub fn jwt(&self, key: &str) -> Option<&JwtRef> {
        self.jwt.get(key)
    }

    pub fn acl_groups(&self, consumer_id: &str) -> &[String] {
        self.consumers
            .get(consumer_id)
            .map(|consumer| consumer.acl_groups.as_slice())
            .unwrap_or(&[])
    }
}

fn compile_endpoints(
    services: &[Service],
    upstreams: &[Upstream],
    targets: &[Target],
    health_store: &HealthStore,
) -> HashMap<String, ServiceEndpoint> {
    let mut targets_by_upstream: HashMap<&str, Vec<&Target>> = HashMap::new();
    for target in targets {
        targets_by_upstream
            .entry(target.upstream_id.as_str())
            .or_default()
            .push(target);
    }

    let mut upstreams_by_name: HashMap<&str, &Upstream> = HashMap::new();
    for upstream in upstreams {
        upstreams_by_name.insert(upstream.name.as_str(), upstream);
    }

    let mut endpoints = HashMap::new();
    for service in services {
        if !service.enabled || octra_api_core::is_internal_llm_service(service) {
            continue;
        }
        let scheme = if service.protocol.eq_ignore_ascii_case("https") {
            "https"
        } else {
            "http"
        };
        let endpoint = if let Some(upstream) = upstreams_by_name.get(service.host.as_str()) {
            let pool_targets: Vec<Target> = targets_by_upstream
                .get(upstream.id.as_str())
                .map(|targets| {
                    targets
                        .iter()
                        .filter(|target| target.weight > 0)
                        .map(|target| (*target).clone())
                        .collect()
                })
                .unwrap_or_default();
            if pool_targets.is_empty() {
                continue;
            }
            ServiceEndpoint::Pool(Arc::new(TargetPool::from_upstream(
                upstream,
                &pool_targets,
                scheme,
                pool_seed(&service.id),
                health_store,
            )))
        } else {
            let url = format!("{}://{}:{}", scheme, service.host, service.port);
            ServiceEndpoint::Direct(Endpoint::direct(service.id.clone(), url))
        };
        endpoints.insert(service.id.clone(), endpoint);
    }
    endpoints
}

fn consumer_record(
    consumer: &Consumer,
    acl_groups: &HashMap<String, Vec<String>>,
    consumer_groups: &HashMap<String, Vec<(String, String)>>,
) -> ConsumerRecord {
    let groups = consumer_groups
        .get(&consumer.id)
        .cloned()
        .unwrap_or_default();
    ConsumerRecord {
        id: consumer.id.clone(),
        username: consumer.username.clone(),
        custom_id: consumer.custom_id.clone(),
        acl_groups: acl_groups.get(&consumer.id).cloned().unwrap_or_default(),
        group_ids: groups.iter().map(|(id, _)| id.clone()).collect(),
        groups: groups.iter().map(|(_, name)| name.clone()).collect(),
    }
}

fn group_consumer_groups(
    groups: &[octra_api_core::ConsumerGroup],
    members: &[octra_api_core::ConsumerGroupMember],
) -> HashMap<String, Vec<(String, String)>> {
    let names: HashMap<&str, &str> = groups
        .iter()
        .map(|group| (group.id.as_str(), group.name.as_str()))
        .collect();
    let mut memberships: HashMap<String, Vec<(String, String)>> = HashMap::new();
    for member in members {
        let Some(name) = names.get(member.consumer_group_id.as_str()) else {
            continue;
        };
        memberships
            .entry(member.consumer_id.clone())
            .or_default()
            .push((member.consumer_group_id.clone(), (*name).to_string()));
    }
    for groups in memberships.values_mut() {
        groups.sort_by(|left, right| left.1.cmp(&right.1).then_with(|| left.0.cmp(&right.0)));
    }
    memberships
}

fn group_acls(acls: &[Acl]) -> HashMap<String, Vec<String>> {
    let mut groups: HashMap<String, Vec<String>> = HashMap::new();
    for acl in acls {
        groups
            .entry(acl.consumer_id.clone())
            .or_default()
            .push(acl.group.clone());
    }
    groups
}

fn keyauth_ref(credential: &KeyauthCredential) -> KeyauthRef {
    KeyauthRef {
        id: credential.id.clone(),
        consumer_id: credential.consumer_id.clone(),
        key: credential.key.clone(),
    }
}

fn basicauth_ref(credential: &BasicauthCredential) -> BasicAuthRef {
    BasicAuthRef {
        id: credential.id.clone(),
        consumer_id: credential.consumer_id.clone(),
        username: credential.username.clone(),
        password: credential.password.clone(),
    }
}

fn jwt_ref(secret: &JwtSecret) -> JwtRef {
    JwtRef {
        id: secret.id.clone(),
        consumer_id: secret.consumer_id.clone(),
        key: secret.key.clone(),
        secret: secret.secret.clone(),
        algorithm: secret.algorithm.clone(),
        rsa_public_key: secret.rsa_public_key.clone(),
    }
}

fn pool_seed(service_id: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    service_id.hash(&mut hasher);
    hasher.finish() % 64
}

/// Owns the current snapshot and reloads it from storage.
pub struct GatewayRuntime {
    storage: Arc<Storage>,
    snapshot: RwLock<Arc<RuntimeSnapshot>>,
    health: HealthStore,
}

impl GatewayRuntime {
    pub async fn bootstrap(storage: Arc<Storage>) -> Result<Self> {
        let data = storage.api.load_runtime_data().await?;
        let health = HealthStore::default();
        Ok(Self {
            storage,
            snapshot: RwLock::new(Arc::new(RuntimeSnapshot::compile_with_health(
                data, &health,
            ))),
            health,
        })
    }

    pub async fn reload(&self) -> Result<SnapshotInfo> {
        let data = self.storage.api.load_runtime_data().await?;
        let next = Arc::new(RuntimeSnapshot::compile_with_health(data, &self.health));
        let info = next.info.clone();
        *self.snapshot.write().await = next;
        Ok(info)
    }

    pub async fn snapshot(&self) -> Arc<RuntimeSnapshot> {
        self.snapshot.read().await.clone()
    }

    pub fn storage(&self) -> &Storage {
        &self.storage
    }

    /// Runs one round of active health checks over the current snapshot.
    /// Targets that are not due are skipped, so the caller may tick faster
    /// than the configured intervals.
    pub async fn check_health(&self) {
        let snapshot = self.snapshot().await;
        for pool in snapshot.pools() {
            if pool.active_health_enabled() {
                pool.check_active().await;
            }
        }
    }
}

impl std::fmt::Debug for GatewayRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewayRuntime").finish_non_exhaustive()
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
    fn host_normalization() {
        assert_eq!(normalize_host("Example.COM:8080"), "example.com");
        assert_eq!(normalize_host("[::1]:8080"), "::1");
        assert_eq!(normalize_host("user@host.test:1"), "host.test");
    }

    #[test]
    fn snapshot_compiles_direct_and_upstream_endpoints() {
        let direct = Service {
            id: "svc-direct".into(),
            name: "direct".into(),
            protocol: "http".into(),
            host: "127.0.0.1".into(),
            port: 9000,
            path: None,
            retries: 0,
            connect_timeout: 1000,
            write_timeout: 1000,
            read_timeout: 1000,
            enabled: true,
            description: String::new(),
            tags: Vec::new(),
            created_at: 0,
            updated_at: 0,
        };
        let pooled = Service {
            id: "svc-pool".into(),
            host: "up".into(),
            ..direct.clone()
        };
        let data = ApiRuntimeData {
            services: vec![direct, pooled],
            upstreams: vec![Upstream {
                id: "u".into(),
                name: "up".into(),
                algorithm: octra_api_core::LoadBalanceAlgorithm::RoundRobin,
                hash_on: "none".into(),
                hash_fallback: "none".into(),
                hash_on_header: "X-Consumer-ID".into(),
                hash_fallback_header: "X-Consumer-ID".into(),
                hash_on_cookie: "kong".into(),
                hash_on_cookie_path: "/".into(),
                hash_on_query_arg: String::new(),
                hash_fallback_query_arg: String::new(),
                hash_on_uri_capture: String::new(),
                hash_fallback_uri_capture: String::new(),
                slots: 10000,
                healthchecks: serde_json::json!({}),
                host_header: None,
                tags: Vec::new(),
                created_at: 0,
                updated_at: 0,
            }],
            targets: vec![Target {
                id: "t".into(),
                upstream_id: "u".into(),
                target: "127.0.0.2:9001".into(),
                weight: 100,
                tags: Vec::new(),
                created_at: 0,
                updated_at: 0,
            }],
            ..Default::default()
        };
        let snapshot = RuntimeSnapshot::compile(data);
        assert!(matches!(
            snapshot.endpoint_for_service("svc-direct"),
            Some(ServiceEndpoint::Direct(_))
        ));
        assert!(matches!(
            snapshot.endpoint_for_service("svc-pool"),
            Some(ServiceEndpoint::Pool(_))
        ));
    }

    #[test]
    fn service_endpoint_exposes_sticky_cookie_for_pools_only() {
        let direct = Service {
            id: "svc-direct".into(),
            name: "direct".into(),
            protocol: "http".into(),
            host: "127.0.0.1".into(),
            port: 9000,
            path: None,
            retries: 0,
            connect_timeout: 1000,
            write_timeout: 1000,
            read_timeout: 1000,
            enabled: true,
            description: String::new(),
            tags: Vec::new(),
            created_at: 0,
            updated_at: 0,
        };
        let pooled = Service {
            id: "svc-pool".into(),
            host: "up".into(),
            ..direct.clone()
        };
        let data = ApiRuntimeData {
            services: vec![direct, pooled],
            upstreams: vec![Upstream {
                id: "u".into(),
                name: "up".into(),
                algorithm: octra_api_core::LoadBalanceAlgorithm::ConsistentHashing,
                hash_on: "cookie".into(),
                hash_fallback: "none".into(),
                hash_on_header: "X-Consumer-ID".into(),
                hash_fallback_header: "X-Consumer-ID".into(),
                hash_on_cookie: "session".into(),
                hash_on_cookie_path: "/api".into(),
                hash_on_query_arg: String::new(),
                hash_fallback_query_arg: String::new(),
                hash_on_uri_capture: String::new(),
                hash_fallback_uri_capture: String::new(),
                slots: 10000,
                healthchecks: serde_json::json!({}),
                host_header: None,
                tags: Vec::new(),
                created_at: 0,
                updated_at: 0,
            }],
            targets: vec![Target {
                id: "t".into(),
                upstream_id: "u".into(),
                target: "127.0.0.2:9001".into(),
                weight: 100,
                tags: Vec::new(),
                created_at: 0,
                updated_at: 0,
            }],
            ..Default::default()
        };
        let snapshot = RuntimeSnapshot::compile(data);
        let direct = snapshot
            .endpoint_for_service("svc-direct")
            .expect("direct endpoint");
        assert!(direct.sticky_cookie().is_none());
        let pooled = snapshot
            .endpoint_for_service("svc-pool")
            .expect("pooled endpoint");
        assert_eq!(
            pooled.sticky_cookie(),
            Some(StickyCookie {
                name: "session".into(),
                path: "/api".into(),
            })
        );
    }

    #[test]
    fn upstream_host_header_reaches_the_pool() {
        let upstream = Upstream {
            id: "u".into(),
            name: "u".into(),
            algorithm: Default::default(),
            hash_on: "none".into(),
            hash_fallback: "none".into(),
            hash_on_header: String::new(),
            hash_fallback_header: String::new(),
            hash_on_cookie: String::new(),
            hash_on_cookie_path: String::new(),
            hash_on_query_arg: String::new(),
            hash_fallback_query_arg: String::new(),
            hash_on_uri_capture: String::new(),
            hash_fallback_uri_capture: String::new(),
            slots: 10000,
            healthchecks: serde_json::json!({}),
            host_header: Some("api.internal".into()),
            tags: Vec::new(),
            created_at: 0,
            updated_at: 0,
        };
        let targets = vec![Target {
            id: "t".into(),
            upstream_id: "u".into(),
            target: "127.0.0.2:9001".into(),
            weight: 100,
            tags: Vec::new(),
            created_at: 0,
            updated_at: 0,
        }];
        let pool =
            TargetPool::from_upstream(&upstream, &targets, "http", 0, &HealthStore::default());
        assert_eq!(pool.host_header(), Some("api.internal"));
    }

    #[test]
    fn snapshot_compiles_consumer_group_membership_sorted_by_name() {
        let data = ApiRuntimeData {
            consumers: vec![Consumer {
                id: "c".into(),
                username: Some("alice".into()),
                custom_id: None,
                tags: Vec::new(),
                created_at: 0,
                updated_at: 0,
            }],
            consumer_groups: vec![
                octra_api_core::ConsumerGroup {
                    id: "g2".into(),
                    name: "operators".into(),
                    tags: Vec::new(),
                    created_at: 0,
                    updated_at: 0,
                },
                octra_api_core::ConsumerGroup {
                    id: "g1".into(),
                    name: "admins".into(),
                    tags: Vec::new(),
                    created_at: 0,
                    updated_at: 0,
                },
            ],
            consumer_group_members: vec![
                octra_api_core::ConsumerGroupMember {
                    consumer_id: "c".into(),
                    consumer_group_id: "g2".into(),
                    created_at: 0,
                },
                octra_api_core::ConsumerGroupMember {
                    consumer_id: "c".into(),
                    consumer_group_id: "g1".into(),
                    created_at: 0,
                },
            ],
            ..Default::default()
        };
        let snapshot = RuntimeSnapshot::compile(data);
        let record = snapshot.consumer("c").unwrap();
        assert_eq!(
            record.groups,
            vec!["admins".to_string(), "operators".into()]
        );
        assert_eq!(record.group_ids, vec!["g1".to_string(), "g2".into()]);
    }
}
