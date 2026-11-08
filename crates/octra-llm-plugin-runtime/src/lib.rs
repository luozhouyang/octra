//! LLM plugin registry, scope resolution and ordered execution.
//!
//! Chains are compiled from the shared plugin catalog once per control-plane
//! mutation. The data plane only performs map lookups and in-memory calls, so
//! configuration changes never parse JSON on the request path.

// Rejections carry the full gateway error, as in `octra-llm-gateway`; boxing
// would only obscure the data-plane flow.
#![allow(clippy::result_large_err)]

mod counter_store;

pub use counter_store::StorageCounterStore;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use octra_api_plugin_types::{Capability, FailPolicy, PluginError, PluginManifest};
use octra_core::{Error, Result};
use octra_llm_core::{ChatResponse, GatewayError, StreamEvent};
use octra_llm_plugin_types::{
    LLM_PLUGIN_API_VERSION, LlmAction, LlmCounterStore, LlmErrorContext, LlmFinalResponseContext,
    LlmLogContext, LlmLogEvent, LlmPlugin, LlmPluginCreateContext, LlmPluginFactory,
    LlmPluginResult, LlmPluginStateStore, LlmProviderInfo, LlmProviderRequestContext,
    LlmRequestContext, LlmRequestMeta, LlmRequestState, LlmResponseContext, LlmResponseHeaders,
    LlmScope, LlmStreamAction, LlmStreamContext, LlmStreamResult, MemoryCounterStore,
};
use octra_storage::Storage;
use octra_storage::plugins::{PluginConfigRecord, PluginData, PluginRecord};
use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use serde_json::Value;

#[derive(Default)]
pub struct LlmPluginRegistry {
    /// Shared catalog. Dynamic installers (WASM artifacts) register here at
    /// runtime; chain compilation reads a snapshot on the next reload.
    factories: RwLock<HashMap<String, Arc<dyn LlmPluginFactory>>>,
}

impl LlmPluginRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register<F>(&self, factory: F)
    where
        F: LlmPluginFactory + 'static,
    {
        self.register_arc(Arc::new(factory));
    }

    pub fn register_arc(&self, factory: Arc<dyn LlmPluginFactory>) {
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
        self.factories.write().insert(manifest.name, factory);
    }

    /// Removes a dynamically registered plugin. Returns the factory so a
    /// failed reload can restore it.
    pub fn remove(&self, name: &str) -> Option<Arc<dyn LlmPluginFactory>> {
        self.factories.write().remove(name)
    }

    pub fn manifests(&self) -> Vec<PluginManifest> {
        let mut manifests: Vec<_> = self
            .factories
            .read()
            .values()
            .map(|factory| factory.manifest())
            .collect();
        manifests.sort_by(|left, right| left.name.cmp(&right.name));
        manifests
    }

    pub fn factory(&self, name: &str) -> Option<Arc<dyn LlmPluginFactory>> {
        self.factories.read().get(name).cloned()
    }

    pub fn validate(&self, name: &str, config: &Value) -> std::result::Result<(), PluginError> {
        self.factory(name)
            .ok_or_else(|| PluginError::fail_closed(format!("unknown LLM plugin '{name}'")))?
            .validate(config)
    }

    /// Scopes the named plugin can honour. Unknown plugins report no scopes.
    pub fn supported_scopes(&self, name: &str) -> Vec<LlmScope> {
        self.factory(name)
            .map(|factory| factory.supported_scopes().to_vec())
            .unwrap_or_default()
    }

    pub fn supports_scope(&self, name: &str, scope: LlmScope) -> bool {
        self.factory(name)
            .is_some_and(|factory| factory.supported_scopes().contains(&scope))
    }
}

/// Stages tracked by the metrics counters, in snapshot order.
const STAGE_COUNT: usize = 7;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Request,
    ProviderRequest,
    Response,
    StreamEvent,
    Error,
    Log,
    FinalResponse,
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
            Self::FinalResponse => 6,
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
            Self::FinalResponse => "final_response",
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum StageFailure {
    Error,
    Timeout,
}

/// Number of buckets in the per-stage latency histogram.
const LATENCY_BUCKETS: usize = 24;

/// Bucket upper bounds in microseconds, ascending. A sample is counted in the
/// first bucket whose bound is greater than or equal to its duration. The final
/// entry is the `+Inf` bucket, which also catches samples above 120 seconds.
const LATENCY_BOUNDS_MICROS: [u64; LATENCY_BUCKETS] = [
    0,
    10,
    25,
    50,
    100,
    250,
    500,
    1_000,
    2_500,
    5_000,
    10_000,
    25_000,
    50_000,
    100_000,
    250_000,
    500_000,
    1_000_000,
    2_500_000,
    5_000_000,
    10_000_000,
    30_000_000,
    60_000_000,
    120_000_000,
    u64::MAX,
];

/// Index of the first bucket whose upper bound is at least `micros`.
fn latency_bucket_index(micros: u64) -> usize {
    LATENCY_BOUNDS_MICROS
        .iter()
        .position(|&bound| micros <= bound)
        .unwrap_or(LATENCY_BUCKETS - 1)
}

/// Nearest-rank percentile over cumulative bucket counts. Returns the upper
/// bound of the bucket holding the requested rank, or 0 when there are no
/// samples.
fn latency_percentile_micros(calls: u64, buckets: &[u64; LATENCY_BUCKETS], percentile: u64) -> u64 {
    if calls == 0 {
        return 0;
    }
    // ceil(percentile / 100 * calls), clamped to the sample range.
    let rank = ((u128::from(percentile) * u128::from(calls)).div_ceil(100) as u64).clamp(1, calls);
    let mut cumulative = 0u64;
    for (index, count) in buckets.iter().enumerate() {
        cumulative = cumulative.saturating_add(*count);
        if cumulative >= rank {
            return LATENCY_BOUNDS_MICROS[index];
        }
    }
    LATENCY_BOUNDS_MICROS[LATENCY_BUCKETS - 1]
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
    latency_buckets: [AtomicU64; LATENCY_BUCKETS],
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
        let bucket = latency_bucket_index(micros);
        stats.latency_buckets[bucket].fetch_add(1, Ordering::Relaxed);
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

/// Identity of one metrics entry. Configured instances are keyed by config id,
/// while catalog entries only describe a registered plugin.
#[derive(Debug, Clone)]
struct PluginIdentity {
    name: String,
    config_id: String,
    scope: LlmScope,
    scope_id: String,
}

struct MetricsEntry {
    identity: PluginIdentity,
    stats: Arc<PluginStats>,
}

/// Shared per-instance counters. Configured entries clone the `Arc`, so counters
/// survive a chain reload and in-flight requests keep updating them.
#[derive(Clone, Default)]
pub struct LlmPluginMetrics {
    plugins: Arc<Mutex<HashMap<String, Arc<MetricsEntry>>>>,
}

impl LlmPluginMetrics {
    /// Stats for a configured instance, keyed by its unique config id. Two
    /// configs of the same plugin at different scopes stay distinguishable.
    fn stats(
        &self,
        config_id: &str,
        name: &str,
        scope: LlmScope,
        scope_id: &str,
    ) -> Arc<PluginStats> {
        let mut plugins = self.plugins.lock();
        plugins
            .entry(config_id.to_string())
            .or_insert_with(|| {
                Arc::new(MetricsEntry {
                    identity: PluginIdentity {
                        name: name.to_string(),
                        config_id: config_id.to_string(),
                        scope,
                        scope_id: scope_id.to_string(),
                    },
                    stats: Arc::new(PluginStats::default()),
                })
            })
            .stats
            .clone()
    }

    /// Removes counters for configured instances that are no longer present.
    /// Catalog entries (empty config id) are kept and handled by
    /// [`Self::seed_if_missing`].
    fn retain_configs(&self, keep: &HashSet<String>) {
        self.plugins.lock().retain(|_, entry| {
            entry.identity.config_id.is_empty() || keep.contains(&entry.identity.config_id)
        });
    }

    /// Adds a zero-state entry for a registered plugin that has no
    /// configuration, so the snapshot still advertises it. A configured
    /// instance replaces any catalog placeholder for the same name.
    fn seed_if_missing(&self, name: &str) {
        let mut plugins = self.plugins.lock();
        let configured = plugins
            .values()
            .any(|entry| entry.identity.name == name && !entry.identity.config_id.is_empty());
        if configured {
            plugins.retain(|_, entry| {
                !(entry.identity.name == name && entry.identity.config_id.is_empty())
            });
            return;
        }
        if plugins.values().any(|entry| entry.identity.name == name) {
            return;
        }
        plugins.insert(
            format!("catalog:{name}"),
            Arc::new(MetricsEntry {
                identity: PluginIdentity {
                    name: name.to_string(),
                    config_id: String::new(),
                    scope: LlmScope::Global,
                    scope_id: String::new(),
                },
                stats: Arc::new(PluginStats::default()),
            }),
        );
    }

    pub fn snapshot(&self) -> Vec<PluginMetricsView> {
        let entries: Vec<Arc<MetricsEntry>> = self.plugins.lock().values().cloned().collect();
        let mut views: Vec<_> = entries
            .iter()
            .map(|entry| PluginMetricsView::from_stats(&entry.identity, &entry.stats))
            .collect();
        views.sort_by(|left, right| {
            left.name
                .cmp(&right.name)
                .then_with(|| left.config_id.cmp(&right.config_id))
        });
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
    pub latency_p50_micros: u64,
    pub latency_p95_micros: u64,
    pub latency_p99_micros: u64,
}

#[derive(Debug, Serialize)]
pub struct PluginMetricsView {
    pub name: String,
    pub config_id: String,
    pub scope: LlmScope,
    pub scope_id: String,
    pub stages: BTreeMap<&'static str, StageMetricsView>,
}

impl PluginMetricsView {
    fn from_stats(identity: &PluginIdentity, stats: &PluginStats) -> Self {
        let mut stages = BTreeMap::new();
        for (index, stage) in [
            Stage::Request,
            Stage::ProviderRequest,
            Stage::Response,
            Stage::StreamEvent,
            Stage::Error,
            Stage::Log,
            Stage::FinalResponse,
        ]
        .into_iter()
        .enumerate()
        {
            let counters = &stats.stages[index];
            let calls = counters.calls.load(Ordering::Relaxed);
            // Buckets are read after `calls`: `record` bumps the call counter
            // before its bucket, so the cumulative counts observed here never
            // understate the samples covered by `calls` beyond a single
            // in-flight record.
            let latency_buckets: [u64; LATENCY_BUCKETS] = std::array::from_fn(|bucket| {
                counters.latency_buckets[bucket].load(Ordering::Relaxed)
            });
            stages.insert(
                stage.as_str(),
                StageMetricsView {
                    calls,
                    failures: counters.failures.load(Ordering::Relaxed),
                    timeouts: counters.timeouts.load(Ordering::Relaxed),
                    rejections: counters.rejections.load(Ordering::Relaxed),
                    fail_open: counters.fail_open.load(Ordering::Relaxed),
                    total_micros: counters.total_micros.load(Ordering::Relaxed),
                    max_micros: counters.max_micros.load(Ordering::Relaxed),
                    latency_p50_micros: latency_percentile_micros(calls, &latency_buckets, 50),
                    latency_p95_micros: latency_percentile_micros(calls, &latency_buckets, 95),
                    latency_p99_micros: latency_percentile_micros(calls, &latency_buckets, 99),
                },
            );
        }
        Self {
            name: identity.name.clone(),
            config_id: identity.config_id.clone(),
            scope: identity.scope,
            scope_id: identity.scope_id.clone(),
            stages,
        }
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
    /// A plugin that declares `RequestBody` mutates the canonical request.
    /// Request-stage failures on such plugins are always fatal: continuing
    /// after a timeout could route a half-rewritten prompt upstream.
    mutates_request: bool,
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

/// Result of the client-facing request chain.
///
/// `Respond` carries a short-circuit response built by a plugin, such as a
/// cache hit. Callers that cannot serve one (for example the compatibility
/// [`LlmPluginChain::on_request`] wrapper) treat it as `Continue`.
// The outcome lives for a single request, so keeping the response inline is
// cheaper than an allocation on the common `Continue` path.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum LlmRequestOutcome {
    Continue,
    Respond(ChatResponse),
    Reject(GatewayError),
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

    /// Runs the client-facing request chain and reports a plugin short-circuit
    /// as an outcome.
    ///
    /// Entries run in request order. The first plugin that returns
    /// [`LlmAction::Respond`] stops the chain; later request plugins do not
    /// run, because the request is already answered.
    pub async fn on_request_outcome(
        &self,
        meta: &LlmRequestMeta<'_>,
        headers: &mut LlmResponseHeaders,
        request: &mut octra_llm_core::ChatRequest,
        state: &mut LlmRequestState,
    ) -> std::result::Result<LlmRequestOutcome, LlmChainError> {
        for entry in &self.entries {
            let mut cx = LlmRequestContext {
                meta,
                headers,
                request,
                state: state.slot(&entry.name),
            };
            match run_stage(
                entry,
                Stage::Request,
                entry.plugin.on_request(&mut cx),
                entry.mutates_request,
            )
            .await?
            {
                LlmAction::Continue => {}
                LlmAction::Respond(response) => {
                    return Ok(LlmRequestOutcome::Respond(*response));
                }
                LlmAction::Reject(error) => return Ok(LlmRequestOutcome::Reject(error)),
            }
        }
        Ok(LlmRequestOutcome::Continue)
    }

    /// Compatibility wrapper over [`Self::on_request_outcome`].
    ///
    /// The `Option<GatewayError>` signature cannot represent a short-circuit
    /// response, so [`LlmAction::Respond`] is reported as `Ok(None)`. Callers
    /// that must serve cache hits have to use [`Self::on_request_outcome`].
    pub async fn on_request(
        &self,
        meta: &LlmRequestMeta<'_>,
        headers: &mut LlmResponseHeaders,
        request: &mut octra_llm_core::ChatRequest,
        state: &mut LlmRequestState,
    ) -> std::result::Result<Option<GatewayError>, LlmChainError> {
        match self
            .on_request_outcome(meta, headers, request, state)
            .await?
        {
            LlmRequestOutcome::Continue | LlmRequestOutcome::Respond(_) => Ok(None),
            LlmRequestOutcome::Reject(error) => Ok(Some(error)),
        }
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
                entry.mutates_request,
            )
            .await?
            {
                LlmAction::Continue => {}
                LlmAction::Respond(_) => return Err(respond_outside_request_stage(entry)),
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
            match run_stage(
                entry,
                Stage::Response,
                entry.plugin.on_response(&mut cx),
                false,
            )
            .await?
            {
                LlmAction::Continue => {}
                LlmAction::Respond(_) => return Err(respond_outside_request_stage(entry)),
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
            match run_stage(entry, Stage::Error, entry.plugin.on_error(&mut cx), false).await? {
                LlmAction::Continue => {}
                LlmAction::Respond(_) => return Err(respond_outside_request_stage(entry)),
                LlmAction::Reject(replacement) => {
                    *cx.error = replacement;
                }
            }
        }
        Ok(())
    }

    /// Runs observational final-response hooks in reverse order, after every
    /// response-stage transform and before protocol encoding.
    ///
    /// The hook is best-effort: a timed-out plugin is logged and skipped, and
    /// it cannot change the response.
    pub async fn on_final_response(
        &self,
        meta: &LlmRequestMeta<'_>,
        request_model: &str,
        response: &ChatResponse,
        state: &mut LlmRequestState,
    ) {
        for entry in self.entries.iter().rev() {
            let mut cx = LlmFinalResponseContext {
                meta,
                request_model,
                response,
                state: state.slot(&entry.name),
            };
            let started = Instant::now();
            let timed_out =
                tokio::time::timeout(entry.timeout, entry.plugin.on_final_response(&mut cx))
                    .await
                    .is_err();
            entry.stats.record(
                Stage::FinalResponse,
                started.elapsed(),
                timed_out.then_some(StageFailure::Timeout),
                timed_out,
                false,
            );
            if timed_out {
                tracing::warn!(plugin = %entry.name, "LLM plugin final-response hook timed out");
            }
        }
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
            let timed_out = tokio::time::timeout(entry.timeout, entry.plugin.on_log(&mut cx))
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
    force_closed: bool,
) -> std::result::Result<LlmAction, LlmChainError>
where
    F: std::future::Future<Output = LlmPluginResult>,
{
    let started = Instant::now();
    let (result, failure) = match tokio::time::timeout(entry.timeout, future).await {
        Ok(Ok(action)) => (Ok(action), None),
        Ok(Err(error)) => {
            let result = if force_closed {
                Err(LlmChainError::Plugin(error))
            } else {
                handle_plugin_error(entry, error)
            };
            (result, Some(StageFailure::Error))
        }
        Err(_) => {
            let result = if force_closed {
                Err(LlmChainError::Plugin(PluginError::fail_closed(format!(
                    "LLM plugin '{}' timed out while mutating the request",
                    entry.name
                ))))
            } else {
                timeout_error(entry)
            };
            (result, Some(StageFailure::Timeout))
        }
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
                Ok(LlmAction::Respond(_)) => {
                    unreachable!("stream errors never short-circuit this way")
                }
                Err(error) => Err(error),
            }
        }
        Err(_) => {
            failure = Some(StageFailure::Timeout);
            match timeout_error(entry) {
                Ok(LlmAction::Continue) => Ok(LlmStreamAction::Continue),
                Ok(LlmAction::Reject(_)) => unreachable!("stream timeouts never reject this way"),
                Ok(LlmAction::Respond(_)) => {
                    unreachable!("stream timeouts never short-circuit this way")
                }
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

/// Builds the fail-closed chain error for a plugin that returns
/// [`LlmAction::Respond`] outside the request stage.
fn respond_outside_request_stage(entry: &LlmEntry) -> LlmChainError {
    LlmChainError::Plugin(PluginError::fail_closed(format!(
        "LLM plugin '{}' returned a short-circuit response, which is only valid at the request stage",
        entry.name
    )))
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

/// In-memory plugin state, shared across chain reloads in one process.
#[derive(Debug, Default)]
pub struct MemoryPluginStateStore {
    values: RwLock<HashMap<String, Value>>,
}

impl LlmPluginStateStore for MemoryPluginStateStore {
    fn get(&self, key: &str) -> Option<Value> {
        self.values.read().get(key).cloned()
    }

    fn put(&self, key: &str, value: Value) {
        self.values.write().insert(key.to_string(), value);
    }
}

/// State store that keeps writes in memory and flushes them to SQLite on a
/// fixed interval. The in-memory copy stays authoritative between flushes.
struct PersistentPluginStateStore {
    values: RwLock<HashMap<String, Value>>,
    dirty: Mutex<HashSet<String>>,
    storage: Arc<Storage>,
}

impl PersistentPluginStateStore {
    fn new(storage: Arc<Storage>) -> Self {
        Self {
            values: RwLock::new(HashMap::new()),
            dirty: Mutex::new(HashSet::new()),
            storage,
        }
    }

    fn seed(&self, rows: Vec<(String, Value)>) {
        let mut values = self.values.write();
        for (key, value) in rows {
            values.insert(key, value);
        }
    }

    async fn flush(&self) -> Result<()> {
        let keys: Vec<String> = {
            let mut dirty = self.dirty.lock();
            dirty.drain().collect()
        };
        for key in keys {
            let Some(value) = self.values.read().get(&key).cloned() else {
                continue;
            };
            self.storage
                .llm
                .upsert_plugin_state(&key, &value.to_string())
                .await?;
        }
        Ok(())
    }
}

impl LlmPluginStateStore for PersistentPluginStateStore {
    fn get(&self, key: &str) -> Option<Value> {
        self.values.read().get(key).cloned()
    }

    fn put(&self, key: &str, value: Value) {
        self.values.write().insert(key.to_string(), value);
        self.dirty.lock().insert(key.to_string());
    }
}

/// Upper bound on cached resolved chains; clearing only costs recomputation.
const MAX_CACHED_CHAINS: usize = 4096;

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
        let state: Arc<dyn LlmPluginStateStore> = Arc::new(MemoryPluginStateStore::default());
        let counters: Arc<dyn LlmCounterStore> = Arc::new(MemoryCounterStore::default());
        Self::compile_with(
            data,
            registry,
            &LlmPluginMetrics::default(),
            &state,
            &counters,
        )
    }

    fn compile_with(
        data: PluginData,
        registry: &LlmPluginRegistry,
        metrics: &LlmPluginMetrics,
        state: &Arc<dyn LlmPluginStateStore>,
        counters: &Arc<dyn LlmCounterStore>,
    ) -> Result<Self> {
        let plugins: HashMap<&str, &PluginRecord> = data
            .plugins
            .iter()
            .filter(|plugin| plugin.kind == "llm")
            .map(|plugin| (plugin.id.as_str(), plugin))
            .collect();
        let mut by_scope: HashMap<ScopeKey, Vec<LlmEntry>> = HashMap::new();
        let mut configured = HashSet::new();
        for config in &data.configs {
            if let Some((key, entry)) =
                compile_config(config, &plugins, registry, metrics, state, counters)?
            {
                configured.insert(config.id.clone());
                by_scope.entry(key).or_default().push(entry);
            }
        }
        // Drop counters for configs that no longer exist. In-flight requests
        // keep their `Arc<PluginStats>` alive, but the snapshot stops
        // advertising a deleted instance.
        metrics.retain_configs(&configured);
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
        let mut cache = self.cache.lock();
        if cache.len() >= MAX_CACHED_CHAINS {
            cache.clear();
        }
        cache.insert(key, chain.clone());
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
    state: &Arc<dyn LlmPluginStateStore>,
    counters: &Arc<dyn LlmCounterStore>,
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
    let scope = LlmScope::parse(&config.scope)
        .ok_or_else(|| Error::Runtime(format!("unknown LLM plugin scope '{}'", config.scope)))?;
    if scope != LlmScope::Global && config.scope_id.trim().is_empty() {
        return Err(Error::Runtime(format!(
            "LLM plugin '{}': {scope} scope requires a scope id",
            plugin.name
        )));
    }
    if !factory.supported_scopes().contains(&scope) {
        // Legacy or hand-written rows may predate a scope restriction. Skip
        // them like an unregistered plugin instead of refusing to boot.
        tracing::warn!(
            plugin = %plugin.name,
            scope = %scope,
            "LLM plugin config uses an unsupported scope; skipping config"
        );
        return Ok(None);
    }
    let context = LlmPluginCreateContext {
        instance_key: &config.id,
        plugin_name: &plugin.name,
        scope: scope.as_str(),
        scope_id: &config.scope_id,
        state: state.clone(),
        counters: counters.clone(),
    };
    let instance = factory
        .create_with(value, &context)
        .map_err(|error| Error::Runtime(format!("LLM plugin '{}': {error}", plugin.name)))?;
    let manifest = instance.manifest();
    Ok(Some((
        (scope, config.scope_id.clone()),
        LlmEntry {
            name: plugin.name.clone(),
            scope,
            scope_id: config.scope_id.clone(),
            priority: i64::from(manifest.priority),
            timeout: manifest.timeout(),
            fail_policy: manifest.fail_policy,
            mutates_request: manifest.capabilities.contains(&Capability::RequestBody),
            stats: metrics.stats(&config.id, &plugin.name, scope, &config.scope_id),
            plugin: instance,
        },
    )))
}

pub struct LlmPluginRuntime {
    storage: Option<Arc<Storage>>,
    registry: Arc<LlmPluginRegistry>,
    chains: RwLock<Arc<LlmPluginChains>>,
    metrics: LlmPluginMetrics,
    state: Arc<dyn LlmPluginStateStore>,
    persistent: Option<Arc<PersistentPluginStateStore>>,
    counters: Arc<dyn LlmCounterStore>,
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
            state: Arc::new(MemoryPluginStateStore::default()),
            persistent: None,
            counters: Arc::new(MemoryCounterStore::default()),
        }
    }

    pub async fn bootstrap(
        storage: Arc<Storage>,
        registry: Arc<LlmPluginRegistry>,
    ) -> Result<Self> {
        Self::bootstrap_with_counters(storage, registry, Arc::new(MemoryCounterStore::default()))
            .await
    }

    /// Boots the runtime with a specific counter backend, for example the
    /// SQLite store shared by several daemon processes.
    pub async fn bootstrap_with_counters(
        storage: Arc<Storage>,
        registry: Arc<LlmPluginRegistry>,
        counters: Arc<dyn LlmCounterStore>,
    ) -> Result<Self> {
        let persistent = Arc::new(PersistentPluginStateStore::new(storage.clone()));
        persistent.seed(storage.llm.load_plugin_state().await?);
        let flusher = persistent.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(5));
            interval.tick().await;
            loop {
                interval.tick().await;
                if let Err(error) = flusher.flush().await {
                    tracing::warn!(%error, "cannot flush LLM plugin state");
                }
            }
        });
        let runtime = Self {
            storage: Some(storage),
            registry,
            chains: RwLock::new(Arc::new(LlmPluginChains::default())),
            metrics: LlmPluginMetrics::default(),
            state: persistent.clone(),
            persistent: Some(persistent),
            counters,
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
        self.upsert_manifests(storage).await?;
        let data = storage.llm.load_plugin_data().await?;
        self.compile_and_swap(data)
    }

    /// Reloads the chains with storage-backed plugin data plus extra records
    /// that are not persisted, such as AI policies compiled on the fly.
    ///
    /// The extra records are appended to the stored plugin catalog and
    /// configuration, so the normal plugin-admin view stays authoritative for
    /// everything that lives in storage. A failed compilation keeps the
    /// previously active chains.
    pub async fn reload_with_extra(&self, extra: PluginData) -> Result<()> {
        let mut data = match &self.storage {
            Some(storage) => {
                self.upsert_manifests(storage).await?;
                storage.llm.load_plugin_data().await?
            }
            None => PluginData::default(),
        };
        data.plugins.extend(extra.plugins);
        data.configs.extend(extra.configs);
        self.compile_and_swap(data)
    }

    async fn upsert_manifests(&self, storage: &Storage) -> Result<()> {
        for manifest in self.registry.manifests() {
            storage.llm.upsert_plugin(&manifest).await?;
        }
        Ok(())
    }

    /// Compiles a new chain set and swaps it in only after compilation
    /// succeeds, so a bad configuration leaves the old chains serving.
    fn compile_and_swap(&self, data: PluginData) -> Result<()> {
        let chains = LlmPluginChains::compile_with(
            data,
            &self.registry,
            &self.metrics,
            &self.state,
            &self.counters,
        )?;
        for manifest in self.registry.manifests() {
            self.metrics.seed_if_missing(&manifest.name);
        }
        *self.chains.write() = Arc::new(chains);
        Ok(())
    }

    /// Flushes dirty plugin state to storage. The periodic flusher calls this
    /// automatically; tests and graceful shutdown may call it directly.
    pub async fn flush_plugin_state(&self) -> Result<()> {
        match &self.persistent {
            Some(persistent) => persistent.flush().await,
            None => Ok(()),
        }
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
    use octra_llm_core::{
        AssistantContent, ChatRequest, ChatResponse, FinishReason, Message, Protocol, Role, Usage,
    };
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
            phases: Vec::new(),
            capabilities: Vec::<Capability>::new(),
            config_schema: None,
            api_version: "2".to_string(),
            entities: Vec::new(),
            migrations: Vec::new(),
            admin_routes: Vec::new(),
            subscriptions: Vec::new(),
            jobs: Vec::new(),
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
            source: "builtin".into(),
            artifact_id: None,
            storage_path: None,
            approved_capabilities: Vec::new(),
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
        let registry = LlmPluginRegistry::new();
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

    #[test]
    fn registry_allows_runtime_registration_and_removal() {
        let registry = LlmPluginRegistry::new();
        assert!(registry.factory("mark").is_none());
        registry.register(MarkFactory(MarkPlugin {
            manifest: manifest("mark"),
            mark: "marked",
        }));
        assert!(registry.factory("mark").is_some());
        assert!(registry.remove("mark").is_some());
        assert!(registry.factory("mark").is_none());
        assert!(registry.remove("mark").is_none());
    }

    #[tokio::test]
    async fn consumer_scope_overrides_global_and_api_plugins_are_ignored() {
        let registry = registry();
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
            virtual_key_family: None,
            consumer_group_ids: &[],
            session_id: None,
            client_ip: None,
            user_agent: None,
            intent: None,
        };
        let mut state = LlmRequestState::new();
        assert!(
            chain
                .on_request(
                    &meta,
                    &mut LlmResponseHeaders::default(),
                    &mut request,
                    &mut state
                )
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

        let registry = LlmPluginRegistry::new();
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
            virtual_key_family: None,
            consumer_group_ids: &[],
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
        let registry = LlmPluginRegistry::new();
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
            virtual_key_family: None,
            consumer_group_ids: &[],
            session_id: None,
            client_ip: None,
            user_agent: None,
            intent: None,
        };
        let mut request = ChatRequest::new("m", Vec::new());
        let mut state = LlmRequestState::new();
        chain
            .on_request(
                &meta,
                &mut LlmResponseHeaders::default(),
                &mut request,
                &mut state,
            )
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
        chain_with_failure_caps(policy, Vec::new())
    }

    fn chain_with_failure_caps(
        policy: FailPolicy,
        capabilities: Vec<Capability>,
    ) -> Arc<LlmPluginChain> {
        let registry = LlmPluginRegistry::new();
        let mut entry = manifest("failing");
        entry.fail_policy = policy;
        entry.capabilities = capabilities;
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
    async fn request_mutating_plugins_fail_closed_on_error() {
        let meta = LlmRequestMeta {
            request_id: "r",
            ingress: Protocol::Openai,
            consumer_id: None,
            virtual_key_id: None,
            virtual_key_family: None,
            consumer_group_ids: &[],
            session_id: None,
            client_ip: None,
            user_agent: None,
            intent: None,
        };
        let mut request = ChatRequest::new("m", Vec::new());
        let mut state = LlmRequestState::new();
        let chain = chain_with_failure_caps(FailPolicy::Open, vec![Capability::RequestBody]);
        assert!(matches!(
            chain
                .on_request(
                    &meta,
                    &mut LlmResponseHeaders::default(),
                    &mut request,
                    &mut state
                )
                .await,
            Err(LlmChainError::Plugin(_))
        ));
    }

    struct SlowPlugin {
        manifest: PluginManifest,
    }

    #[async_trait]
    impl LlmPlugin for SlowPlugin {
        fn manifest(&self) -> &PluginManifest {
            &self.manifest
        }

        async fn on_request(&self, _cx: &mut LlmRequestContext<'_>) -> LlmPluginResult {
            tokio::time::sleep(Duration::from_secs(5)).await;
            Ok(LlmAction::Continue)
        }
    }

    struct SlowFactory(PluginManifest);

    impl LlmPluginFactory for SlowFactory {
        fn manifest(&self) -> PluginManifest {
            self.0.clone()
        }

        fn create(&self, _config: Value) -> std::result::Result<Arc<dyn LlmPlugin>, PluginError> {
            Ok(Arc::new(SlowPlugin {
                manifest: self.0.clone(),
            }))
        }
    }

    #[tokio::test]
    async fn request_mutating_plugins_fail_closed_on_timeout() {
        let registry = LlmPluginRegistry::new();
        let mut entry = manifest("slow");
        entry.timeout_ms = 10;
        entry.capabilities = vec![Capability::RequestBody];
        registry.register(SlowFactory(entry));
        let data = PluginData {
            plugins: vec![plugin("slow", "p1", "llm", true)],
            configs: vec![config("c1", "p1", "global", "")],
        };
        let chain = LlmPluginChains::compile(data, &registry)
            .unwrap()
            .chain_for(None, None, "m", None);
        let meta = LlmRequestMeta {
            request_id: "r",
            ingress: Protocol::Openai,
            consumer_id: None,
            virtual_key_id: None,
            virtual_key_family: None,
            consumer_group_ids: &[],
            session_id: None,
            client_ip: None,
            user_agent: None,
            intent: None,
        };
        let mut request = ChatRequest::new("m", Vec::new());
        let mut state = LlmRequestState::new();
        assert!(matches!(
            chain
                .on_request(
                    &meta,
                    &mut LlmResponseHeaders::default(),
                    &mut request,
                    &mut state
                )
                .await,
            Err(LlmChainError::Plugin(error)) if error.message.contains("timed out")
        ));
    }

    #[tokio::test]
    async fn manifest_fail_policy_controls_timeouts_and_errors() {
        let meta = LlmRequestMeta {
            request_id: "r",
            ingress: Protocol::Openai,
            consumer_id: None,
            virtual_key_id: None,
            virtual_key_family: None,
            consumer_group_ids: &[],
            session_id: None,
            client_ip: None,
            user_agent: None,
            intent: None,
        };
        let mut request = ChatRequest::new("m", Vec::new());
        let mut state = LlmRequestState::new();
        let open = chain_with_failure(FailPolicy::Open);
        assert!(
            open.on_request(
                &meta,
                &mut LlmResponseHeaders::default(),
                &mut request,
                &mut state
            )
            .await
            .unwrap()
            .is_none()
        );

        let closed = chain_with_failure(FailPolicy::Closed);
        assert!(matches!(
            closed
                .on_request(
                    &meta,
                    &mut LlmResponseHeaders::default(),
                    &mut request,
                    &mut state
                )
                .await,
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
        let registry = LlmPluginRegistry::new();
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
            virtual_key_family: None,
            consumer_group_ids: &[],
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
            virtual_key_family: None,
            consumer_group_ids: &[],
            session_id: None,
            client_ip: None,
            user_agent: None,
            intent: None,
        }
    }

    #[tokio::test]
    async fn metrics_count_calls_failures_and_fail_open() {
        let registry = LlmPluginRegistry::new();
        registry.register(FailFactory(manifest("failing")));
        let data = PluginData {
            plugins: vec![plugin("failing", "p1", "llm", true)],
            configs: vec![config("c1", "p1", "global", "")],
        };
        let metrics = LlmPluginMetrics::default();
        let state: Arc<dyn LlmPluginStateStore> = Arc::new(MemoryPluginStateStore::default());
        let counters: Arc<dyn LlmCounterStore> = Arc::new(MemoryCounterStore::default());
        let chains =
            LlmPluginChains::compile_with(data, &registry, &metrics, &state, &counters).unwrap();
        let chain = chains.chain_for(None, None, "m", None);
        let mut request = ChatRequest::new("m", Vec::new());
        let mut state = LlmRequestState::new();
        chain
            .on_request(
                &test_meta(),
                &mut LlmResponseHeaders::default(),
                &mut request,
                &mut state,
            )
            .await
            .unwrap();

        let snapshot = metrics.snapshot();
        let view = snapshot
            .iter()
            .find(|view| view.name == "failing")
            .expect("metrics for the configured plugin");
        assert_eq!(view.config_id, "c1");
        assert_eq!(view.scope, LlmScope::Global);
        assert_eq!(view.scope_id, "");
        assert_eq!(view.stages["request"].calls, 1);
        assert_eq!(view.stages["request"].failures, 1);
        assert_eq!(view.stages["request"].fail_open, 1);
        assert_eq!(view.stages["request"].rejections, 0);
        assert_eq!(view.stages["response"].calls, 0);

        // The percentile fields must remain part of the serialized payload so
        // existing API consumers can pick them up without a schema change.
        let request_json = serde_json::to_value(&view.stages["request"]).unwrap();
        for field in [
            "latency_p50_micros",
            "latency_p95_micros",
            "latency_p99_micros",
        ] {
            assert!(
                request_json.get(field).is_some(),
                "missing serialized field {field}"
            );
        }
    }

    fn test_identity() -> PluginIdentity {
        PluginIdentity {
            name: "test".to_string(),
            config_id: "c1".to_string(),
            scope: LlmScope::Global,
            scope_id: String::new(),
        }
    }

    #[test]
    fn latency_percentiles_use_nearest_rank() {
        let stats = PluginStats::default();
        stats.record(
            Stage::Request,
            Duration::from_micros(100),
            None,
            false,
            false,
        );
        stats.record(
            Stage::Request,
            Duration::from_millis(10),
            None,
            false,
            false,
        );
        stats.record(Stage::Request, Duration::from_secs(1), None, false, false);

        let view = PluginMetricsView::from_stats(&test_identity(), &stats);
        let request = &view.stages["request"];
        assert_eq!(request.calls, 3);
        // Nearest-rank ranks for 3 samples are 2, 3 and 3, so p50 lands in
        // the 10 ms bucket while p95 and p99 land in the 1 s bucket.
        assert_eq!(request.latency_p50_micros, 10_000);
        assert_eq!(request.latency_p95_micros, 1_000_000);
        assert_eq!(request.latency_p99_micros, 1_000_000);
        assert_eq!(request.max_micros, 1_000_000);
    }

    #[test]
    fn latency_percentiles_are_zero_without_samples() {
        let stats = PluginStats::default();
        let view = PluginMetricsView::from_stats(&test_identity(), &stats);
        for stage in view.stages.values() {
            assert_eq!(stage.calls, 0);
            assert_eq!(stage.latency_p50_micros, 0);
            assert_eq!(stage.latency_p95_micros, 0);
            assert_eq!(stage.latency_p99_micros, 0);
        }
    }

    #[tokio::test]
    async fn metrics_distinguish_configs_by_scope() {
        let data = PluginData {
            plugins: vec![plugin("mark", "p1", "llm", true)],
            configs: vec![
                config("c1", "p1", "global", ""),
                config("c2", "p1", "consumer", "consumer-1"),
            ],
        };
        let metrics = LlmPluginMetrics::default();
        let state: Arc<dyn LlmPluginStateStore> = Arc::new(MemoryPluginStateStore::default());
        let counters: Arc<dyn LlmCounterStore> = Arc::new(MemoryCounterStore::default());
        let _chains =
            LlmPluginChains::compile_with(data, &registry(), &metrics, &state, &counters).unwrap();

        let snapshot = metrics.snapshot();
        let views: Vec<_> = snapshot.iter().filter(|view| view.name == "mark").collect();
        assert_eq!(views.len(), 2);
        assert_eq!(views[0].config_id, "c1");
        assert_eq!(views[0].scope, LlmScope::Global);
        assert_eq!(views[1].config_id, "c2");
        assert_eq!(views[1].scope, LlmScope::Consumer);
        assert_eq!(views[1].scope_id, "consumer-1");
    }

    struct RequestScopedFactory(MarkPlugin);

    impl LlmPluginFactory for RequestScopedFactory {
        fn manifest(&self) -> PluginManifest {
            self.0.manifest.clone()
        }

        fn supported_scopes(&self) -> &'static [LlmScope] {
            &octra_llm_plugin_types::REQUEST_LLM_SCOPES
        }

        fn create(&self, _config: Value) -> std::result::Result<Arc<dyn LlmPlugin>, PluginError> {
            Ok(Arc::new(MarkPlugin {
                manifest: self.0.manifest.clone(),
                mark: self.0.mark,
            }))
        }
    }

    #[tokio::test]
    async fn unsupported_scope_is_skipped_not_fatal() {
        let registry = LlmPluginRegistry::new();
        registry.register(RequestScopedFactory(MarkPlugin {
            manifest: manifest("request-only"),
            mark: "request-only",
        }));
        let data = PluginData {
            plugins: vec![plugin("request-only", "p1", "llm", true)],
            configs: vec![config("c1", "p1", "provider", "provider-1")],
        };
        let chains = LlmPluginChains::compile(data, &registry)
            .expect("a legacy unsupported scope must not abort compilation");
        assert!(!chains.has_any);
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

        let registry = LlmPluginRegistry::new();
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

    /// Factory that returns one shared plugin instance.
    struct SharedFactory {
        manifest: PluginManifest,
        plugin: Arc<dyn LlmPlugin>,
    }

    impl LlmPluginFactory for SharedFactory {
        fn manifest(&self) -> PluginManifest {
            self.manifest.clone()
        }

        fn create(&self, _config: Value) -> std::result::Result<Arc<dyn LlmPlugin>, PluginError> {
            Ok(self.plugin.clone())
        }
    }

    fn chain_for_plugin(name: &str, instance: Arc<dyn LlmPlugin>) -> Arc<LlmPluginChain> {
        let registry = LlmPluginRegistry::new();
        registry.register(SharedFactory {
            manifest: manifest(name),
            plugin: instance,
        });
        let data = PluginData {
            plugins: vec![plugin(name, "p1", "llm", true)],
            configs: vec![config("c1", "p1", "global", "")],
        };
        LlmPluginChains::compile(data, &registry)
            .unwrap()
            .chain_for(None, None, "m", None)
    }

    fn assistant_response(id: &str, model: &str, text: &str) -> ChatResponse {
        let mut response = ChatResponse::new(id, model, Message::new(Role::Assistant));
        response.message.assistant.push(AssistantContent::Text {
            text: text.to_string(),
        });
        response
    }

    struct ShortCircuitPlugin {
        manifest: PluginManifest,
    }

    #[async_trait]
    impl LlmPlugin for ShortCircuitPlugin {
        fn manifest(&self) -> &PluginManifest {
            &self.manifest
        }

        async fn on_request(&self, _cx: &mut LlmRequestContext<'_>) -> LlmPluginResult {
            Ok(LlmAction::Respond(Box::new(assistant_response(
                "cached-1",
                "cached-model",
                "cached answer",
            ))))
        }
    }

    #[tokio::test]
    async fn request_short_circuit_returns_the_plugin_response() {
        let chain = chain_for_plugin(
            "cache",
            Arc::new(ShortCircuitPlugin {
                manifest: manifest("cache"),
            }),
        );
        let mut request = ChatRequest::new("m", Vec::new());
        let mut state = LlmRequestState::new();
        let outcome = chain
            .on_request_outcome(
                &test_meta(),
                &mut LlmResponseHeaders::default(),
                &mut request,
                &mut state,
            )
            .await
            .unwrap();
        let LlmRequestOutcome::Respond(response) = outcome else {
            panic!("expected the request chain to short-circuit");
        };
        assert_eq!(response.model, "cached-model");
        assert_eq!(response.text(), "cached answer");
    }

    #[tokio::test]
    async fn compatibility_on_request_ignores_a_short_circuit() {
        let chain = chain_for_plugin(
            "cache",
            Arc::new(ShortCircuitPlugin {
                manifest: manifest("cache"),
            }),
        );
        let mut request = ChatRequest::new("m", Vec::new());
        let mut state = LlmRequestState::new();
        let rejection = chain
            .on_request(
                &test_meta(),
                &mut LlmResponseHeaders::default(),
                &mut request,
                &mut state,
            )
            .await
            .unwrap();
        assert!(rejection.is_none());
    }

    struct FinalObserverPlugin {
        manifest: PluginManifest,
    }

    #[async_trait]
    impl LlmPlugin for FinalObserverPlugin {
        fn manifest(&self) -> &PluginManifest {
            &self.manifest
        }

        async fn on_final_response(&self, cx: &mut LlmFinalResponseContext<'_>) {
            *cx.state = Value::String(cx.response.text());
        }
    }

    #[tokio::test]
    async fn final_response_hook_observes_the_final_text() {
        let chain = chain_for_plugin(
            "observer",
            Arc::new(FinalObserverPlugin {
                manifest: manifest("observer"),
            }),
        );
        let response = assistant_response("id", "m", "final text");
        let mut state = LlmRequestState::new();
        chain
            .on_final_response(&test_meta(), "m", &response, &mut state)
            .await;
        assert_eq!(
            state.slot("observer"),
            &Value::String("final text".to_string())
        );
    }

    struct MisplacedRespondPlugin {
        manifest: PluginManifest,
    }

    #[async_trait]
    impl LlmPlugin for MisplacedRespondPlugin {
        fn manifest(&self) -> &PluginManifest {
            &self.manifest
        }

        async fn on_provider_request(
            &self,
            _cx: &mut LlmProviderRequestContext<'_>,
        ) -> LlmPluginResult {
            Ok(LlmAction::Respond(Box::new(assistant_response(
                "id", "m", "nope",
            ))))
        }

        async fn on_response(&self, _cx: &mut LlmResponseContext<'_>) -> LlmPluginResult {
            Ok(LlmAction::Respond(Box::new(assistant_response(
                "id", "m", "nope",
            ))))
        }
    }

    #[tokio::test]
    async fn respond_outside_the_request_stage_fails_closed() {
        let chain = chain_for_plugin(
            "misplaced",
            Arc::new(MisplacedRespondPlugin {
                manifest: manifest("misplaced"),
            }),
        );
        let provider = LlmProviderInfo {
            attempt: 1,
            target_model_id: "m",
            provider_id: "provider-1",
            provider_name: "provider",
            protocol: Protocol::Openai,
            account_id: None,
            account_name: None,
        };
        let mut request = ChatRequest::new("m", Vec::new());
        let mut state = LlmRequestState::new();
        assert!(matches!(
            chain
                .on_provider_request(&test_meta(), &provider, &mut request, &mut state)
                .await,
            Err(LlmChainError::Plugin(error))
                if error.message.contains("only valid at the request stage")
        ));

        let mut response = assistant_response("id", "m", "final");
        let mut usage = Usage::default();
        assert!(matches!(
            chain
                .on_response(
                    &test_meta(),
                    "m",
                    &mut response,
                    &mut usage,
                    &mut state
                )
                .await,
            Err(LlmChainError::Plugin(error))
                if error.message.contains("only valid at the request stage")
        ));
    }

    #[test]
    fn metrics_include_the_final_response_stage() {
        let stats = PluginStats::default();
        stats.record(
            Stage::FinalResponse,
            Duration::from_micros(5),
            None,
            false,
            false,
        );
        let view = PluginMetricsView::from_stats(&test_identity(), &stats);
        assert_eq!(view.stages.len(), STAGE_COUNT);
        assert_eq!(view.stages["final_response"].calls, 1);
        assert_eq!(view.stages["request"].calls, 0);
    }
}
