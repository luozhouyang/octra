//! LLM plugin registry, scope resolution and ordered execution.
//!
//! Chains are compiled from the shared plugin catalog once per control-plane
//! mutation. The data plane only performs map lookups and in-memory calls, so
//! configuration changes never parse JSON on the request path.

// Rejections carry the full gateway error, as in `octra-llm-gateway`; boxing
// would only obscure the data-plane flow.
#![allow(clippy::result_large_err)]

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use octra_api_plugin_types::{FailPolicy, PluginError, PluginManifest};
use octra_core::{Error, Result};
use octra_llm_core::{GatewayError, StreamEvent};
use octra_llm_plugin_types::{
    LLM_PLUGIN_API_VERSION, LlmAction, LlmErrorContext, LlmLogContext, LlmLogEvent, LlmPlugin,
    LlmPluginFactory, LlmPluginResult, LlmProviderInfo, LlmProviderRequestContext,
    LlmRequestContext, LlmRequestMeta, LlmRequestState, LlmResponseContext, LlmScope,
    LlmStreamAction, LlmStreamContext, LlmStreamResult,
};
use octra_storage::Storage;
use octra_storage::plugins::{PluginConfigRecord, PluginData, PluginRecord};
use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use serde_json::Value;

#[derive(Default)]
pub struct LlmPluginRegistry {
    factories: HashMap<String, Arc<dyn LlmPluginFactory>>,
}

impl LlmPluginRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register<F>(&mut self, factory: F)
    where
        F: LlmPluginFactory + 'static,
    {
        self.register_arc(Arc::new(factory));
    }

    pub fn register_arc(&mut self, factory: Arc<dyn LlmPluginFactory>) {
        let manifest = factory.manifest();
        if factory.api_version() != LLM_PLUGIN_API_VERSION {
            tracing::warn!(
                plugin = %manifest.name,
                version = factory.api_version(),
                expected = LLM_PLUGIN_API_VERSION,
                "LLM plugin targets an incompatible API version; skipping"
            );
            return;
        }
        self.factories.insert(manifest.name, factory);
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

    pub fn factory(&self, name: &str) -> Option<Arc<dyn LlmPluginFactory>> {
        self.factories.get(name).cloned()
    }

    pub fn validate(&self, name: &str, config: &Value) -> std::result::Result<(), PluginError> {
        self.factory(name)
            .ok_or_else(|| PluginError::fail_closed(format!("unknown LLM plugin '{name}'")))?
            .validate(config)
    }
}

/// Stages tracked by the metrics counters, in snapshot order.
const STAGE_COUNT: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Request,
    ProviderRequest,
    Response,
    StreamEvent,
    Error,
    Log,
}

impl Stage {
    fn index(self) -> usize {
        match self {
            Self::Request => 0,
            Self::ProviderRequest => 1,
            Self::Response => 2,
            Self::StreamEvent => 3,
            Self::Error => 4,
            Self::Log => 5,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Request => "request",
            Self::ProviderRequest => "provider_request",
            Self::Response => "response",
            Self::StreamEvent => "stream_event",
            Self::Error => "error",
            Self::Log => "log",
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum StageFailure {
    Error,
    Timeout,
}

#[derive(Default)]
struct StageStats {
    calls: AtomicU64,
    failures: AtomicU64,
    timeouts: AtomicU64,
    rejections: AtomicU64,
    fail_open: AtomicU64,
    total_micros: AtomicU64,
    max_micros: AtomicU64,
}

#[derive(Default)]
struct PluginStats {
    stages: [StageStats; STAGE_COUNT],
}

impl PluginStats {
    fn record(
        &self,
        stage: Stage,
        elapsed: Duration,
        failure: Option<StageFailure>,
        fail_open: bool,
        rejected: bool,
    ) {
        let stats = &self.stages[stage.index()];
        stats.calls.fetch_add(1, Ordering::Relaxed);
        let micros = elapsed.as_micros().min(u64::MAX as u128) as u64;
        stats.total_micros.fetch_add(micros, Ordering::Relaxed);
        stats.max_micros.fetch_max(micros, Ordering::Relaxed);
        match failure {
            Some(StageFailure::Error) => {
                stats.failures.fetch_add(1, Ordering::Relaxed);
            }
            Some(StageFailure::Timeout) => {
                stats.failures.fetch_add(1, Ordering::Relaxed);
                stats.timeouts.fetch_add(1, Ordering::Relaxed);
            }
            None => {}
        }
        if fail_open {
            stats.fail_open.fetch_add(1, Ordering::Relaxed);
        }
        if rejected {
            stats.rejections.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Shared per-plugin counters. Configured entries clone the `Arc`, so counters
/// survive a chain reload and in-flight requests keep updating them.
#[derive(Clone, Default)]
pub struct LlmPluginMetrics {
    plugins: Arc<Mutex<HashMap<String, Arc<PluginStats>>>>,
}

impl LlmPluginMetrics {
    fn stats(&self, name: &str) -> Arc<PluginStats> {
        self.plugins
            .lock()
            .entry(name.to_string())
            .or_default()
            .clone()
    }

    pub fn snapshot(&self) -> Vec<PluginMetricsView> {
        let stats: Vec<(String, Arc<PluginStats>)> = self
            .plugins
            .lock()
            .iter()
            .map(|(name, stats)| (name.clone(), stats.clone()))
            .collect();
        let mut views: Vec<_> = stats
            .into_iter()
            .map(|(name, stats)| PluginMetricsView::from_stats(name, &stats))
            .collect();
        views.sort_by(|left, right| left.name.cmp(&right.name));
        views
    }
}

#[derive(Debug, Serialize)]
pub struct StageMetricsView {
    pub calls: u64,
    pub failures: u64,
    pub timeouts: u64,
    pub rejections: u64,
    pub fail_open: u64,
    pub total_micros: u64,
    pub max_micros: u64,
}

#[derive(Debug, Serialize)]
pub struct PluginMetricsView {
    pub name: String,
    pub stages: BTreeMap<&'static str, StageMetricsView>,
}

impl PluginMetricsView {
    fn from_stats(name: String, stats: &PluginStats) -> Self {
        let mut stages = BTreeMap::new();
        for (index, stage) in [
            Stage::Request,
            Stage::ProviderRequest,
            Stage::Response,
            Stage::StreamEvent,
            Stage::Error,
            Stage::Log,
        ]
        .into_iter()
        .enumerate()
        {
            let counters = &stats.stages[index];
            stages.insert(
                stage.as_str(),
                StageMetricsView {
                    calls: counters.calls.load(Ordering::Relaxed),
                    failures: counters.failures.load(Ordering::Relaxed),
                    timeouts: counters.timeouts.load(Ordering::Relaxed),
                    rejections: counters.rejections.load(Ordering::Relaxed),
                    fail_open: counters.fail_open.load(Ordering::Relaxed),
                    total_micros: counters.total_micros.load(Ordering::Relaxed),
                    max_micros: counters.max_micros.load(Ordering::Relaxed),
                },
            );
        }
        Self { name, stages }
    }
}

/// One entry of a compiled chain, as returned by introspection.
#[derive(Debug, Clone, Serialize)]
pub struct LlmChainEntry {
    pub name: String,
    pub scope: LlmScope,
    pub scope_id: String,
    pub priority: i64,
    pub timeout_ms: u64,
    pub fail_policy: FailPolicy,
}

#[derive(Clone)]
struct LlmEntry {
    name: String,
    scope: LlmScope,
    scope_id: String,
    priority: i64,
    timeout: Duration,
    fail_policy: FailPolicy,
    stats: Arc<PluginStats>,
    plugin: Arc<dyn LlmPlugin>,
}

type ScopeKey = (LlmScope, String);
type ChainKey = (Option<String>, Option<String>, String, Option<String>);

/// Errors surfaced by a chain: configuration/execution failures from a plugin,
/// or a deliberate rejection carrying the error the client should see.
#[derive(Debug)]
pub enum LlmChainError {
    Plugin(PluginError),
    Rejected(GatewayError),
}

impl From<PluginError> for LlmChainError {
    fn from(error: PluginError) -> Self {
        Self::Plugin(error)
    }
}

#[derive(Default)]
pub struct LlmPluginChain {
    entries: Vec<LlmEntry>,
}

/// Upper bound on the number of events a plugin may emit for one input event.
const MAX_STREAM_EVENTS: usize = 256;

impl LlmPluginChain {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|entry| entry.name.as_str())
    }

    /// Execution-order description of the chain, including the scope each
    /// entry was resolved from.
    pub fn describe(&self) -> Vec<LlmChainEntry> {
        self.entries
            .iter()
            .map(|entry| LlmChainEntry {
                name: entry.name.clone(),
                scope: entry.scope,
                scope_id: entry.scope_id.clone(),
                priority: entry.priority,
                timeout_ms: entry.timeout.as_millis().min(u64::MAX as u128) as u64,
                fail_policy: entry.fail_policy,
            })
            .collect()
    }

    pub async fn on_request(
        &self,
        meta: &LlmRequestMeta<'_>,
        request: &mut octra_llm_core::ChatRequest,
        state: &mut LlmRequestState,
    ) -> std::result::Result<Option<GatewayError>, LlmChainError> {
        for entry in &self.entries {
            let mut cx = LlmRequestContext {
                meta,
                request,
                state: state.slot(&entry.name),
            };
            match run_stage(entry, Stage::Request, entry.plugin.on_request(&mut cx)).await? {
                LlmAction::Continue => {}
                LlmAction::Reject(error) => return Ok(Some(error)),
            }
        }
        Ok(None)
    }

    pub async fn on_provider_request(
        &self,
        meta: &LlmRequestMeta<'_>,
        provider: &LlmProviderInfo<'_>,
        request: &mut octra_llm_core::ChatRequest,
        state: &mut LlmRequestState,
    ) -> std::result::Result<Option<GatewayError>, LlmChainError> {
        for entry in &self.entries {
            let mut cx = LlmProviderRequestContext {
                meta,
                provider,
                request,
                state: state.slot(&entry.name),
            };
            match run_stage(
                entry,
                Stage::ProviderRequest,
                entry.plugin.on_provider_request(&mut cx),
            )
            .await?
            {
                LlmAction::Continue => {}
                LlmAction::Reject(error) => return Ok(Some(error)),
            }
        }
        Ok(None)
    }

    /// Response stages unwind in reverse order so reversible transforms nest.
    pub async fn on_response(
        &self,
        meta: &LlmRequestMeta<'_>,
        request_model: &str,
        response: &mut octra_llm_core::ChatResponse,
        usage: &mut octra_llm_core::Usage,
        state: &mut LlmRequestState,
    ) -> std::result::Result<Option<GatewayError>, LlmChainError> {
        for entry in self.entries.iter().rev() {
            let mut cx = LlmResponseContext {
                meta,
                request_model,
                response,
                usage,
                state: state.slot(&entry.name),
            };
            match run_stage(entry, Stage::Response, entry.plugin.on_response(&mut cx)).await? {
                LlmAction::Continue => {}
                LlmAction::Reject(error) => return Ok(Some(error)),
            }
        }
        Ok(None)
    }

    pub async fn on_error(
        &self,
        meta: &LlmRequestMeta<'_>,
        provider_id: Option<&str>,
        account_id: Option<&str>,
        error: &mut GatewayError,
        state: &mut LlmRequestState,
    ) -> std::result::Result<(), LlmChainError> {
        for entry in self.entries.iter().rev() {
            let mut cx = LlmErrorContext {
                meta,
                provider_id,
                account_id,
                error,
                state: state.slot(&entry.name),
            };
            match run_stage(entry, Stage::Error, entry.plugin.on_error(&mut cx)).await? {
                LlmAction::Continue => {}
                LlmAction::Reject(replacement) => {
                    *cx.error = replacement;
                }
            }
        }
        Ok(())
    }

    /// Runs observational log hooks; failures never affect the response.
    pub async fn on_log(
        &self,
        meta: &LlmRequestMeta<'_>,
        event: &LlmLogEvent<'_>,
        state: &mut LlmRequestState,
    ) {
        for entry in self.entries.iter().rev() {
            let mut cx = LlmLogContext {
                meta,
                event,
                state: state.slot(&entry.name),
            };
            let started = Instant::now();
            let timed_out = tokio::time::timeout(entry.timeout, entry.plugin.on_log(&cx))
                .await
                .is_err();
            entry.stats.record(
                Stage::Log,
                started.elapsed(),
                timed_out.then_some(StageFailure::Timeout),
                timed_out,
                false,
            );
            if timed_out {
                tracing::warn!(plugin = %entry.name, "LLM plugin log hook timed out");
            }
            let _ = &mut cx;
        }
    }

    /// Runs every event through each plugin in reverse order, letting a plugin
    /// split or drop events. State is preserved per plugin for one stream.
    pub async fn on_stream_events(
        &self,
        meta: &LlmRequestMeta<'_>,
        events: Vec<StreamEvent>,
        state: &mut LlmRequestState,
    ) -> std::result::Result<Vec<StreamEvent>, LlmChainError> {
        let mut current = events;
        for entry in self.entries.iter().rev() {
            let mut next = Vec::with_capacity(current.len());
            for mut event in current {
                let action = {
                    let mut cx = LlmStreamContext {
                        meta,
                        event: &mut event,
                        state: state.slot(&entry.name),
                    };
                    run_stream_stage(entry, entry.plugin.on_stream_event(&mut cx)).await?
                };
                match action {
                    LlmStreamAction::Continue => next.push(event),
                    LlmStreamAction::Replace(events) => next.extend(events),
                    LlmStreamAction::Reject(error) => return Err(LlmChainError::Rejected(error)),
                }
                if next.len() > MAX_STREAM_EVENTS {
                    return Err(LlmChainError::Plugin(PluginError::fail_closed(format!(
                        "LLM plugin '{}' expanded a stream event beyond {MAX_STREAM_EVENTS}",
                        entry.name
                    ))));
                }
            }
            current = next;
        }
        Ok(current)
    }
}

async fn run_stage<F>(
    entry: &LlmEntry,
    stage: Stage,
    future: F,
) -> std::result::Result<LlmAction, LlmChainError>
where
    F: std::future::Future<Output = LlmPluginResult>,
{
    let started = Instant::now();
    let (result, failure) = match tokio::time::timeout(entry.timeout, future).await {
        Ok(Ok(action)) => (Ok(action), None),
        Ok(Err(error)) => (handle_plugin_error(entry, error), Some(StageFailure::Error)),
        Err(_) => (timeout_error(entry), Some(StageFailure::Timeout)),
    };
    let rejected = matches!(&result, Ok(LlmAction::Reject(_)));
    let fail_open = failure.is_some() && result.is_ok();
    entry
        .stats
        .record(stage, started.elapsed(), failure, fail_open, rejected);
    result
}

async fn run_stream_stage<F>(
    entry: &LlmEntry,
    future: F,
) -> std::result::Result<LlmStreamAction, LlmChainError>
where
    F: std::future::Future<Output = LlmStreamResult>,
{
    let started = Instant::now();
    let mut failure = None;
    let result = match tokio::time::timeout(entry.timeout, future).await {
        Ok(Ok(action)) => Ok(action),
        Ok(Err(error)) => {
            failure = Some(StageFailure::Error);
            match handle_plugin_error(entry, error) {
                Ok(LlmAction::Continue) => Ok(LlmStreamAction::Continue),
                Ok(LlmAction::Reject(_)) => unreachable!("stream errors never reject this way"),
                Err(error) => Err(error),
            }
        }
        Err(_) => {
            failure = Some(StageFailure::Timeout);
            match timeout_error(entry) {
                Ok(LlmAction::Continue) => Ok(LlmStreamAction::Continue),
                Ok(LlmAction::Reject(_)) => unreachable!("stream timeouts never reject this way"),
                Err(error) => Err(error),
            }
        }
    };
    let fail_open = failure.is_some() && result.is_ok();
    let rejected = matches!(&result, Ok(LlmStreamAction::Reject(_)));
    entry.stats.record(
        Stage::StreamEvent,
        started.elapsed(),
        failure,
        fail_open,
        rejected,
    );
    result
}

fn handle_plugin_error(
    entry: &LlmEntry,
    error: PluginError,
) -> std::result::Result<LlmAction, LlmChainError> {
    let closed = entry.fail_policy == FailPolicy::Closed || error.fail_policy == FailPolicy::Closed;
    if closed {
        Err(LlmChainError::Plugin(error))
    } else {
        tracing::warn!(plugin = %entry.name, error = %error, "LLM plugin failed; continuing");
        Ok(LlmAction::Continue)
    }
}

fn timeout_error(entry: &LlmEntry) -> std::result::Result<LlmAction, LlmChainError> {
    let error = PluginError::fail_closed(format!("LLM plugin '{}' timed out", entry.name));
    if entry.fail_policy == FailPolicy::Closed {
        Err(LlmChainError::Plugin(error))
    } else {
        tracing::warn!(plugin = %entry.name, "LLM plugin timed out; continuing");
        Ok(LlmAction::Continue)
    }
}

#[derive(Default)]
struct LlmPluginChains {
    by_scope: HashMap<ScopeKey, Vec<LlmEntry>>,
    cache: Mutex<HashMap<ChainKey, Arc<LlmPluginChain>>>,
    empty: Arc<LlmPluginChain>,
    has_any: bool,
}

impl LlmPluginChains {
    #[cfg(test)]
    fn compile(data: PluginData, registry: &LlmPluginRegistry) -> Result<Self> {
        Self::compile_with(data, registry, &LlmPluginMetrics::default())
    }

    fn compile_with(
        data: PluginData,
        registry: &LlmPluginRegistry,
        metrics: &LlmPluginMetrics,
    ) -> Result<Self> {
        let plugins: HashMap<&str, &PluginRecord> = data
            .plugins
            .iter()
            .filter(|plugin| plugin.kind == "llm")
            .map(|plugin| (plugin.id.as_str(), plugin))
            .collect();
        let mut by_scope: HashMap<ScopeKey, Vec<LlmEntry>> = HashMap::new();
        for config in &data.configs {
            if let Some((key, entry)) = compile_config(config, &plugins, registry, metrics)? {
                by_scope.entry(key).or_default().push(entry);
            }
        }
        for entries in by_scope.values_mut() {
            sort_entries(entries);
        }
        Ok(Self {
            has_any: !by_scope.is_empty(),
            by_scope,
            cache: Mutex::new(HashMap::new()),
            empty: Arc::new(LlmPluginChain::default()),
        })
    }

    fn chain_for(
        &self,
        consumer_id: Option<&str>,
        virtual_key_id: Option<&str>,
        model: &str,
        provider_id: Option<&str>,
    ) -> Arc<LlmPluginChain> {
        if !self.has_any {
            return self.empty.clone();
        }
        let key = (
            consumer_id.map(str::to_string),
            virtual_key_id.map(str::to_string),
            model.to_string(),
            provider_id.map(str::to_string),
        );
        if let Some(chain) = self.cache.lock().get(&key) {
            return chain.clone();
        }

        let mut seen = HashSet::new();
        let mut entries = Vec::new();
        let mut push = |scope: LlmScope, scope_id: &str| {
            let Some(list) = self.by_scope.get(&(scope, scope_id.to_string())) else {
                return;
            };
            for entry in list {
                if seen.insert(entry.name.clone()) {
                    entries.push(entry.clone());
                }
            }
        };

        if let Some(consumer_id) = consumer_id {
            push(LlmScope::Consumer, consumer_id);
        }
        if let Some(virtual_key_id) = virtual_key_id {
            push(LlmScope::VirtualKey, virtual_key_id);
        }
        push(LlmScope::Model, model);
        if let Some(provider_id) = provider_id {
            push(LlmScope::Provider, provider_id);
        }
        push(LlmScope::Global, "");

        sort_entries(&mut entries);
        let chain = Arc::new(LlmPluginChain { entries });
        self.cache.lock().insert(key, chain.clone());
        chain
    }
}

fn sort_entries(entries: &mut [LlmEntry]) {
    entries.sort_by(|left, right| {
        right
            .priority
            .cmp(&left.priority)
            .then_with(|| left.name.cmp(&right.name))
    });
}

/// Resolves `secret_ref` properties in place and stores the value under a
/// sibling `secret` property. Other strings are never touched, and the control
/// plane keeps returning the original, unresolved configuration.
fn resolve_secret_refs(value: &mut Value) -> std::result::Result<(), PluginError> {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(reference)) = map.get("secret_ref") {
                let resolved = resolve_secret_reference(reference)?;
                map.insert("secret".to_string(), Value::String(resolved));
            }
            for child in map.values_mut() {
                resolve_secret_refs(child)?;
            }
        }
        Value::Array(items) => {
            for item in items {
                resolve_secret_refs(item)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn resolve_secret_reference(reference: &str) -> std::result::Result<String, PluginError> {
    let resolved = if let Some(var) = reference.strip_prefix("env:") {
        let var = var.trim();
        if var.is_empty() {
            return Err(PluginError::fail_closed(
                "secret_ref env: requires a variable name",
            ));
        }
        std::env::var(var).map_err(|_| {
            PluginError::fail_closed(format!("environment variable {var} is not set"))
        })?
    } else if let Some(path) = reference.strip_prefix("file:") {
        let path = std::path::Path::new(path.trim());
        if !path.is_absolute() {
            return Err(PluginError::fail_closed(
                "secret_ref file: requires an absolute path",
            ));
        }
        std::fs::read_to_string(path).map_err(|error| {
            PluginError::fail_closed(format!(
                "cannot read secret_ref file {}: {error}",
                path.display()
            ))
        })?
    } else {
        return Err(PluginError::fail_closed(format!(
            "unsupported secret_ref '{reference}': use env:VAR or file:/absolute/path"
        )));
    };
    let resolved = resolved.trim();
    if resolved.is_empty() {
        return Err(PluginError::fail_closed(
            "secret_ref resolved to an empty value",
        ));
    }
    Ok(resolved.to_string())
}

fn compile_config(
    config: &PluginConfigRecord,
    plugins: &HashMap<&str, &PluginRecord>,
    registry: &LlmPluginRegistry,
    metrics: &LlmPluginMetrics,
) -> Result<Option<(ScopeKey, LlmEntry)>> {
    if !config.enabled {
        return Ok(None);
    }
    let Some(plugin) = plugins.get(config.plugin_id.as_str()) else {
        return Ok(None);
    };
    if !plugin.enabled {
        return Ok(None);
    }
    let Some(factory) = registry.factory(&plugin.name) else {
        tracing::warn!(plugin = %plugin.name, "LLM plugin is not registered; skipping config");
        return Ok(None);
    };
    let mut value: Value = serde_json::from_str(&config.config_json).unwrap_or(Value::Null);
    resolve_secret_refs(&mut value)
        .map_err(|error| Error::Runtime(format!("LLM plugin '{}': {error}", plugin.name)))?;
    let instance = factory
        .create(value)
        .map_err(|error| Error::Runtime(format!("LLM plugin '{}': {error}", plugin.name)))?;
    let scope = LlmScope::parse(&config.scope)
        .ok_or_else(|| Error::Runtime(format!("unknown LLM plugin scope '{}'", config.scope)))?;
    if scope != LlmScope::Global && config.scope_id.trim().is_empty() {
        return Err(Error::Runtime(format!(
            "LLM plugin '{}': {scope} scope requires a scope id",
            plugin.name
        )));
    }
    let manifest = instance.manifest();
    Ok(Some((
        (scope, config.scope_id.clone()),
        LlmEntry {
            name: plugin.name.clone(),
            scope,
            scope_id: config.scope_id.clone(),
            priority: plugin.priority,
            timeout: manifest.timeout(),
            fail_policy: manifest.fail_policy,
            stats: metrics.stats(&plugin.name),
            plugin: instance,
        },
    )))
}

pub struct LlmPluginRuntime {
    storage: Option<Arc<Storage>>,
    registry: Arc<LlmPluginRegistry>,
    chains: RwLock<Arc<LlmPluginChains>>,
    metrics: LlmPluginMetrics,
}

impl Default for LlmPluginRuntime {
    fn default() -> Self {
        Self::empty()
    }
}

impl LlmPluginRuntime {
    pub fn empty() -> Self {
        Self {
            storage: None,
            registry: Arc::new(LlmPluginRegistry::default()),
            chains: RwLock::new(Arc::new(LlmPluginChains::default())),
            metrics: LlmPluginMetrics::default(),
        }
    }

    pub async fn bootstrap(
        storage: Arc<Storage>,
        registry: Arc<LlmPluginRegistry>,
    ) -> Result<Self> {
        for manifest in registry.manifests() {
            storage.llm.upsert_plugin(&manifest).await?;
        }
        let runtime = Self {
            storage: Some(storage),
            registry,
            chains: RwLock::new(Arc::new(LlmPluginChains::default())),
            metrics: LlmPluginMetrics::default(),
        };
        runtime.reload().await?;
        Ok(runtime)
    }

    pub fn registry(&self) -> &Arc<LlmPluginRegistry> {
        &self.registry
    }

    pub async fn reload(&self) -> Result<()> {
        let Some(storage) = &self.storage else {
            return Ok(());
        };
        let data = storage.llm.load_plugin_data().await?;
        let chains = LlmPluginChains::compile_with(data, &self.registry, &self.metrics)?;
        for manifest in self.registry.manifests() {
            self.metrics.stats(&manifest.name);
        }
        *self.chains.write() = Arc::new(chains);
        Ok(())
    }

    /// Chain for the client-facing request and response stages.
    pub fn request_chain(
        &self,
        consumer_id: Option<&str>,
        virtual_key_id: Option<&str>,
        model: &str,
    ) -> Arc<LlmPluginChain> {
        self.chains
            .read()
            .chain_for(consumer_id, virtual_key_id, model, None)
    }

    /// Chain including the provider scope, used once a target is selected.
    pub fn provider_chain(
        &self,
        consumer_id: Option<&str>,
        virtual_key_id: Option<&str>,
        model: &str,
        provider_id: &str,
    ) -> Arc<LlmPluginChain> {
        self.chains
            .read()
            .chain_for(consumer_id, virtual_key_id, model, Some(provider_id))
    }

    pub fn has_plugins(&self) -> bool {
        self.chains.read().has_any
    }

    pub fn metrics(&self) -> Vec<PluginMetricsView> {
        self.metrics.snapshot()
    }

    /// Execution-order chain for a caller, including each entry's scope.
    pub fn effective_chain(
        &self,
        consumer_id: Option<&str>,
        virtual_key_id: Option<&str>,
        model: &str,
        provider_id: Option<&str>,
    ) -> Vec<LlmChainEntry> {
        self.chains
            .read()
            .chain_for(consumer_id, virtual_key_id, model, provider_id)
            .describe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use octra_api_plugin_types::{Capability, PluginKind, PluginRuntimeKind};
    use octra_llm_core::{ChatRequest, ChatResponse, FinishReason, Protocol, Usage};
    use octra_llm_plugin_types::LlmPluginResult;

    struct MarkPlugin {
        manifest: PluginManifest,
        mark: &'static str,
    }

    #[async_trait]
    impl LlmPlugin for MarkPlugin {
        fn manifest(&self) -> &PluginManifest {
            &self.manifest
        }

        async fn on_request(&self, cx: &mut LlmRequestContext<'_>) -> LlmPluginResult {
            cx.request
                .extra
                .insert(self.mark.to_string(), Value::Bool(true));
            Ok(LlmAction::Continue)
        }
    }

    struct MarkFactory(MarkPlugin);

    impl LlmPluginFactory for MarkFactory {
        fn manifest(&self) -> PluginManifest {
            self.0.manifest.clone()
        }

        fn create(&self, _config: Value) -> std::result::Result<Arc<dyn LlmPlugin>, PluginError> {
            Ok(Arc::new(MarkPlugin {
                manifest: self.0.manifest.clone(),
                mark: self.0.mark,
            }))
        }
    }

    fn manifest(name: &str) -> PluginManifest {
        PluginManifest {
            name: name.to_string(),
            version: "0".into(),
            kind: PluginKind::Llm,
            runtime: PluginRuntimeKind::Builtin,
            priority: 0,
            description: String::new(),
            timeout_ms: 100,
            fail_policy: FailPolicy::Open,
            capabilities: Vec::<Capability>::new(),
            config_schema: None,
        }
    }

    fn plugin(name: &str, id: &str, kind: &str, enabled: bool) -> PluginRecord {
        PluginRecord {
            id: id.into(),
            name: name.into(),
            version: "0".into(),
            kind: kind.into(),
            runtime_type: "builtin".into(),
            enabled,
            priority: 0,
            description: String::new(),
            manifest_json: "{}".into(),
            config_schema_json: "{}".into(),
            artifact_path: None,
            artifact_hash: None,
            updated_at: String::new(),
        }
    }

    fn config(id: &str, plugin_id: &str, scope: &str, scope_id: &str) -> PluginConfigRecord {
        PluginConfigRecord {
            id: id.into(),
            plugin_id: plugin_id.into(),
            scope: scope.into(),
            scope_id: scope_id.into(),
            enabled: true,
            config_json: "{}".into(),
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    fn registry() -> LlmPluginRegistry {
        let mut registry = LlmPluginRegistry::new();
        registry.register(MarkFactory(MarkPlugin {
            manifest: manifest("mark"),
            mark: "marked",
        }));
        registry.register(MarkFactory(MarkPlugin {
            manifest: manifest("other"),
            mark: "other",
        }));
        registry
    }

    #[tokio::test]
    async fn consumer_scope_overrides_global_and_api_plugins_are_ignored() {
        let mut registry = registry();
        registry.register(MarkFactory(MarkPlugin {
            manifest: manifest("api-only"),
            mark: "api",
        }));
        let data = PluginData {
            plugins: vec![
                plugin("mark", "p1", "llm", true),
                plugin("api-only", "p2", "api", true),
            ],
            configs: vec![
                config("c1", "p1", "global", ""),
                config("c2", "p1", "consumer", "consumer-1"),
                config("c3", "p2", "global", ""),
            ],
        };
        let chains = LlmPluginChains::compile(data, &registry).unwrap();
        // The global config wins for unbound callers; the consumer config
        // replaces it rather than running twice.
        let chain = chains.chain_for(None, None, "m", None);
        assert_eq!(chain.names().collect::<Vec<_>>(), vec!["mark"]);
        let chain = chains.chain_for(Some("consumer-1"), None, "m", None);
        assert_eq!(chain.names().collect::<Vec<_>>(), vec!["mark"]);
    }

    #[tokio::test]
    async fn request_stage_mutates_the_request() {
        let data = PluginData {
            plugins: vec![plugin("mark", "p1", "llm", true)],
            configs: vec![config("c1", "p1", "global", "")],
        };
        let chains = LlmPluginChains::compile(data, &registry()).unwrap();
        let chain = chains.chain_for(None, None, "m", None);
        let mut request = ChatRequest::new("m", Vec::new());
        let meta = LlmRequestMeta {
            request_id: "r",
            ingress: Protocol::Openai,
            consumer_id: None,
            virtual_key_id: None,
            session_id: None,
            client_ip: None,
            user_agent: None,
            intent: None,
        };
        let mut state = LlmRequestState::new();
        assert!(
            chain
                .on_request(&meta, &mut request, &mut state)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(request.extra.get("marked"), Some(&Value::Bool(true)));
    }

    #[tokio::test]
    async fn stream_stage_can_replace_events() {
        struct SplitPlugin {
            manifest: PluginManifest,
        }

        #[async_trait]
        impl LlmPlugin for SplitPlugin {
            fn manifest(&self) -> &PluginManifest {
                &self.manifest
            }

            async fn on_stream_event(&self, cx: &mut LlmStreamContext<'_>) -> LlmStreamResult {
                match cx.event {
                    StreamEvent::TextDelta { text } => Ok(LlmStreamAction::Replace(vec![
                        StreamEvent::TextDelta { text: text.clone() },
                        StreamEvent::TextDelta {
                            text: "!".to_string(),
                        },
                    ])),
                    _ => Ok(LlmStreamAction::Continue),
                }
            }
        }

        struct SplitFactory(SplitPlugin);

        impl LlmPluginFactory for SplitFactory {
            fn manifest(&self) -> PluginManifest {
                self.0.manifest.clone()
            }

            fn create(
                &self,
                _config: Value,
            ) -> std::result::Result<Arc<dyn LlmPlugin>, PluginError> {
                Ok(Arc::new(SplitPlugin {
                    manifest: self.0.manifest.clone(),
                }))
            }
        }

        let mut registry = LlmPluginRegistry::new();
        registry.register(SplitFactory(SplitPlugin {
            manifest: manifest("split"),
        }));
        let data = PluginData {
            plugins: vec![plugin("split", "p1", "llm", true)],
            configs: vec![config("c1", "p1", "global", "")],
        };
        let chains = LlmPluginChains::compile(data, &registry).unwrap();
        let chain = chains.chain_for(None, None, "m", None);
        let meta = LlmRequestMeta {
            request_id: "r",
            ingress: Protocol::Openai,
            consumer_id: None,
            virtual_key_id: None,
            session_id: None,
            client_ip: None,
            user_agent: None,
            intent: None,
        };
        let mut state = LlmRequestState::new();
        let events = chain
            .on_stream_events(
                &meta,
                vec![StreamEvent::TextDelta { text: "hi".into() }],
                &mut state,
            )
            .await
            .unwrap();
        assert_eq!(events.len(), 2);
    }

    struct Recorder {
        manifest: PluginManifest,
        log: Arc<parking_lot::Mutex<Vec<String>>>,
        name: &'static str,
    }

    #[async_trait]
    impl LlmPlugin for Recorder {
        fn manifest(&self) -> &PluginManifest {
            &self.manifest
        }

        async fn on_request(&self, _cx: &mut LlmRequestContext<'_>) -> LlmPluginResult {
            self.log.lock().push(format!("request:{}", self.name));
            Ok(LlmAction::Continue)
        }

        async fn on_response(&self, _cx: &mut LlmResponseContext<'_>) -> LlmPluginResult {
            self.log.lock().push(format!("response:{}", self.name));
            Ok(LlmAction::Continue)
        }
    }

    struct RecorderFactory {
        manifest: PluginManifest,
        log: Arc<parking_lot::Mutex<Vec<String>>>,
        name: &'static str,
    }

    impl LlmPluginFactory for RecorderFactory {
        fn manifest(&self) -> PluginManifest {
            self.manifest.clone()
        }

        fn create(&self, _config: Value) -> std::result::Result<Arc<dyn LlmPlugin>, PluginError> {
            Ok(Arc::new(Recorder {
                manifest: self.manifest.clone(),
                log: self.log.clone(),
                name: self.name,
            }))
        }
    }

    #[tokio::test]
    async fn response_stage_unwinds_in_reverse() {
        let log = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let mut registry = LlmPluginRegistry::new();
        registry.register(RecorderFactory {
            manifest: manifest("alpha"),
            log: log.clone(),
            name: "alpha",
        });
        registry.register(RecorderFactory {
            manifest: manifest("beta"),
            log: log.clone(),
            name: "beta",
        });
        let data = PluginData {
            plugins: vec![
                plugin("alpha", "p1", "llm", true),
                plugin("beta", "p2", "llm", true),
            ],
            configs: vec![
                config("c1", "p1", "global", ""),
                config("c2", "p2", "global", ""),
            ],
        };
        let chains = LlmPluginChains::compile(data, &registry).unwrap();
        let chain = chains.chain_for(None, None, "m", None);
        let meta = LlmRequestMeta {
            request_id: "r",
            ingress: Protocol::Openai,
            consumer_id: None,
            virtual_key_id: None,
            session_id: None,
            client_ip: None,
            user_agent: None,
            intent: None,
        };
        let mut request = ChatRequest::new("m", Vec::new());
        let mut state = LlmRequestState::new();
        chain
            .on_request(&meta, &mut request, &mut state)
            .await
            .unwrap();
        let mut response = ChatResponse::new(
            "id",
            "m",
            octra_llm_core::Message::new(octra_llm_core::Role::Assistant),
        );
        let mut usage = Usage::default();
        chain
            .on_response(&meta, "m", &mut response, &mut usage, &mut state)
            .await
            .unwrap();
        assert_eq!(
            *log.lock(),
            vec![
                "request:alpha".to_string(),
                "request:beta".to_string(),
                "response:beta".to_string(),
                "response:alpha".to_string()
            ]
        );
    }

    struct FailPlugin {
        manifest: PluginManifest,
    }

    #[async_trait]
    impl LlmPlugin for FailPlugin {
        fn manifest(&self) -> &PluginManifest {
            &self.manifest
        }

        async fn on_request(&self, _cx: &mut LlmRequestContext<'_>) -> LlmPluginResult {
            Err(PluginError::fail_open("boom"))
        }
    }

    struct FailFactory(PluginManifest);

    impl LlmPluginFactory for FailFactory {
        fn manifest(&self) -> PluginManifest {
            self.0.clone()
        }

        fn create(&self, _config: Value) -> std::result::Result<Arc<dyn LlmPlugin>, PluginError> {
            Ok(Arc::new(FailPlugin {
                manifest: self.0.clone(),
            }))
        }
    }

    fn chain_with_failure(policy: FailPolicy) -> Arc<LlmPluginChain> {
        let mut registry = LlmPluginRegistry::new();
        let mut entry = manifest("failing");
        entry.fail_policy = policy;
        registry.register(FailFactory(entry));
        let data = PluginData {
            plugins: vec![plugin("failing", "p1", "llm", true)],
            configs: vec![config("c1", "p1", "global", "")],
        };
        LlmPluginChains::compile(data, &registry)
            .unwrap()
            .chain_for(None, None, "m", None)
    }

    #[tokio::test]
    async fn manifest_fail_policy_controls_timeouts_and_errors() {
        let meta = LlmRequestMeta {
            request_id: "r",
            ingress: Protocol::Openai,
            consumer_id: None,
            virtual_key_id: None,
            session_id: None,
            client_ip: None,
            user_agent: None,
            intent: None,
        };
        let mut request = ChatRequest::new("m", Vec::new());
        let mut state = LlmRequestState::new();
        let open = chain_with_failure(FailPolicy::Open);
        assert!(
            open.on_request(&meta, &mut request, &mut state)
                .await
                .unwrap()
                .is_none()
        );

        let closed = chain_with_failure(FailPolicy::Closed);
        assert!(matches!(
            closed.on_request(&meta, &mut request, &mut state).await,
            Err(LlmChainError::Plugin(_))
        ));
    }

    struct ExpandPlugin {
        manifest: PluginManifest,
    }

    #[async_trait]
    impl LlmPlugin for ExpandPlugin {
        fn manifest(&self) -> &PluginManifest {
            &self.manifest
        }

        async fn on_stream_event(&self, _cx: &mut LlmStreamContext<'_>) -> LlmStreamResult {
            Ok(LlmStreamAction::Replace(
                (0..MAX_STREAM_EVENTS + 1)
                    .map(|_| StreamEvent::TextDelta {
                        text: "x".to_string(),
                    })
                    .collect(),
            ))
        }
    }

    struct ExpandFactory;

    impl LlmPluginFactory for ExpandFactory {
        fn manifest(&self) -> PluginManifest {
            manifest("expand")
        }

        fn create(&self, _config: Value) -> std::result::Result<Arc<dyn LlmPlugin>, PluginError> {
            Ok(Arc::new(ExpandPlugin {
                manifest: manifest("expand"),
            }))
        }
    }

    #[tokio::test]
    async fn stream_expansion_is_bounded() {
        let mut registry = LlmPluginRegistry::new();
        registry.register(ExpandFactory);
        let data = PluginData {
            plugins: vec![plugin("expand", "p1", "llm", true)],
            configs: vec![config("c1", "p1", "global", "")],
        };
        let chains = LlmPluginChains::compile(data, &registry).unwrap();
        let chain = chains.chain_for(None, None, "m", None);
        let meta = LlmRequestMeta {
            request_id: "r",
            ingress: Protocol::Openai,
            consumer_id: None,
            virtual_key_id: None,
            session_id: None,
            client_ip: None,
            user_agent: None,
            intent: None,
        };
        let mut state = LlmRequestState::new();
        assert!(matches!(
            chain
                .on_stream_events(
                    &meta,
                    vec![StreamEvent::TextDelta { text: "hi".into() }],
                    &mut state
                )
                .await,
            Err(LlmChainError::Plugin(_))
        ));
    }

    fn test_meta() -> LlmRequestMeta<'static> {
        LlmRequestMeta {
            request_id: "r",
            ingress: Protocol::Openai,
            consumer_id: None,
            virtual_key_id: None,
            session_id: None,
            client_ip: None,
            user_agent: None,
            intent: None,
        }
    }

    #[tokio::test]
    async fn metrics_count_calls_failures_and_fail_open() {
        let mut registry = LlmPluginRegistry::new();
        registry.register(FailFactory(manifest("failing")));
        let data = PluginData {
            plugins: vec![plugin("failing", "p1", "llm", true)],
            configs: vec![config("c1", "p1", "global", "")],
        };
        let metrics = LlmPluginMetrics::default();
        let chains = LlmPluginChains::compile_with(data, &registry, &metrics).unwrap();
        let chain = chains.chain_for(None, None, "m", None);
        let mut request = ChatRequest::new("m", Vec::new());
        let mut state = LlmRequestState::new();
        chain
            .on_request(&test_meta(), &mut request, &mut state)
            .await
            .unwrap();

        let snapshot = metrics.snapshot();
        let view = snapshot
            .iter()
            .find(|view| view.name == "failing")
            .expect("metrics for the configured plugin");
        assert_eq!(view.stages["request"].calls, 1);
        assert_eq!(view.stages["request"].failures, 1);
        assert_eq!(view.stages["request"].fail_open, 1);
        assert_eq!(view.stages["request"].rejections, 0);
        assert_eq!(view.stages["response"].calls, 0);
    }

    #[tokio::test]
    async fn effective_chain_reports_scope_and_order() {
        let data = PluginData {
            plugins: vec![plugin("mark", "p1", "llm", true)],
            configs: vec![
                config("c1", "p1", "global", ""),
                config("c2", "p1", "consumer", "consumer-1"),
            ],
        };
        let chains = LlmPluginChains::compile(data, &registry()).unwrap();
        let described = chains
            .chain_for(Some("consumer-1"), None, "m", None)
            .describe();
        assert_eq!(described.len(), 1);
        assert_eq!(described[0].name, "mark");
        assert_eq!(described[0].scope, LlmScope::Consumer);
        assert_eq!(described[0].scope_id, "consumer-1");
    }

    #[test]
    fn incompatible_api_versions_are_not_registered() {
        struct OldFactory(PluginManifest);

        impl LlmPluginFactory for OldFactory {
            fn manifest(&self) -> PluginManifest {
                self.0.clone()
            }

            fn api_version(&self) -> u32 {
                LLM_PLUGIN_API_VERSION + 1
            }

            fn create(
                &self,
                _config: Value,
            ) -> std::result::Result<Arc<dyn LlmPlugin>, PluginError> {
                Ok(Arc::new(FailPlugin {
                    manifest: self.0.clone(),
                }))
            }
        }

        let mut registry = LlmPluginRegistry::new();
        registry.register(OldFactory(manifest("old")));
        assert!(registry.factory("old").is_none());
        assert!(registry.manifests().is_empty());
    }

    #[test]
    fn secret_refs_resolve_files_and_reject_unknown_schemes() {
        let mut value = serde_json::json!({"plain": "kept"});
        resolve_secret_refs(&mut value).unwrap();
        assert_eq!(value["plain"], "kept");

        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "octra-plugin-secret-{}-{nanos}",
            std::process::id()
        ));
        std::fs::write(&path, "  sk-test-123  \n").unwrap();
        let mut value = serde_json::json!({
            "auth": {"secret_ref": format!("file:{}", path.display())}
        });
        resolve_secret_refs(&mut value).unwrap();
        assert_eq!(value["auth"]["secret"], "sk-test-123");
        std::fs::remove_file(&path).ok();

        let mut bad = serde_json::json!({"secret_ref": "plain-value"});
        assert!(resolve_secret_refs(&mut bad).is_err());
        let mut relative = serde_json::json!({"secret_ref": "file:relative.txt"});
        assert!(resolve_secret_refs(&mut relative).is_err());
    }

    #[test]
    fn response_and_error_types_are_available() {
        // Keeps the imported stage types exercised at compile time.
        let _ = ChatResponse {
            id: String::new(),
            model: String::new(),
            message: octra_llm_core::Message::new(octra_llm_core::Role::Assistant),
            finish_reason: FinishReason::Stop,
            usage: Usage::default(),
            reasoning: None,
            programmatic: None,
            extra: Default::default(),
        };
    }
}
