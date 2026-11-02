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
use octra_core::{Error, Result};
use octra_plugin_api::{
    ApiGatewayPlugin, ApiGatewayPluginFactory, BodyContext, ErrorContext, FailPolicy, LogContext,
    PluginAction, PluginError, PluginManifest, PluginPhase, RequestBodyContext, RequestContext,
    ResponseBodyContext, ResponseContext, UpstreamRequestContext,
};
use octra_storage::Storage;
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

#[derive(Clone)]
struct PluginEntry {
    pub id: String,
    name: String,
    priority: i32,
    timeout: Duration,
    phases: Vec<PluginPhase>,
    auth: bool,
    ordering: Option<Value>,
    service_id: Option<String>,
    route_id: Option<String>,
    consumer_id: Option<String>,
    plugin: Arc<dyn ApiGatewayPlugin>,
}

impl PluginEntry {
    fn has_consumer_scope(&self) -> bool {
        self.consumer_id.is_some()
    }

    fn applies(
        &self,
        route_id: Option<&str>,
        service_id: Option<&str>,
        consumer_id: Option<&str>,
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
        let has_r = self.route_id.is_some();
        let has_s = self.service_id.is_some();
        let has_c = self.consumer_id.is_some();

        Some(match (has_c, has_r, has_s) {
            (true, true, true) => 8,
            (true, true, false) => 7,
            (true, false, true) => 6,
            (false, true, true) => 5,
            (true, false, false) => 4,
            (false, true, false) => 3,
            (false, false, true) => 2,
            (false, false, false) => 1,
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
            let action =
                run_stage(entry, PluginPhase::Request, entry.plugin.on_request(cx)).await?;
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
            if !entry.phases.contains(&PluginPhase::Log) {
                continue;
            }
            let action = run_stage(entry, PluginPhase::Log, entry.plugin.on_error(cx)).await?;
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
    pre_cache: Mutex<HashMap<(String, String), Arc<PluginChain>>>,
    post_cache: Mutex<HashMap<(String, String, String), Arc<PluginChain>>>,
    empty: Arc<PluginChain>,
    has_any: bool,
}

impl PluginChains {
    fn compile(plugins: Vec<PluginInstance>, registry: &PluginRegistry) -> Result<Self> {
        let mut entries = Vec::new();
        for plugin in plugins {
            if !plugin.enabled {
                continue;
            }
            let Some(factory) = registry.factory(&plugin.name) else {
                tracing::warn!(plugin = %plugin.name, "plugin is not registered; skipping config");
                continue;
            };
            let instance = match factory.create(&plugin.id, plugin.config.clone()) {
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
                priority: manifest.priority,
                timeout: manifest.timeout(),
                phases: factory.phases(),
                auth: factory.auth(),
                ordering: plugin.ordering.clone(),
                service_id: plugin.service_id.clone(),
                route_id: plugin.route_id.clone(),
                consumer_id: plugin.consumer_id.clone(),
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

    fn pre_auth(&self, route_id: Option<&str>, service_id: Option<&str>) -> Arc<PluginChain> {
        if !self.has_any {
            return self.empty.clone();
        }
        let key = (
            route_id.unwrap_or_default().to_string(),
            service_id.unwrap_or_default().to_string(),
        );
        if let Some(chain) = self.pre_cache.lock().get(&key) {
            return chain.clone();
        }
        let chain = Arc::new(PluginChain {
            entries: resolve(
                &self.entries,
                route_id,
                service_id,
                None,
                false,
                &HashSet::new(),
            ),
        });
        self.pre_cache.lock().insert(key, chain.clone());
        chain
    }

    fn post_auth(
        &self,
        route_id: Option<&str>,
        service_id: Option<&str>,
        consumer_id: &str,
    ) -> Arc<PluginChain> {
        if !self.has_any {
            return self.empty.clone();
        }
        let key = (
            route_id.unwrap_or_default().to_string(),
            service_id.unwrap_or_default().to_string(),
            consumer_id.to_string(),
        );
        if let Some(chain) = self.post_cache.lock().get(&key) {
            return chain.clone();
        }

        let mut seen = HashSet::new();
        for entry in resolve(
            &self.entries,
            route_id,
            service_id,
            None,
            false,
            &HashSet::new(),
        ) {
            seen.insert(entry.name);
        }

        let chain = Arc::new(PluginChain {
            entries: resolve(
                &self.entries,
                route_id,
                service_id,
                Some(consumer_id),
                true,
                &seen,
            ),
        });
        self.post_cache.lock().insert(key, chain.clone());
        chain
    }
}

fn resolve(
    entries: &[PluginEntry],
    route_id: Option<&str>,
    service_id: Option<&str>,
    consumer_id: Option<&str>,
    consumer_scoped_only: bool,
    exclude: &HashSet<String>,
) -> Vec<PluginEntry> {
    let mut best: HashMap<&str, (i32, &PluginEntry)> = HashMap::new();
    for entry in entries {
        if consumer_scoped_only && !entry.has_consumer_scope() {
            continue;
        }
        if exclude.contains(&entry.name) {
            continue;
        }
        let Some(score) = entry.applies(route_id, service_id, consumer_id) else {
            continue;
        };
        match best.get(entry.name.as_str()) {
            Some((current, _)) if *current >= score => {}
            _ => {
                best.insert(entry.name.as_str(), (score, entry));
            }
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

/// Applies `ordering.before` / `ordering.after` constraints while keeping the
/// priority order for unconstrained plugins.
fn apply_ordering(entries: &mut Vec<PluginEntry>) {
    let mut changed = true;
    while changed {
        changed = false;
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
    storage: Option<Arc<Storage>>,
    registry: Arc<PluginRegistry>,
    chains: RwLock<Arc<PluginChains>>,
}

impl Default for PluginRuntime {
    fn default() -> Self {
        Self::empty()
    }
}

impl PluginRuntime {
    pub fn empty() -> Self {
        Self {
            storage: None,
            registry: Arc::new(PluginRegistry::default()),
            chains: RwLock::new(Arc::new(PluginChains::default())),
        }
    }

    pub async fn bootstrap(storage: Arc<Storage>, registry: Arc<PluginRegistry>) -> Result<Self> {
        let runtime = Self {
            storage: Some(storage),
            registry,
            chains: RwLock::new(Arc::new(PluginChains::default())),
        };
        runtime.reload().await?;
        Ok(runtime)
    }

    pub fn registry(&self) -> &Arc<PluginRegistry> {
        &self.registry
    }

    pub async fn reload(&self) -> Result<()> {
        let Some(storage) = &self.storage else {
            return Ok(());
        };
        let plugins = storage.list_plugin_instances().await?;
        let chains = PluginChains::compile(plugins, &self.registry)?;
        *self.chains.write() = Arc::new(chains);
        Ok(())
    }

    pub fn pre_auth_chain(
        &self,
        route_id: Option<&str>,
        service_id: Option<&str>,
    ) -> Arc<PluginChain> {
        let chains = self.chains.read().clone();
        chains.pre_auth(route_id, service_id)
    }

    pub fn post_auth_chain(
        &self,
        route_id: Option<&str>,
        service_id: Option<&str>,
        consumer_id: &str,
    ) -> Arc<PluginChain> {
        let chains = self.chains.read().clone();
        chains.post_auth(route_id, service_id, consumer_id)
    }

    pub fn has_plugins(&self) -> bool {
        self.chains.read().has_any
    }
}
