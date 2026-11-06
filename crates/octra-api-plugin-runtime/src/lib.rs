//! Plugin registry, scope resolution, and phase execution.
//!
//! Chains are compiled from configured plugin instances once per control-plane
//! mutation. The data plane only performs map lookups and in-memory calls.
//! Consumer-scoped plugins are selected in a second pass after authentication.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use axum::response::Response;
use bytes::Bytes;
use octra_api_core::PluginInstance;
use octra_api_plugin_types::{
    ApiGatewayPlugin, ApiGatewayPluginFactory, BodyContext, Capability, EntityQuery, ErrorContext,
    EventSubscriber, FailPolicy, GatewayEvent, JobInvocation, LogContext, LogLevel, PluginAction,
    PluginError, PluginHost, PluginManifest, PluginPhase, PluginServiceSource, PluginStorage,
    RequestBodyContext, RequestContext, ResponseBodyContext, ResponseContext, ScheduledJob,
    StorageError, UpstreamRequestContext,
};
use octra_core::Result;
use parking_lot::{Mutex, RwLock};
use serde_json::Value;

#[derive(Default)]
pub struct PluginRegistry {
    factories: HashMap<String, Arc<dyn ApiGatewayPluginFactory>>,
}

impl PluginRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register<F>(&mut self, factory: F)
    where
        F: ApiGatewayPluginFactory + 'static,
    {
        self.register_arc(Arc::new(factory));
    }

    pub fn register_arc(&mut self, factory: Arc<dyn ApiGatewayPluginFactory>) {
        let name = factory.manifest().name;
        self.factories.insert(name, factory);
    }

    pub fn manifests(&self) -> Vec<PluginManifest> {
        let mut manifests: Vec<_> = self
            .factories
            .values()
            .map(|factory| factory.manifest())
            .collect();
        manifests.sort_by(|left, right| left.name.cmp(&right.name));
        manifests
    }

    pub fn factory(&self, name: &str) -> Option<Arc<dyn ApiGatewayPluginFactory>> {
        self.factories.get(name).cloned()
    }

    pub fn validate(&self, name: &str, config: &Value) -> std::result::Result<(), PluginError> {
        self.factory(name)
            .ok_or_else(|| PluginError::fail_closed(format!("unknown plugin '{name}'")))?
            .validate(config)
    }
}

pub use octra_api_plugin_types::{PluginFactoryResolver, PluginHostProvider};

impl PluginFactoryResolver for PluginRegistry {
    fn factory(&self, name: &str) -> Option<Arc<dyn ApiGatewayPluginFactory>> {
        PluginRegistry::factory(self, name)
    }

    fn manifests(&self) -> Vec<PluginManifest> {
        PluginRegistry::manifests(self)
    }
}

/// Consults the built-in registry first, then a dynamic catalog (installed
/// WASM plugins). Built-in names always win so a third-party artifact cannot
/// shadow a trusted plugin.
pub struct CompositePluginFactoryResolver {
    builtins: Arc<dyn PluginFactoryResolver>,
    dynamic: Arc<dyn PluginFactoryResolver>,
}

impl CompositePluginFactoryResolver {
    pub fn new(
        builtins: Arc<dyn PluginFactoryResolver>,
        dynamic: Arc<dyn PluginFactoryResolver>,
    ) -> Self {
        Self { builtins, dynamic }
    }
}

impl PluginFactoryResolver for CompositePluginFactoryResolver {
    fn factory(&self, name: &str) -> Option<Arc<dyn ApiGatewayPluginFactory>> {
        self.builtins
            .factory(name)
            .or_else(|| self.dynamic.factory(name))
    }

    fn manifests(&self) -> Vec<PluginManifest> {
        let mut seen = HashSet::new();
        let mut manifests = Vec::new();
        for manifest in self
            .builtins
            .manifests()
            .into_iter()
            .chain(self.dynamic.manifests())
        {
            if seen.insert(manifest.name.clone()) {
                manifests.push(manifest);
            }
        }
        manifests.sort_by(|left, right| left.name.cmp(&right.name));
        manifests
    }
}

/// Used by built-in-only runtimes; built-in factories ignore the host.
#[derive(Default)]
pub struct NoopPluginHostProvider;

impl PluginHostProvider for NoopPluginHostProvider {
    fn host_for(&self, instance: &PluginInstance) -> Arc<dyn PluginHost> {
        Arc::new(NoopPluginHost {
            plugin_name: instance.name.clone(),
            instance_id: instance.id.clone(),
        })
    }
}

struct NoopPluginHost {
    plugin_name: String,
    instance_id: String,
}

static NOOP_PLUGIN_STORAGE: NoopPluginStorage = NoopPluginStorage;

impl PluginHost for NoopPluginHost {
    fn plugin_name(&self) -> &str {
        &self.plugin_name
    }

    fn plugin_instance_id(&self) -> &str {
        &self.instance_id
    }

    fn capabilities(&self) -> &[Capability] {
        &[]
    }

    fn storage(&self) -> &dyn PluginStorage {
        &NOOP_PLUGIN_STORAGE
    }

    fn log(&self, level: LogLevel, message: &str) {
        match level {
            LogLevel::Trace => tracing::trace!(plugin = %self.plugin_name, "{message}"),
            LogLevel::Debug => tracing::debug!(plugin = %self.plugin_name, "{message}"),
            LogLevel::Info => tracing::info!(plugin = %self.plugin_name, "{message}"),
            LogLevel::Warn => tracing::warn!(plugin = %self.plugin_name, "{message}"),
            LogLevel::Error => tracing::error!(plugin = %self.plugin_name, "{message}"),
        }
    }
}

struct NoopPluginStorage;

impl PluginStorage for NoopPluginStorage {
    fn kv_get(&self, _key: &str) -> std::result::Result<Option<Vec<u8>>, StorageError> {
        Err(StorageError::Unavailable)
    }

    fn kv_put(
        &self,
        _key: &str,
        _value: &[u8],
        _ttl: Option<Duration>,
    ) -> std::result::Result<(), StorageError> {
        Err(StorageError::Unavailable)
    }

    fn kv_delete(&self, _key: &str) -> std::result::Result<bool, StorageError> {
        Err(StorageError::Unavailable)
    }

    fn kv_incr(
        &self,
        _key: &str,
        _delta: i64,
        _ttl: Option<Duration>,
    ) -> std::result::Result<i64, StorageError> {
        Err(StorageError::Unavailable)
    }

    fn get(&self, _entity: &str, _key: &Value) -> std::result::Result<Option<Value>, StorageError> {
        Err(StorageError::Unavailable)
    }

    fn upsert(&self, _entity: &str, _values: &Value) -> std::result::Result<(), StorageError> {
        Err(StorageError::Unavailable)
    }

    fn increment(
        &self,
        _entity: &str,
        _key: &Value,
        _field: &str,
        _delta: i64,
        _extra: Option<&Value>,
    ) -> std::result::Result<i64, StorageError> {
        Err(StorageError::Unavailable)
    }

    fn delete(&self, _entity: &str, _key: &Value) -> std::result::Result<bool, StorageError> {
        Err(StorageError::Unavailable)
    }

    fn query(
        &self,
        _entity: &str,
        _query: &EntityQuery,
    ) -> std::result::Result<Vec<Value>, StorageError> {
        Err(StorageError::Unavailable)
    }
}

#[derive(Clone)]
struct PluginEntry {
    pub id: String,
    name: String,
    instance_name: Option<String>,
    priority: i32,
    timeout: Duration,
    phases: Vec<PluginPhase>,
    capabilities: Vec<Capability>,
    protocols: Vec<String>,
    auth: bool,
    ordering: Option<Value>,
    service_id: Option<String>,
    route_id: Option<String>,
    consumer_id: Option<String>,
    consumer_group_id: Option<String>,
    plugin: Arc<dyn ApiGatewayPlugin>,
}

#[derive(Clone, Copy)]
struct ConsumerGroupRef<'a> {
    id: &'a str,
    name: &'a str,
}

impl PluginEntry {
    fn has_post_auth_scope(&self) -> bool {
        self.consumer_id.is_some() || self.consumer_group_id.is_some()
    }

    fn supports_protocol(&self, protocol: &str) -> bool {
        self.protocols
            .iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(protocol))
    }

    fn route_service_matches(&self, route_id: Option<&str>, service_id: Option<&str>) -> bool {
        self.route_id
            .as_deref()
            .is_none_or(|expected| route_id == Some(expected))
            && self
                .service_id
                .as_deref()
                .is_none_or(|expected| service_id == Some(expected))
    }

    fn applies(
        &self,
        route_id: Option<&str>,
        service_id: Option<&str>,
        consumer_id: Option<&str>,
        groups: &[ConsumerGroupRef<'_>],
    ) -> Option<i32> {
        if let Some(expected) = &self.route_id
            && route_id != Some(expected.as_str())
        {
            return None;
        }
        if let Some(expected) = &self.service_id
            && service_id != Some(expected.as_str())
        {
            return None;
        }
        if let Some(expected) = &self.consumer_id
            && consumer_id != Some(expected.as_str())
        {
            return None;
        }
        if let Some(expected) = &self.consumer_group_id
            && !groups.iter().any(|group| group.id == expected)
        {
            return None;
        }
        let has_r = self.route_id.is_some();
        let has_s = self.service_id.is_some();
        let has_c = self.consumer_id.is_some();
        let has_g = self.consumer_group_id.is_some();

        Some(match (has_c, has_g, has_r, has_s) {
            (true, false, true, true) => 12,
            (false, true, true, true) => 11,
            (true, false, true, false) => 10,
            (true, false, false, true) => 9,
            (false, true, true, false) => 8,
            (false, true, false, true) => 7,
            (false, false, true, true) => 6,
            (true, false, false, false) => 5,
            (false, true, false, false) => 4,
            (false, false, true, false) => 3,
            (false, false, false, true) => 2,
            (false, false, false, false) => 1,
            _ => return None,
        })
    }
}

#[derive(Default)]
pub struct PluginChain {
    entries: Vec<PluginEntry>,
}

impl PluginChain {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|entry| entry.name.as_str())
    }

    pub fn has_phase(&self, phase: PluginPhase) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.phases.contains(&phase))
    }

    pub fn has_capability(&self, capability: Capability) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.capabilities.contains(&capability))
    }

    pub fn has_auth(&self) -> bool {
        self.entries.iter().any(|entry| entry.auth)
    }

    pub async fn on_rewrite(
        &self,
        cx: &mut RequestContext<'_>,
    ) -> std::result::Result<Option<Response>, PluginError> {
        for entry in &self.entries {
            if !entry.phases.contains(&PluginPhase::Rewrite) {
                continue;
            }
            let action =
                run_stage(entry, PluginPhase::Rewrite, entry.plugin.on_rewrite(cx)).await?;
            if let PluginAction::ShortCircuit(response) = action {
                return Ok(Some(response));
            }
        }
        Ok(None)
    }

    pub async fn on_request(
        &self,
        cx: &mut RequestContext<'_>,
    ) -> std::result::Result<Option<Response>, PluginError> {
        for entry in &self.entries {
            if !entry.phases.contains(&PluginPhase::Request) {
                continue;
            }
            let action = if entry.auth {
                run_auth_stage(entry, entry.plugin.on_request(cx)).await?
            } else {
                run_stage(entry, PluginPhase::Request, entry.plugin.on_request(cx)).await?
            };
            if let PluginAction::ShortCircuit(response) = action {
                return Ok(Some(response));
            }
        }
        Ok(None)
    }

    pub async fn on_upstream_request(
        &self,
        cx: &mut UpstreamRequestContext<'_>,
    ) -> std::result::Result<Option<Response>, PluginError> {
        for entry in &self.entries {
            if !entry.phases.contains(&PluginPhase::UpstreamRequest) {
                continue;
            }
            let action = run_stage(
                entry,
                PluginPhase::UpstreamRequest,
                entry.plugin.on_upstream_request(cx),
            )
            .await?;
            if let PluginAction::ShortCircuit(response) = action {
                return Ok(Some(response));
            }
        }
        Ok(None)
    }

    pub async fn on_response(
        &self,
        cx: &mut ResponseContext<'_>,
    ) -> std::result::Result<Option<Response>, PluginError> {
        for entry in &self.entries {
            if !entry.phases.contains(&PluginPhase::Response) {
                continue;
            }
            let action =
                run_stage(entry, PluginPhase::Response, entry.plugin.on_response(cx)).await?;
            if let PluginAction::ShortCircuit(response) = action {
                return Ok(Some(response));
            }
        }
        Ok(None)
    }

    pub async fn on_response_body(
        &self,
        cx: &mut BodyContext<'_>,
        mut chunk: Bytes,
    ) -> std::result::Result<Bytes, PluginError> {
        for entry in &self.entries {
            if !entry.phases.contains(&PluginPhase::ResponseBody) {
                continue;
            }
            let result = tokio::time::timeout(
                entry.timeout,
                entry.plugin.on_response_body(cx, chunk.clone()),
            )
            .await;
            match result {
                Ok(Ok(next)) => chunk = next,
                Ok(Err(error)) => match error.fail_policy {
                    FailPolicy::Open => {
                        tracing::warn!(plugin = %entry.name, error = %error, "plugin body filter failed")
                    }
                    FailPolicy::Closed => return Err(error),
                },
                Err(_) => {
                    tracing::warn!(plugin = %entry.name, "plugin body filter timed out");
                }
            }
        }
        Ok(chunk)
    }

    pub async fn on_request_buffer(
        &self,
        cx: &mut RequestBodyContext<'_>,
        mut body: Bytes,
    ) -> std::result::Result<Bytes, PluginError> {
        for entry in &self.entries {
            if !entry.phases.contains(&PluginPhase::Request) {
                continue;
            }
            let fallback = body.clone();
            body =
                run_bytes_stage(entry, entry.plugin.on_request_buffer(cx, body), fallback).await?;
        }
        Ok(body)
    }

    pub async fn on_response_buffer(
        &self,
        cx: &mut ResponseBodyContext<'_>,
        mut body: Bytes,
    ) -> std::result::Result<Bytes, PluginError> {
        for entry in &self.entries {
            if !entry.phases.contains(&PluginPhase::ResponseBody) {
                continue;
            }
            let fallback = body.clone();
            body =
                run_bytes_stage(entry, entry.plugin.on_response_buffer(cx, body), fallback).await?;
        }
        Ok(body)
    }

    pub async fn on_error(
        &self,
        cx: &mut ErrorContext<'_>,
    ) -> std::result::Result<Option<Response>, PluginError> {
        for entry in &self.entries {
            if !entry.phases.contains(&PluginPhase::Error) {
                continue;
            }
            let action = run_stage(entry, PluginPhase::Error, entry.plugin.on_error(cx)).await?;
            if let PluginAction::ShortCircuit(response) = action {
                return Ok(Some(response));
            }
        }
        Ok(None)
    }

    pub async fn on_log(&self, cx: &mut LogContext<'_>) {
        for entry in &self.entries {
            if !entry.phases.contains(&PluginPhase::Log) {
                continue;
            }
            if tokio::time::timeout(entry.timeout, entry.plugin.on_log(cx))
                .await
                .is_err()
            {
                tracing::warn!(plugin = %entry.name, "plugin log hook timed out");
            }
        }
    }
}

async fn run_stage<F>(
    entry: &PluginEntry,
    _phase: PluginPhase,
    future: F,
) -> std::result::Result<PluginAction, PluginError>
where
    F: std::future::Future<Output = std::result::Result<PluginAction, PluginError>>,
{
    let result = tokio::time::timeout(entry.timeout, future).await;
    match result {
        Ok(Ok(action)) => Ok(action),
        Ok(Err(error)) => match error.fail_policy {
            FailPolicy::Open => {
                tracing::warn!(plugin = %entry.name, error = %error, "plugin failed; continuing");
                Ok(PluginAction::Continue)
            }
            FailPolicy::Closed => Err(error),
        },
        Err(_) => {
            let error = PluginError::fail_closed(format!("plugin '{}' timed out", entry.name));
            tracing::warn!(plugin = %entry.name, "plugin timed out");
            Err(error)
        }
    }
}

/// Auth plugins run in the request (access) phase like Kong, but their
/// failures never fail open: continuing unauthenticated would silently
/// bypass the plugin.
async fn run_auth_stage<F>(
    entry: &PluginEntry,
    future: F,
) -> std::result::Result<PluginAction, PluginError>
where
    F: std::future::Future<Output = std::result::Result<PluginAction, PluginError>>,
{
    match tokio::time::timeout(entry.timeout, future).await {
        Ok(Ok(action)) => Ok(action),
        Ok(Err(error)) => {
            tracing::warn!(plugin = %entry.name, error = %error, "auth plugin failed");
            Err(PluginError::fail_closed(error.message))
        }
        Err(_) => {
            tracing::warn!(plugin = %entry.name, "auth plugin timed out");
            Err(PluginError::fail_closed(format!(
                "plugin '{}' timed out",
                entry.name
            )))
        }
    }
}

async fn run_bytes_stage<F>(
    entry: &PluginEntry,
    future: F,
    fallback: Bytes,
) -> std::result::Result<Bytes, PluginError>
where
    F: std::future::Future<Output = std::result::Result<Bytes, PluginError>>,
{
    let result = tokio::time::timeout(entry.timeout, future).await;
    match result {
        Ok(Ok(body)) => Ok(body),
        Ok(Err(error)) => match error.fail_policy {
            FailPolicy::Open => {
                tracing::warn!(plugin = %entry.name, error = %error, "plugin body stage failed");
                Ok(fallback)
            }
            FailPolicy::Closed => Err(error),
        },
        Err(_) => {
            let error = PluginError::fail_closed(format!("plugin '{}' timed out", entry.name));
            tracing::warn!(plugin = %entry.name, "plugin body stage timed out");
            Err(error)
        }
    }
}

#[derive(Default)]
struct PluginChains {
    entries: Vec<PluginEntry>,
    subscribers: Vec<Arc<PluginSubscriber>>,
    jobs: Vec<Arc<PluginJob>>,
    pre_cache: Mutex<HashMap<(String, String, String), Arc<PluginChain>>>,
    post_cache: Mutex<HashMap<String, Arc<PluginChain>>>,
    empty: Arc<PluginChain>,
    has_any: bool,
}

struct PluginSubscriber {
    id: String,
    plugin_name: String,
    instance_id: String,
    topics: Vec<String>,
    plugin: Arc<dyn ApiGatewayPlugin>,
}

#[async_trait::async_trait]
impl EventSubscriber for PluginSubscriber {
    fn subscriber_id(&self) -> &str {
        &self.id
    }

    fn plugin_name(&self) -> &str {
        &self.plugin_name
    }

    fn instance_id(&self) -> &str {
        &self.instance_id
    }

    fn topics(&self) -> &[String] {
        &self.topics
    }

    async fn deliver(&self, event: &GatewayEvent) -> std::result::Result<(), PluginError> {
        self.plugin.on_event(event).await
    }
}

struct PluginJob {
    id: String,
    plugin_name: String,
    instance_id: String,
    name: String,
    schedule: String,
    timeout: Duration,
    plugin: Arc<dyn ApiGatewayPlugin>,
}

#[async_trait::async_trait]
impl ScheduledJob for PluginJob {
    fn job_id(&self) -> &str {
        &self.id
    }

    fn plugin_name(&self) -> &str {
        &self.plugin_name
    }

    fn instance_id(&self) -> &str {
        &self.instance_id
    }

    fn job_name(&self) -> &str {
        &self.name
    }

    fn schedule(&self) -> &str {
        &self.schedule
    }

    fn timeout(&self) -> Duration {
        self.timeout
    }

    async fn run(&self, job: &JobInvocation) -> std::result::Result<(), PluginError> {
        self.plugin.on_job(job).await
    }
}

impl PluginChains {
    fn compile(
        plugins: Vec<PluginInstance>,
        resolver: &dyn PluginFactoryResolver,
        host_provider: &dyn PluginHostProvider,
    ) -> Result<Self> {
        let mut entries = Vec::new();
        let mut subscribers = Vec::new();
        let mut jobs = Vec::new();
        for plugin in plugins {
            if !plugin.enabled {
                continue;
            }
            let Some(factory) = resolver.factory(&plugin.name) else {
                tracing::warn!(plugin = %plugin.name, "plugin is not registered; skipping config");
                continue;
            };
            let host = host_provider.host_for(&plugin);
            let instance = match factory.create_with_host(host, &plugin.id, plugin.config.clone()) {
                Ok(instance) => instance,
                Err(error) => {
                    tracing::warn!(plugin = %plugin.name, %error, "invalid plugin config; skipping");
                    continue;
                }
            };
            let manifest = instance.manifest();

            if !manifest.subscriptions.is_empty() {
                let topics = manifest
                    .subscriptions
                    .iter()
                    .map(|subscription| subscription.topic.clone())
                    .collect();
                subscribers.push(Arc::new(PluginSubscriber {
                    id: format!("{}#{}", plugin.name, plugin.id),
                    plugin_name: plugin.name.clone(),
                    instance_id: plugin.id.clone(),
                    topics,
                    plugin: instance.clone(),
                }));
            }

            for job in &manifest.jobs {
                jobs.push(Arc::new(PluginJob {
                    id: format!("{}#{}#{}", plugin.name, plugin.id, job.name),
                    plugin_name: plugin.name.clone(),
                    instance_id: plugin.id.clone(),
                    name: job.name.clone(),
                    schedule: job.schedule.clone(),
                    timeout: Duration::from_millis(
                        job.timeout_ms.unwrap_or(manifest.timeout_ms).max(1),
                    ),
                    plugin: instance.clone(),
                }));
            }

            entries.push(PluginEntry {
                id: plugin.id.clone(),
                name: plugin.name.clone(),
                instance_name: plugin.instance_name.clone(),
                priority: manifest.priority,
                timeout: manifest.timeout(),
                phases: factory.phases(),
                capabilities: manifest.capabilities.clone(),
                protocols: if plugin.protocols.is_empty() {
                    default_protocols()
                } else {
                    plugin.protocols.clone()
                },
                auth: manifest.capabilities.contains(&Capability::Auth),
                ordering: plugin.ordering.clone(),
                service_id: plugin.service_id.clone(),
                route_id: plugin.route_id.clone(),
                consumer_id: plugin.consumer_id.clone(),
                consumer_group_id: plugin.consumer_group_id.clone(),
                plugin: instance,
            });
        }

        entries.sort_by(|left, right| {
            right
                .priority
                .cmp(&left.priority)
                .then_with(|| left.name.cmp(&right.name))
        });

        Ok(Self {
            has_any: !entries.is_empty(),
            entries,
            subscribers,
            jobs,
            pre_cache: Mutex::new(HashMap::new()),
            post_cache: Mutex::new(HashMap::new()),
            empty: Arc::new(PluginChain::default()),
        })
    }

    fn pre_auth(
        &self,
        route_id: Option<&str>,
        service_id: Option<&str>,
        protocol: &str,
    ) -> Arc<PluginChain> {
        if !self.has_any {
            return self.empty.clone();
        }
        let key = (
            route_id.unwrap_or_default().to_string(),
            service_id.unwrap_or_default().to_string(),
            protocol.to_ascii_lowercase(),
        );
        if let Some(chain) = self.pre_cache.lock().get(&key) {
            return chain.clone();
        }
        let chain = Arc::new(PluginChain {
            entries: self.pre_auth_entries(route_id, service_id, protocol),
        });
        self.pre_cache.lock().insert(key, chain.clone());
        chain
    }

    fn pre_auth_entries(
        &self,
        route_id: Option<&str>,
        service_id: Option<&str>,
        protocol: &str,
    ) -> Vec<PluginEntry> {
        let deferred = self.deferred_names(route_id, service_id, protocol);
        let mut selected = resolve(
            &self.entries,
            route_id,
            service_id,
            None,
            &[],
            &HashSet::new(),
            protocol,
        );
        selected.retain(|entry| !deferred.contains(entry.name.as_str()));
        selected
    }

    /// Plugin names with a consumer or consumer-group scoped instance for this
    /// route/service are resolved after authentication instead, so that the
    /// scoped instance can replace a less specific one as Kong does.
    fn deferred_names(
        &self,
        route_id: Option<&str>,
        service_id: Option<&str>,
        protocol: &str,
    ) -> HashSet<&str> {
        self.entries
            .iter()
            .filter(|entry| {
                !entry.auth
                    && entry.has_post_auth_scope()
                    && entry.route_service_matches(route_id, service_id)
                    && entry.supports_protocol(protocol)
            })
            .map(|entry| entry.name.as_str())
            .collect()
    }

    fn post_auth(
        &self,
        route_id: Option<&str>,
        service_id: Option<&str>,
        consumer_id: Option<&str>,
        group_ids: &[String],
        group_names: &[String],
        protocol: &str,
    ) -> Arc<PluginChain> {
        if !self.has_any {
            return self.empty.clone();
        }
        let groups: Vec<ConsumerGroupRef<'_>> = group_ids
            .iter()
            .enumerate()
            .map(|(index, id)| ConsumerGroupRef {
                id,
                name: group_names.get(index).map(String::as_str).unwrap_or(id),
            })
            .collect();
        let key = post_cache_key(route_id, service_id, consumer_id, &groups, protocol);
        if let Some(chain) = self.post_cache.lock().get(&key) {
            return chain.clone();
        }

        let mut seen = HashSet::new();
        for entry in self.pre_auth_entries(route_id, service_id, protocol) {
            seen.insert((entry.name, entry.instance_name.unwrap_or_default()));
        }

        let entries = resolve(
            &self.entries,
            route_id,
            service_id,
            consumer_id,
            &groups,
            &seen,
            protocol,
        )
        .into_iter()
        .filter(|entry| !entry.auth)
        .collect();
        let chain = Arc::new(PluginChain { entries });
        self.post_cache.lock().insert(key, chain.clone());
        chain
    }
}

fn post_cache_key(
    route_id: Option<&str>,
    service_id: Option<&str>,
    consumer_id: Option<&str>,
    groups: &[ConsumerGroupRef<'_>],
    protocol: &str,
) -> String {
    let mut groups: Vec<String> = groups
        .iter()
        .map(|group| format!("{}={}", group.name, group.id))
        .collect();
    groups.sort();
    format!(
        "{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}",
        route_id.unwrap_or_default(),
        service_id.unwrap_or_default(),
        consumer_id.unwrap_or_default(),
        groups.join(","),
        protocol.to_ascii_lowercase()
    )
}

fn default_protocols() -> Vec<String> {
    vec!["http".to_string(), "https".to_string()]
}

fn resolve(
    entries: &[PluginEntry],
    route_id: Option<&str>,
    service_id: Option<&str>,
    consumer_id: Option<&str>,
    groups: &[ConsumerGroupRef<'_>],
    exclude: &HashSet<(String, String)>,
    protocol: &str,
) -> Vec<PluginEntry> {
    let mut best: HashMap<(&str, &str), (i32, &PluginEntry)> = HashMap::new();
    for entry in entries {
        if !entry.supports_protocol(protocol) {
            continue;
        }
        let key = (
            entry.name.as_str(),
            entry.instance_name.as_deref().unwrap_or(""),
        );
        if exclude.contains(&(entry.name.clone(), key.1.to_string())) {
            continue;
        }
        let Some(score) = entry.applies(route_id, service_id, consumer_id, groups) else {
            continue;
        };
        let replace = match best.get(&key) {
            None => true,
            Some((current, current_entry)) => {
                score > *current
                    || (score == *current
                        && (group_name(entry, groups), entry.id.as_str())
                            < (group_name(current_entry, groups), current_entry.id.as_str()))
            }
        };
        if replace {
            best.insert(key, (score, entry));
        }
    }

    let mut selected: Vec<PluginEntry> =
        best.into_values().map(|(_, entry)| entry.clone()).collect();
    selected.sort_by(|left, right| {
        right
            .priority
            .cmp(&left.priority)
            .then_with(|| left.name.cmp(&right.name))
    });
    apply_ordering(&mut selected);
    selected
}

fn group_name<'a>(entry: &PluginEntry, groups: &[ConsumerGroupRef<'a>]) -> &'a str {
    entry
        .consumer_group_id
        .as_deref()
        .and_then(|id| groups.iter().find(|group| group.id == id))
        .map(|group| group.name)
        .unwrap_or("")
}

/// Applies `ordering.before` / `ordering.after` constraints while keeping the
/// priority order for unconstrained plugins.
fn apply_ordering(entries: &mut Vec<PluginEntry>) {
    // Contradictory constraints (a before b, b before a) never settle, so bound
    // the passes; each pass applies at most one move.
    let max_passes = entries.len().saturating_mul(entries.len()).max(1) + 1;
    let mut passes = 0;
    let mut changed = true;
    while changed && passes < max_passes {
        changed = false;
        passes += 1;
        for index in 0..entries.len() {
            let ordering = entries[index].ordering.clone();
            let Some(ordering) = ordering else {
                continue;
            };
            let before = ordering
                .get("before")
                .and_then(Value::as_array)
                .map(|items| items.iter().filter_map(Value::as_str).collect::<Vec<_>>())
                .unwrap_or_default();
            let after = ordering
                .get("after")
                .and_then(Value::as_array)
                .map(|items| items.iter().filter_map(Value::as_str).collect::<Vec<_>>())
                .unwrap_or_default();

            for name in before {
                if let Some(other) = entries.iter().position(|entry| entry.name == name)
                    && other < index
                {
                    let entry = entries.remove(index);
                    entries.insert(other, entry);
                    changed = true;
                    break;
                }
            }
            if changed {
                break;
            }
            for name in after {
                if let Some(other) = entries.iter().position(|entry| entry.name == name)
                    && other > index
                {
                    let entry = entries.remove(index);
                    entries.insert(other, entry);
                    changed = true;
                    break;
                }
            }
            if changed {
                break;
            }
        }
    }
}

pub struct PluginRuntime {
    registry: Arc<PluginRegistry>,
    resolver: Arc<dyn PluginFactoryResolver>,
    host_provider: Arc<dyn PluginHostProvider>,
    chains: RwLock<Arc<PluginChains>>,
}

impl Default for PluginRuntime {
    fn default() -> Self {
        Self::empty()
    }
}

impl PluginRuntime {
    pub fn empty() -> Self {
        Self::new(Arc::new(PluginRegistry::default()))
    }

    pub fn new(registry: Arc<PluginRegistry>) -> Self {
        let resolver: Arc<dyn PluginFactoryResolver> = registry.clone();
        Self {
            registry,
            resolver,
            host_provider: Arc::new(NoopPluginHostProvider),
            chains: RwLock::new(Arc::new(PluginChains::default())),
        }
    }

    /// Builds a runtime whose factory lookup consults `resolver` (for example
    /// built-ins plus installed WASM plugins) while keeping the static
    /// registry available for admin schema endpoints.
    pub fn with_resolver(
        registry: Arc<PluginRegistry>,
        resolver: Arc<dyn PluginFactoryResolver>,
        host_provider: Arc<dyn PluginHostProvider>,
    ) -> Self {
        Self {
            registry,
            resolver,
            host_provider,
            chains: RwLock::new(Arc::new(PluginChains::default())),
        }
    }

    /// Compiles an initial set of plugin instances. The host loads instances
    /// from its configuration store and passes them in.
    pub fn with_plugins(
        registry: Arc<PluginRegistry>,
        plugins: Vec<PluginInstance>,
    ) -> Result<Self> {
        let runtime = Self::new(registry);
        runtime.reload(plugins)?;
        Ok(runtime)
    }

    pub fn registry(&self) -> &Arc<PluginRegistry> {
        &self.registry
    }

    pub fn factory(&self, name: &str) -> Option<Arc<dyn ApiGatewayPluginFactory>> {
        self.resolver.factory(name)
    }

    pub fn manifests(&self) -> Vec<PluginManifest> {
        let mut manifests = self.resolver.manifests();
        manifests.sort_by(|left, right| left.name.cmp(&right.name));
        manifests
    }

    pub fn validate(&self, name: &str, config: &Value) -> std::result::Result<(), PluginError> {
        self.factory(name)
            .ok_or_else(|| PluginError::fail_closed(format!("unknown plugin '{name}'")))?
            .validate(config)
    }

    pub fn reload(&self, plugins: Vec<PluginInstance>) -> Result<()> {
        let chains = PluginChains::compile(plugins, &*self.resolver, &*self.host_provider)?;
        *self.chains.write() = Arc::new(chains);
        Ok(())
    }

    pub fn pre_auth_chain(
        &self,
        route_id: Option<&str>,
        service_id: Option<&str>,
        protocol: &str,
    ) -> Arc<PluginChain> {
        let chains = self.chains.read().clone();
        chains.pre_auth(route_id, service_id, protocol)
    }

    pub fn post_auth_chain(
        &self,
        route_id: Option<&str>,
        service_id: Option<&str>,
        consumer_id: Option<&str>,
        group_ids: &[String],
        group_names: &[String],
        protocol: &str,
    ) -> Arc<PluginChain> {
        let chains = self.chains.read().clone();
        chains.post_auth(
            route_id,
            service_id,
            consumer_id,
            group_ids,
            group_names,
            protocol,
        )
    }

    pub fn has_plugins(&self) -> bool {
        self.chains.read().has_any
    }

    /// Snapshot of subscriptions declared by the currently compiled plugin
    /// instances. Reloading the runtime replaces the snapshot atomically.
    pub fn service_subscribers(&self) -> Vec<Arc<dyn EventSubscriber>> {
        self.chains
            .read()
            .subscribers
            .iter()
            .cloned()
            .map(|subscriber| subscriber as Arc<dyn EventSubscriber>)
            .collect()
    }

    /// Snapshot of scheduled jobs declared by the currently compiled plugin
    /// instances.
    pub fn service_jobs(&self) -> Vec<Arc<dyn ScheduledJob>> {
        self.chains
            .read()
            .jobs
            .iter()
            .cloned()
            .map(|job| job as Arc<dyn ScheduledJob>)
            .collect()
    }
}

impl PluginServiceSource for PluginRuntime {
    fn subscribers(&self) -> Vec<Arc<dyn EventSubscriber>> {
        self.service_subscribers()
    }

    fn jobs(&self) -> Vec<Arc<dyn ScheduledJob>> {
        self.service_jobs()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use async_trait::async_trait;
    use bytes::Bytes;
    use http::{HeaderMap, Method, StatusCode, Uri};
    use octra_api_core::{PluginInstance, Route, Service};
    use octra_api_plugin_types::{
        ApiGatewayPlugin, ApiGatewayPluginFactory, BodyContext, Capability, ErrorContext,
        FailPolicy, GatewayState, LogContext, PluginAction, PluginError, PluginKind,
        PluginManifest, PluginPhase, PluginResult, PluginRuntimeKind, RequestBodyContext,
        RequestContext, ResponseBodyContext, ResponseContext, UpstreamRequestContext,
    };
    use serde_json::{Value, json};

    use super::{
        CompositePluginFactoryResolver, NoopPluginHostProvider, PluginChain, PluginRegistry,
        PluginRuntime,
    };

    // ---------------------------------------------------------------------
    // Stub factory: one configurable plugin per manifest name. Every hook
    // records `stage:<label>` so tests can assert exact ordering.
    // ---------------------------------------------------------------------

    #[derive(Clone, Copy)]
    enum Outcome {
        Continue,
        ShortCircuit(u16),
        FailOpen,
        FailClosed,
        Hang,
    }

    #[derive(Clone)]
    struct StubSpec {
        name: &'static str,
        priority: i32,
        timeout_ms: u64,
        phases: Vec<PluginPhase>,
        capabilities: Vec<Capability>,
        rejects_config: bool,
        outcome: Outcome,
    }

    fn spec(name: &'static str) -> StubSpec {
        StubSpec {
            name,
            priority: 0,
            timeout_ms: 100,
            phases: vec![PluginPhase::Request],
            capabilities: Vec::new(),
            rejects_config: false,
            outcome: Outcome::Continue,
        }
    }

    impl StubSpec {
        fn with_priority(mut self, priority: i32) -> Self {
            self.priority = priority;
            self
        }

        fn with_phases(mut self, phases: &[PluginPhase]) -> Self {
            self.phases = phases.to_vec();
            self
        }

        fn with_capability(mut self, capability: Capability) -> Self {
            self.capabilities.push(capability);
            self
        }

        fn with_timeout(mut self, timeout_ms: u64) -> Self {
            self.timeout_ms = timeout_ms;
            self
        }

        fn with_outcome(mut self, outcome: Outcome) -> Self {
            self.outcome = outcome;
            self
        }

        fn rejecting_config(mut self) -> Self {
            self.rejects_config = true;
            self
        }

        fn manifest(&self) -> PluginManifest {
            PluginManifest {
                name: self.name.to_string(),
                version: "1.0.0".to_string(),
                kind: PluginKind::Api,
                runtime: PluginRuntimeKind::Builtin,
                priority: self.priority,
                description: String::new(),
                timeout_ms: self.timeout_ms,
                fail_policy: FailPolicy::Open,
                phases: self.phases.clone(),
                capabilities: self.capabilities.clone(),
                config_schema: None,
                api_version: "2".to_string(),
                entities: Vec::new(),
                migrations: Vec::new(),
                admin_routes: Vec::new(),
                subscriptions: Vec::new(),
                jobs: Vec::new(),
            }
        }
    }

    fn short_circuit(status: u16) -> axum::response::Response {
        axum::response::Response::builder()
            .status(StatusCode::from_u16(status).expect("valid status"))
            .body(axum::body::Body::empty())
            .expect("response")
    }

    struct StubPlugin {
        manifest: PluginManifest,
        label: String,
        outcome: Outcome,
        trace: Arc<Mutex<Vec<String>>>,
    }

    impl StubPlugin {
        fn record(&self, stage: &str) {
            self.trace
                .lock()
                .expect("trace lock")
                .push(format!("{stage}:{}", self.label));
        }

        async fn decide(&self) -> PluginResult {
            match self.outcome {
                Outcome::Continue => Ok(PluginAction::Continue),
                Outcome::ShortCircuit(status) => {
                    Ok(PluginAction::ShortCircuit(short_circuit(status)))
                }
                Outcome::FailOpen => Err(PluginError::fail_open("stub failure")),
                Outcome::FailClosed => Err(PluginError::fail_closed("stub failure")),
                Outcome::Hang => {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    Ok(PluginAction::Continue)
                }
            }
        }

        async fn act(&self, stage: &str) -> PluginResult {
            self.record(stage);
            self.decide().await
        }

        async fn transform(&self, stage: &str, body: Bytes) -> Result<Bytes, PluginError> {
            self.record(stage);
            match self.outcome {
                Outcome::FailOpen => Err(PluginError::fail_open("stub failure")),
                Outcome::FailClosed => Err(PluginError::fail_closed("stub failure")),
                Outcome::Hang => {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    Ok(body)
                }
                Outcome::Continue | Outcome::ShortCircuit(_) => {
                    let mut next = body.to_vec();
                    next.extend_from_slice(self.label.as_bytes());
                    Ok(Bytes::from(next))
                }
            }
        }
    }

    #[async_trait]
    impl ApiGatewayPlugin for StubPlugin {
        fn manifest(&self) -> &PluginManifest {
            &self.manifest
        }

        async fn on_rewrite(&self, _cx: &mut RequestContext<'_>) -> PluginResult {
            self.act("rewrite").await
        }

        async fn on_request(&self, _cx: &mut RequestContext<'_>) -> PluginResult {
            self.act("request").await
        }

        async fn on_upstream_request(&self, _cx: &mut UpstreamRequestContext<'_>) -> PluginResult {
            self.act("upstream_request").await
        }

        async fn on_response(&self, _cx: &mut ResponseContext<'_>) -> PluginResult {
            self.act("response").await
        }

        async fn on_error(&self, cx: &mut ErrorContext<'_>) -> PluginResult {
            self.trace
                .lock()
                .expect("trace lock")
                .push(format!("error:{}:{}", self.label, cx.error));
            self.decide().await
        }

        async fn on_request_buffer(
            &self,
            _cx: &mut RequestBodyContext<'_>,
            body: Bytes,
        ) -> Result<Bytes, PluginError> {
            self.transform("request_buffer", body).await
        }

        async fn on_response_buffer(
            &self,
            _cx: &mut ResponseBodyContext<'_>,
            body: Bytes,
        ) -> Result<Bytes, PluginError> {
            self.transform("response_buffer", body).await
        }

        async fn on_response_body(
            &self,
            _cx: &BodyContext<'_>,
            chunk: Bytes,
        ) -> Result<Bytes, PluginError> {
            self.transform("response_body", chunk).await
        }

        async fn on_log(&self, _cx: &LogContext<'_>) {
            if matches!(self.outcome, Outcome::Hang) {
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            self.record("log");
        }
    }

    struct StubFactory {
        spec: StubSpec,
        trace: Arc<Mutex<Vec<String>>>,
    }

    impl ApiGatewayPluginFactory for StubFactory {
        fn manifest(&self) -> PluginManifest {
            self.spec.manifest()
        }

        fn create(
            &self,
            _id: &str,
            config: Value,
        ) -> Result<Arc<dyn ApiGatewayPlugin>, PluginError> {
            if self.spec.rejects_config {
                return Err(PluginError::fail_closed("invalid config"));
            }
            let label = config
                .get("label")
                .and_then(Value::as_str)
                .unwrap_or(self.spec.name)
                .to_string();
            Ok(Arc::new(StubPlugin {
                manifest: self.spec.manifest(),
                label,
                outcome: self.spec.outcome,
                trace: self.trace.clone(),
            }))
        }

        fn phases(&self) -> Vec<PluginPhase> {
            self.spec.phases.clone()
        }
    }

    // ---------------------------------------------------------------------
    // Harness
    // ---------------------------------------------------------------------

    struct Harness {
        runtime: PluginRuntime,
        trace: Arc<Mutex<Vec<String>>>,
    }

    impl Harness {
        fn new(specs: &[StubSpec], instances: Vec<PluginInstance>) -> Self {
            let trace = Arc::new(Mutex::new(Vec::new()));
            let mut registry = PluginRegistry::new();
            for spec in specs {
                registry.register(StubFactory {
                    spec: spec.clone(),
                    trace: trace.clone(),
                });
            }
            let runtime = PluginRuntime::new(Arc::new(registry));
            runtime.reload(instances).expect("reload");
            Self { runtime, trace }
        }

        fn trace(&self) -> Vec<String> {
            self.trace.lock().expect("trace lock").clone()
        }

        fn clear_trace(&self) {
            self.trace.lock().expect("trace lock").clear();
        }
    }

    trait InstanceExt: Sized {
        fn labeled(self, label: &str) -> Self;
        fn with_id(self, id: &str) -> Self;
        fn named(self, instance_name: &str) -> Self;
        fn on_route(self, route: &str) -> Self;
        fn on_service(self, service: &str) -> Self;
        fn for_consumer(self, consumer: &str) -> Self;
        fn for_group(self, group: &str) -> Self;
        fn with_protocols(self, protocols: &[&str]) -> Self;
        fn with_ordering(self, ordering: Value) -> Self;
        fn disabled(self) -> Self;
    }

    impl InstanceExt for PluginInstance {
        fn labeled(mut self, label: &str) -> Self {
            self.config = json!({ "label": label });
            self
        }

        fn with_id(mut self, id: &str) -> Self {
            self.id = id.to_string();
            self
        }

        fn named(mut self, instance_name: &str) -> Self {
            self.instance_name = Some(instance_name.to_string());
            self
        }

        fn on_route(mut self, route: &str) -> Self {
            self.route_id = Some(route.to_string());
            self
        }

        fn on_service(mut self, service: &str) -> Self {
            self.service_id = Some(service.to_string());
            self
        }

        fn for_consumer(mut self, consumer: &str) -> Self {
            self.consumer_id = Some(consumer.to_string());
            self
        }

        fn for_group(mut self, group: &str) -> Self {
            self.consumer_group_id = Some(group.to_string());
            self
        }

        fn with_protocols(mut self, protocols: &[&str]) -> Self {
            self.protocols = protocols
                .iter()
                .map(|protocol| (*protocol).to_string())
                .collect();
            self
        }

        fn with_ordering(mut self, ordering: Value) -> Self {
            self.ordering = Some(ordering);
            self
        }

        fn disabled(mut self) -> Self {
            self.enabled = false;
            self
        }
    }

    fn instance(name: &str) -> PluginInstance {
        PluginInstance {
            id: name.to_string(),
            name: name.to_string(),
            instance_name: None,
            config: json!({ "label": name }),
            enabled: true,
            protocols: Vec::new(),
            ordering: None,
            service_id: None,
            route_id: None,
            consumer_id: None,
            consumer_group_id: None,
            tags: Vec::new(),
            created_at: 0,
            updated_at: 0,
        }
    }

    fn route() -> Route {
        Route {
            id: "route".to_string(),
            name: None,
            protocols: Vec::new(),
            methods: Vec::new(),
            hosts: Vec::new(),
            paths: vec!["/".to_string()],
            headers: Default::default(),
            strip_path: false,
            preserve_host: false,
            request_buffering: true,
            response_buffering: true,
            regex_priority: 0,
            service_id: None,
            enabled: true,
            tags: Vec::new(),
            created_at: 0,
            updated_at: 0,
        }
    }

    fn service() -> Service {
        Service {
            id: "service".to_string(),
            name: "service".to_string(),
            protocol: "http".to_string(),
            host: "localhost".to_string(),
            port: 80,
            path: None,
            retries: 0,
            connect_timeout: 0,
            write_timeout: 0,
            read_timeout: 0,
            enabled: true,
            description: String::new(),
            tags: Vec::new(),
            created_at: 0,
            updated_at: 0,
        }
    }

    fn request_context<'a>(
        state: &'a mut GatewayState,
        uri: &'a mut Uri,
        headers: &'a mut HeaderMap,
        method: &'a Method,
        route: &'a Route,
        service: &'a Service,
    ) -> RequestContext<'a> {
        RequestContext {
            request_id: "req",
            state,
            matched_path: "/",
            method,
            uri,
            headers,
            client_ip: None,
            consumer_id: None,
            credential_id: None,
            route,
            service,
        }
    }

    async fn run_rewrite(
        chain: &PluginChain,
    ) -> Result<Option<axum::response::Response>, PluginError> {
        let route = route();
        let service = service();
        let method = Method::GET;
        let mut uri: Uri = "/".parse().expect("uri");
        let mut headers = HeaderMap::new();
        let mut state = GatewayState::default();
        let mut cx = request_context(
            &mut state,
            &mut uri,
            &mut headers,
            &method,
            &route,
            &service,
        );
        chain.on_rewrite(&mut cx).await
    }

    async fn run_request(
        chain: &PluginChain,
    ) -> Result<Option<axum::response::Response>, PluginError> {
        let route = route();
        let service = service();
        let method = Method::GET;
        let mut uri: Uri = "/".parse().expect("uri");
        let mut headers = HeaderMap::new();
        let mut state = GatewayState::default();
        let mut cx = request_context(
            &mut state,
            &mut uri,
            &mut headers,
            &method,
            &route,
            &service,
        );
        chain.on_request(&mut cx).await
    }

    async fn run_upstream(
        chain: &PluginChain,
    ) -> Result<Option<axum::response::Response>, PluginError> {
        let route = route();
        let service = service();
        let method = Method::GET;
        let mut uri: Uri = "/".parse().expect("uri");
        let mut headers = HeaderMap::new();
        let mut state = GatewayState::default();
        let mut cx = UpstreamRequestContext {
            request_id: "req",
            state: &mut state,
            method: &method,
            uri: &mut uri,
            headers: &mut headers,
            consumer_id: None,
            route: &route,
            service: &service,
            target_url: "http://localhost/",
        };
        chain.on_upstream_request(&mut cx).await
    }

    async fn run_response(
        chain: &PluginChain,
    ) -> Result<Option<axum::response::Response>, PluginError> {
        let route = route();
        let service = service();
        let request_headers = HeaderMap::new();
        let mut status = StatusCode::OK;
        let mut headers = HeaderMap::new();
        let mut state = GatewayState::default();
        let mut cx = ResponseContext {
            request_id: "req",
            state: &mut state,
            request_headers: &request_headers,
            status: &mut status,
            headers: &mut headers,
            consumer_id: None,
            route: &route,
            service: &service,
            target_url: "http://localhost/",
        };
        chain.on_response(&mut cx).await
    }

    async fn run_error(
        chain: &PluginChain,
    ) -> Result<Option<axum::response::Response>, PluginError> {
        let route = route();
        let service = service();
        let method = Method::GET;
        let uri: Uri = "/".parse().expect("uri");
        let headers = HeaderMap::new();
        let mut state = GatewayState::default();
        let mut cx = ErrorContext {
            request_id: "req",
            state: &mut state,
            method: &method,
            uri: &uri,
            headers: &headers,
            consumer_id: None,
            route: &route,
            service: &service,
            target_url: "http://localhost/",
            error: "upstream exploded",
        };
        chain.on_error(&mut cx).await
    }

    async fn run_request_buffer(chain: &PluginChain, body: Bytes) -> Result<Bytes, PluginError> {
        let route = route();
        let service = service();
        let method = Method::GET;
        let mut uri: Uri = "/".parse().expect("uri");
        let mut headers = HeaderMap::new();
        let mut state = GatewayState::default();
        let mut cx = RequestBodyContext {
            request_id: "req",
            state: &mut state,
            method: &method,
            uri: &mut uri,
            headers: &mut headers,
            consumer_id: None,
            route: &route,
            service: &service,
        };
        chain.on_request_buffer(&mut cx, body).await
    }

    async fn run_response_buffer(chain: &PluginChain, body: Bytes) -> Result<Bytes, PluginError> {
        let route = route();
        let service = service();
        let headers = HeaderMap::new();
        let mut state = GatewayState::default();
        let mut cx = ResponseBodyContext {
            request_id: "req",
            state: &mut state,
            status: StatusCode::OK,
            headers: &headers,
            consumer_id: None,
            route: &route,
            service: &service,
            target_url: "http://localhost/",
        };
        chain.on_response_buffer(&mut cx, body).await
    }

    async fn run_response_body(chain: &PluginChain, chunk: Bytes) -> Result<Bytes, PluginError> {
        let route = route();
        let service = service();
        let mut state = GatewayState::default();
        let mut cx = BodyContext {
            request_id: "req",
            state: &mut state,
            route: &route,
            service: &service,
            target_url: "http://localhost/",
        };
        chain.on_response_body(&mut cx, chunk).await
    }

    async fn run_log(chain: &PluginChain) {
        let route = route();
        let service = service();
        let method = Method::GET;
        let mut state = GatewayState::default();
        let mut cx = LogContext {
            request_id: "req",
            state: &mut state,
            method: &method,
            path: "/",
            status: 200,
            latency: Duration::from_millis(1),
            consumer_id: None,
            route: &route,
            service: &service,
            target_url: Some("http://localhost/"),
            error: None,
        };
        chain.on_log(&mut cx).await;
    }

    // ---------------------------------------------------------------------
    // Ordering
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn orders_by_priority_then_name() {
        let harness = Harness::new(
            &[
                spec("alpha").with_priority(10),
                spec("beta").with_priority(20),
                spec("gamma").with_priority(20),
            ],
            vec![instance("alpha"), instance("beta"), instance("gamma")],
        );

        let chain = harness.runtime.pre_auth_chain(None, None, "http");
        assert_eq!(
            chain.names().collect::<Vec<_>>(),
            ["beta", "gamma", "alpha"]
        );
        run_request(&chain).await.expect("request phase");
        assert_eq!(
            harness.trace(),
            ["request:beta", "request:gamma", "request:alpha"]
        );
    }

    #[tokio::test]
    async fn ordering_before_and_after_override_priority() {
        let harness = Harness::new(
            &[
                spec("a").with_priority(30),
                spec("b").with_priority(20),
                spec("c").with_priority(10),
            ],
            vec![
                instance("a"),
                instance("b"),
                instance("c").with_ordering(json!({ "before": ["a"] })),
            ],
        );
        let chain = harness.runtime.pre_auth_chain(None, None, "http");
        assert_eq!(chain.names().collect::<Vec<_>>(), ["c", "a", "b"]);

        let harness = Harness::new(
            &[
                spec("a").with_priority(30),
                spec("b").with_priority(20),
                spec("c").with_priority(10),
            ],
            vec![
                instance("a").with_ordering(json!({ "after": ["c"] })),
                instance("b"),
                instance("c"),
            ],
        );
        let chain = harness.runtime.pre_auth_chain(None, None, "http");
        assert_eq!(chain.names().collect::<Vec<_>>(), ["b", "c", "a"]);
    }

    #[tokio::test]
    async fn ordering_chains_apply_and_cycles_terminate() {
        let harness = Harness::new(
            &[
                spec("a").with_priority(30),
                spec("b").with_priority(20),
                spec("c").with_priority(10),
            ],
            vec![
                instance("a"),
                instance("b").with_ordering(json!({ "before": ["a"] })),
                instance("c").with_ordering(json!({ "before": ["b"] })),
            ],
        );
        let chain = harness.runtime.pre_auth_chain(None, None, "http");
        assert_eq!(chain.names().collect::<Vec<_>>(), ["c", "b", "a"]);

        let harness = Harness::new(
            &[spec("a"), spec("b")],
            vec![
                instance("a").with_ordering(json!({ "before": ["b"] })),
                instance("b").with_ordering(json!({ "before": ["a"] })),
            ],
        );
        let chain = harness.runtime.pre_auth_chain(None, None, "http");
        let mut names = chain.names().collect::<Vec<_>>();
        names.sort();
        assert_eq!(names, ["a", "b"]);
    }

    // ---------------------------------------------------------------------
    // Scope resolution
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn route_scope_beats_service_scope_beats_global() {
        let harness = Harness::new(
            &[spec("stub")],
            vec![
                instance("stub").labeled("global"),
                instance("stub").labeled("service").on_service("svc"),
                instance("stub")
                    .labeled("route")
                    .on_route("rt")
                    .on_service("svc"),
            ],
        );

        let chain = harness
            .runtime
            .pre_auth_chain(Some("rt"), Some("svc"), "http");
        assert_eq!(chain.names().collect::<Vec<_>>(), ["stub"]);
        run_request(&chain).await.expect("request phase");
        assert_eq!(harness.trace(), ["request:route"]);

        harness.clear_trace();
        let chain = harness
            .runtime
            .pre_auth_chain(Some("other"), Some("svc"), "http");
        run_request(&chain).await.expect("request phase");
        assert_eq!(harness.trace(), ["request:service"]);

        harness.clear_trace();
        let chain = harness.runtime.pre_auth_chain(None, None, "http");
        run_request(&chain).await.expect("request phase");
        assert_eq!(harness.trace(), ["request:global"]);
    }

    #[tokio::test]
    async fn distinct_instance_names_all_run_but_duplicates_dedupe_by_id() {
        let harness = Harness::new(
            &[spec("stub")],
            vec![
                instance("stub").named("a").labeled("a"),
                instance("stub").named("b").labeled("b"),
            ],
        );
        let chain = harness.runtime.pre_auth_chain(None, None, "http");
        run_request(&chain).await.expect("request phase");
        let mut trace = harness.trace();
        trace.sort();
        assert_eq!(trace, ["request:a", "request:b"]);

        let harness = Harness::new(
            &[spec("stub")],
            vec![
                instance("stub").with_id("z").labeled("z"),
                instance("stub").with_id("a").labeled("a"),
            ],
        );
        let chain = harness.runtime.pre_auth_chain(None, None, "http");
        assert_eq!(chain.names().count(), 1);
        run_request(&chain).await.expect("request phase");
        assert_eq!(harness.trace(), ["request:a"]);
    }

    #[tokio::test]
    async fn consumer_and_group_scopes_resolve_after_authentication() {
        let harness = Harness::new(
            &[spec("quota")],
            vec![
                instance("quota").labeled("global"),
                instance("quota").labeled("consumer").for_consumer("c1"),
                instance("quota").labeled("group").for_group("g1"),
            ],
        );

        let pre = harness.runtime.pre_auth_chain(None, None, "http");
        assert!(pre.is_empty());

        let chain = harness
            .runtime
            .post_auth_chain(None, None, Some("c1"), &[], &[], "http");
        run_request(&chain).await.expect("request phase");
        assert_eq!(harness.trace(), ["request:consumer"]);

        harness.clear_trace();
        let chain = harness
            .runtime
            .post_auth_chain(None, None, Some("c2"), &[], &[], "http");
        run_request(&chain).await.expect("request phase");
        assert_eq!(harness.trace(), ["request:global"]);

        harness.clear_trace();
        let chain = harness.runtime.post_auth_chain(
            None,
            None,
            Some("c2"),
            &["g1".to_string()],
            &["gold".to_string()],
            "http",
        );
        run_request(&chain).await.expect("request phase");
        assert_eq!(harness.trace(), ["request:group"]);
    }

    #[tokio::test]
    async fn auth_entries_are_excluded_after_authentication() {
        let harness = Harness::new(
            &[
                spec("auth")
                    .with_priority(100)
                    .with_capability(Capability::Auth),
                spec("quota"),
            ],
            vec![
                instance("auth"),
                instance("quota").labeled("global"),
                instance("quota").labeled("consumer").for_consumer("c1"),
            ],
        );

        let pre = harness.runtime.pre_auth_chain(None, None, "http");
        assert!(pre.has_auth());
        assert_eq!(pre.names().collect::<Vec<_>>(), ["auth"]);

        let post = harness
            .runtime
            .post_auth_chain(None, None, Some("c1"), &[], &[], "http");
        assert!(!post.has_auth());
        assert_eq!(post.names().collect::<Vec<_>>(), ["quota"]);
        run_request(&post).await.expect("request phase");
        assert_eq!(harness.trace(), ["request:consumer"]);
    }

    #[tokio::test]
    async fn protocols_filter_instances_case_insensitively() {
        let harness = Harness::new(
            &[spec("stub")],
            vec![instance("stub").with_protocols(&["https"])],
        );

        assert!(
            harness
                .runtime
                .pre_auth_chain(None, None, "http")
                .is_empty()
        );
        assert!(
            !harness
                .runtime
                .pre_auth_chain(None, None, "HTTPS")
                .is_empty()
        );

        let harness = Harness::new(&[spec("stub")], vec![instance("stub")]);
        assert!(
            !harness
                .runtime
                .pre_auth_chain(None, None, "http")
                .is_empty()
        );
        assert!(
            !harness
                .runtime
                .pre_auth_chain(None, None, "https")
                .is_empty()
        );
    }

    // ---------------------------------------------------------------------
    // Failure policy
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn general_fail_open_continues_and_fail_closed_errors() {
        let open = Harness::new(
            &[spec("stub").with_outcome(Outcome::FailOpen)],
            vec![instance("stub")],
        );
        let chain = open.runtime.pre_auth_chain(None, None, "http");
        assert!(run_request(&chain).await.expect("fail-open").is_none());

        let closed = Harness::new(
            &[spec("stub").with_outcome(Outcome::FailClosed)],
            vec![instance("stub")],
        );
        let chain = closed.runtime.pre_auth_chain(None, None, "http");
        let error = run_request(&chain).await.expect_err("fail-closed");
        assert_eq!(error.fail_policy, FailPolicy::Closed);
    }

    #[tokio::test]
    async fn timeouts_fail_closed() {
        let harness = Harness::new(
            &[spec("stub").with_timeout(10).with_outcome(Outcome::Hang)],
            vec![instance("stub")],
        );
        let chain = harness.runtime.pre_auth_chain(None, None, "http");
        let error = run_request(&chain).await.expect_err("timeout");
        assert_eq!(error.fail_policy, FailPolicy::Closed);
        assert!(error.message.contains("timed out"));
    }

    #[tokio::test]
    async fn auth_failures_and_timeouts_fail_closed_even_when_the_policy_is_open() {
        let harness = Harness::new(
            &[spec("auth")
                .with_capability(Capability::Auth)
                .with_outcome(Outcome::FailOpen)],
            vec![instance("auth")],
        );
        let chain = harness.runtime.pre_auth_chain(None, None, "http");
        let error = run_request(&chain).await.expect_err("auth failure");
        assert_eq!(error.fail_policy, FailPolicy::Closed);

        let harness = Harness::new(
            &[spec("auth")
                .with_capability(Capability::Auth)
                .with_timeout(10)
                .with_outcome(Outcome::Hang)],
            vec![instance("auth")],
        );
        let chain = harness.runtime.pre_auth_chain(None, None, "http");
        let error = run_request(&chain).await.expect_err("auth timeout");
        assert_eq!(error.fail_policy, FailPolicy::Closed);
    }

    // ---------------------------------------------------------------------
    // Short-circuiting
    // ---------------------------------------------------------------------

    fn short_circuit_harness(phase: PluginPhase) -> Harness {
        Harness::new(
            &[
                spec("a")
                    .with_phases(&[phase])
                    .with_outcome(Outcome::ShortCircuit(418)),
                spec("b").with_phases(&[phase]),
            ],
            vec![instance("a"), instance("b")],
        )
    }

    async fn assert_short_circuit(result: Result<Option<axum::response::Response>, PluginError>) {
        let response = result.expect("stage result").expect("short circuit");
        assert_eq!(
            response.status(),
            StatusCode::from_u16(418).expect("status")
        );
    }

    #[tokio::test]
    async fn short_circuits_stop_the_rest_of_each_stage() {
        let harness = short_circuit_harness(PluginPhase::Rewrite);
        let chain = harness.runtime.pre_auth_chain(None, None, "http");
        assert_short_circuit(run_rewrite(&chain).await).await;
        assert_eq!(harness.trace(), ["rewrite:a"]);

        let harness = short_circuit_harness(PluginPhase::Request);
        let chain = harness.runtime.pre_auth_chain(None, None, "http");
        assert_short_circuit(run_request(&chain).await).await;
        assert_eq!(harness.trace(), ["request:a"]);

        let harness = short_circuit_harness(PluginPhase::UpstreamRequest);
        let chain = harness.runtime.pre_auth_chain(None, None, "http");
        assert_short_circuit(run_upstream(&chain).await).await;
        assert_eq!(harness.trace(), ["upstream_request:a"]);

        let harness = short_circuit_harness(PluginPhase::Response);
        let chain = harness.runtime.pre_auth_chain(None, None, "http");
        assert_short_circuit(run_response(&chain).await).await;
        assert_eq!(harness.trace(), ["response:a"]);

        let harness = short_circuit_harness(PluginPhase::Error);
        let chain = harness.runtime.pre_auth_chain(None, None, "http");
        assert_short_circuit(run_error(&chain).await).await;
        assert_eq!(harness.trace(), ["error:a:upstream exploded"]);
    }

    // ---------------------------------------------------------------------
    // Bytes stages
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn bytes_stages_fail_open_preserve_the_body() {
        let harness = Harness::new(
            &[spec("stub").with_outcome(Outcome::FailOpen)],
            vec![instance("stub")],
        );
        let chain = harness.runtime.pre_auth_chain(None, None, "http");
        assert_eq!(
            run_request_buffer(&chain, Bytes::from_static(b"body"))
                .await
                .expect("request buffer fail-open"),
            Bytes::from_static(b"body")
        );

        let harness = Harness::new(
            &[spec("stub")
                .with_phases(&[PluginPhase::ResponseBody])
                .with_outcome(Outcome::FailOpen)],
            vec![instance("stub")],
        );
        let chain = harness.runtime.pre_auth_chain(None, None, "http");
        assert_eq!(
            run_response_buffer(&chain, Bytes::from_static(b"body"))
                .await
                .expect("response buffer fail-open"),
            Bytes::from_static(b"body")
        );
        assert_eq!(
            run_response_body(&chain, Bytes::from_static(b"chunk"))
                .await
                .expect("response body fail-open"),
            Bytes::from_static(b"chunk")
        );

        let harness = Harness::new(
            &[spec("stub")
                .with_phases(&[PluginPhase::ResponseBody])
                .with_outcome(Outcome::FailClosed)],
            vec![instance("stub")],
        );
        let chain = harness.runtime.pre_auth_chain(None, None, "http");
        assert!(
            run_response_buffer(&chain, Bytes::from_static(b"body"))
                .await
                .is_err()
        );
        assert!(
            run_response_body(&chain, Bytes::from_static(b"chunk"))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn bytes_stages_chain_in_priority_order() {
        let harness = Harness::new(
            &[spec("a").with_priority(10), spec("b")],
            vec![instance("a"), instance("b")],
        );
        let chain = harness.runtime.pre_auth_chain(None, None, "http");
        assert_eq!(
            run_request_buffer(&chain, Bytes::from_static(b"x"))
                .await
                .expect("request buffer"),
            Bytes::from_static(b"xab")
        );

        let harness = Harness::new(
            &[
                spec("a")
                    .with_priority(10)
                    .with_phases(&[PluginPhase::ResponseBody]),
                spec("b").with_phases(&[PluginPhase::ResponseBody]),
            ],
            vec![instance("a"), instance("b")],
        );
        let chain = harness.runtime.pre_auth_chain(None, None, "http");
        assert_eq!(
            run_response_buffer(&chain, Bytes::from_static(b"x"))
                .await
                .expect("response buffer"),
            Bytes::from_static(b"xab")
        );
        assert_eq!(
            run_response_body(&chain, Bytes::from_static(b"x"))
                .await
                .expect("response body"),
            Bytes::from_static(b"xab")
        );
    }

    // ---------------------------------------------------------------------
    // Log hook
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn log_hook_runs_for_all_entries_and_swallows_timeouts() {
        let harness = Harness::new(
            &[
                spec("a").with_phases(&[PluginPhase::Log]),
                spec("b").with_phases(&[PluginPhase::Log]),
            ],
            vec![instance("a"), instance("b")],
        );
        let chain = harness.runtime.pre_auth_chain(None, None, "http");
        run_log(&chain).await;
        assert_eq!(harness.trace(), ["log:a", "log:b"]);

        let harness = Harness::new(
            &[
                spec("slow")
                    .with_phases(&[PluginPhase::Log])
                    .with_timeout(10)
                    .with_outcome(Outcome::Hang),
                spec("fast").with_phases(&[PluginPhase::Log]),
            ],
            vec![instance("slow"), instance("fast")],
        );
        let chain = harness.runtime.pre_auth_chain(None, None, "http");
        run_log(&chain).await;
        assert_eq!(harness.trace(), ["log:fast"]);
    }

    // ---------------------------------------------------------------------
    // Compilation and resolver behaviour
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn disabled_unknown_and_invalid_instances_are_skipped() {
        let harness = Harness::new(
            &[spec("known")],
            vec![instance("known").disabled(), instance("unknown")],
        );
        assert!(
            harness
                .runtime
                .pre_auth_chain(None, None, "http")
                .is_empty()
        );
        assert!(!harness.runtime.has_plugins());

        let harness = Harness::new(
            &[spec("broken").rejecting_config()],
            vec![instance("broken")],
        );
        assert!(
            harness
                .runtime
                .pre_auth_chain(None, None, "http")
                .is_empty()
        );
        assert!(!harness.runtime.has_plugins());
    }

    #[tokio::test]
    async fn composite_resolver_prefers_builtins_and_dedupes_manifests() {
        let trace = Arc::new(Mutex::new(Vec::new()));
        let mut builtins = PluginRegistry::new();
        builtins.register(StubFactory {
            spec: spec("shared").with_priority(1),
            trace: trace.clone(),
        });
        builtins.register(StubFactory {
            spec: spec("only-builtin"),
            trace: trace.clone(),
        });
        let mut dynamic = PluginRegistry::new();
        dynamic.register(StubFactory {
            spec: spec("shared").with_priority(2),
            trace: trace.clone(),
        });
        dynamic.register(StubFactory {
            spec: spec("only-dynamic"),
            trace,
        });

        let resolver = CompositePluginFactoryResolver::new(Arc::new(builtins), Arc::new(dynamic));
        let runtime = PluginRuntime::with_resolver(
            Arc::new(PluginRegistry::new()),
            Arc::new(resolver),
            Arc::new(NoopPluginHostProvider),
        );

        assert_eq!(
            runtime
                .manifests()
                .into_iter()
                .map(|manifest| manifest.name)
                .collect::<Vec<_>>(),
            ["only-builtin", "only-dynamic", "shared"]
        );
        assert_eq!(
            runtime
                .factory("shared")
                .expect("builtin factory")
                .manifest()
                .priority,
            1
        );
    }

    #[tokio::test]
    async fn chains_are_cached_and_replaced_on_reload() {
        let harness = Harness::new(&[spec("stub")], vec![instance("stub")]);
        let first = harness.runtime.pre_auth_chain(Some("r"), Some("s"), "http");
        let same = harness.runtime.pre_auth_chain(Some("r"), Some("s"), "http");
        assert!(Arc::ptr_eq(&first, &same));

        let other = harness
            .runtime
            .pre_auth_chain(Some("r2"), Some("s"), "http");
        assert!(!Arc::ptr_eq(&first, &other));

        let post =
            harness
                .runtime
                .post_auth_chain(Some("r"), Some("s"), Some("c"), &[], &[], "http");
        let post_same =
            harness
                .runtime
                .post_auth_chain(Some("r"), Some("s"), Some("c"), &[], &[], "http");
        assert!(Arc::ptr_eq(&post, &post_same));

        harness
            .runtime
            .reload(vec![instance("stub").labeled("new")])
            .expect("reload");
        let after = harness.runtime.pre_auth_chain(Some("r"), Some("s"), "http");
        assert!(!Arc::ptr_eq(&first, &after));
    }

    #[tokio::test]
    async fn empty_runtime_shares_an_empty_chain_and_validates_names() {
        let runtime = PluginRuntime::empty();
        let first = runtime.pre_auth_chain(None, None, "http");
        let second = runtime.pre_auth_chain(Some("r"), Some("s"), "https");
        assert!(first.is_empty());
        assert!(Arc::ptr_eq(&first, &second));
        assert!(
            runtime
                .post_auth_chain(None, None, None, &[], &[], "http")
                .is_empty()
        );
        assert!(!runtime.has_plugins());

        let error = runtime
            .validate("missing", &json!({}))
            .expect_err("unknown plugin");
        assert!(error.message.contains("unknown plugin"));
    }

    #[tokio::test]
    async fn validate_accepts_registered_plugins() {
        let harness = Harness::new(&[spec("stub")], Vec::new());
        harness
            .runtime
            .validate("stub", &json!({}))
            .expect("known plugin");
    }
}
