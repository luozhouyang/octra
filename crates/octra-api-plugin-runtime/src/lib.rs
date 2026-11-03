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
    FailPolicy, LogContext, LogLevel, PluginAction, PluginError, PluginHost, PluginManifest,
    PluginPhase, PluginStorage, RequestBodyContext, RequestContext, ResponseBodyContext,
    ResponseContext, StorageError, UpstreamRequestContext,
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
            body = run_bytes_stage(entry, entry.plugin.on_request_buffer(cx, body)).await?;
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
            body = run_bytes_stage(entry, entry.plugin.on_response_buffer(cx, body)).await?;
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
                Ok(Bytes::new())
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
    pre_cache: Mutex<HashMap<(String, String, String), Arc<PluginChain>>>,
    post_cache: Mutex<HashMap<String, Arc<PluginChain>>>,
    empty: Arc<PluginChain>,
    has_any: bool,
}

impl PluginChains {
    fn compile(
        plugins: Vec<PluginInstance>,
        resolver: &dyn PluginFactoryResolver,
        host_provider: &dyn PluginHostProvider,
    ) -> Result<Self> {
        let mut entries = Vec::new();
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
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use async_trait::async_trait;
    use http::Method;
    use octra_api_core::{Route, Service};
    use octra_api_plugin_types::{
        ApiGatewayPlugin, Capability, ConsumerInfo, FailPolicy, GatewayState, PluginAction,
        PluginError, PluginKind, PluginManifest, PluginPhase, PluginResult, PluginRuntimeKind,
    };

    use super::{PluginChain, PluginEntry};

    enum Behaviour {
        Authenticate,
        FailOpen,
    }

    struct StubAuth {
        manifest: PluginManifest,
        behaviour: Behaviour,
    }

    #[async_trait]
    impl ApiGatewayPlugin for StubAuth {
        fn manifest(&self) -> &PluginManifest {
            &self.manifest
        }

        async fn on_request(
            &self,
            cx: &mut octra_api_plugin_types::RequestContext<'_>,
        ) -> PluginResult {
            match self.behaviour {
                Behaviour::Authenticate => {
                    cx.state.consumer = Some(ConsumerInfo {
                        id: "consumer-1".to_string(),
                        ..ConsumerInfo::default()
                    });
                    Ok(PluginAction::Continue)
                }
                Behaviour::FailOpen => Err(PluginError::fail_open("boom")),
            }
        }
    }

    fn manifest() -> PluginManifest {
        PluginManifest {
            name: "stub-auth".to_string(),
            version: "1.0.0".to_string(),
            kind: PluginKind::Api,
            runtime: PluginRuntimeKind::Builtin,
            priority: 0,
            description: String::new(),
            timeout_ms: 100,
            fail_policy: FailPolicy::Open,
            phases: vec![PluginPhase::Request],
            capabilities: vec![Capability::Auth],
            config_schema: None,
            api_version: "1".to_string(),
            entities: Vec::new(),
            migrations: Vec::new(),
            admin_routes: Vec::new(),
        }
    }

    fn chain(behaviour: Behaviour) -> PluginChain {
        PluginChain {
            entries: vec![PluginEntry {
                id: "stub-auth".to_string(),
                name: "stub-auth".to_string(),
                instance_name: None,
                priority: 0,
                timeout: Duration::from_millis(100),
                phases: vec![PluginPhase::Request],
                capabilities: vec![Capability::Auth],
                protocols: Vec::new(),
                auth: true,
                ordering: None,
                service_id: None,
                route_id: None,
                consumer_id: None,
                consumer_group_id: None,
                plugin: Arc::new(StubAuth {
                    manifest: manifest(),
                    behaviour,
                }),
            }],
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

    type AuthRun = (
        Result<Option<axum::response::Response>, PluginError>,
        Option<String>,
    );

    async fn run(behaviour: Behaviour) -> AuthRun {
        let mut uri = "/".parse().unwrap();
        let mut headers = http::HeaderMap::new();
        let mut state = GatewayState::default();
        let route = route();
        let service = service();
        let method = Method::GET;
        let mut cx = octra_api_plugin_types::RequestContext {
            request_id: "req",
            state: &mut state,
            matched_path: "/",
            method: &method,
            uri: &mut uri,
            headers: &mut headers,
            client_ip: None,
            consumer_id: None,
            credential_id: None,
            route: &route,
            service: &service,
        };
        let result = chain(behaviour).on_request(&mut cx).await;
        (result, state.consumer.map(|consumer| consumer.id))
    }

    #[tokio::test]
    async fn auth_entries_run_in_the_request_phase() {
        let (result, consumer) = run(Behaviour::Authenticate).await;
        assert!(result.unwrap().is_none());
        assert_eq!(consumer.as_deref(), Some("consumer-1"));
    }

    #[tokio::test]
    async fn auth_errors_fail_closed_even_when_the_policy_is_open() {
        let (result, consumer) = run(Behaviour::FailOpen).await;
        let error = result.unwrap_err();
        assert_eq!(error.fail_policy, FailPolicy::Closed);
        assert!(consumer.is_none());
    }
}
