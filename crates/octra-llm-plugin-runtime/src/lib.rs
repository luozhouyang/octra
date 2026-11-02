//! LLM plugin registry, scope resolution and ordered execution.
//!
//! Chains are compiled from the shared plugin catalog once per control-plane
//! mutation. The data plane only performs map lookups and in-memory calls, so
//! configuration changes never parse JSON on the request path.

// Rejections carry the full gateway error, as in `octra-llm-gateway`; boxing
// would only obscure the data-plane flow.
#![allow(clippy::result_large_err)]

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use octra_core::{Error, Result};
use octra_llm_core::{GatewayError, StreamEvent};
use octra_llm_plugin_api::{
    LlmAction, LlmErrorContext, LlmLogContext, LlmLogEvent, LlmPlugin, LlmPluginFactory,
    LlmPluginResult, LlmProviderInfo, LlmProviderRequestContext, LlmRequestContext, LlmRequestMeta,
    LlmRequestState, LlmResponseContext, LlmScope, LlmStreamAction, LlmStreamContext,
    LlmStreamResult,
};
use octra_plugin_api::{FailPolicy, PluginError, PluginManifest};
use octra_storage::Storage;
use octra_storage::plugins::{PluginConfigRecord, PluginData, PluginRecord};
use parking_lot::{Mutex, RwLock};
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

    pub fn factory(&self, name: &str) -> Option<Arc<dyn LlmPluginFactory>> {
        self.factories.get(name).cloned()
    }

    pub fn validate(&self, name: &str, config: &Value) -> std::result::Result<(), PluginError> {
        self.factory(name)
            .ok_or_else(|| PluginError::fail_closed(format!("unknown LLM plugin '{name}'")))?
            .validate(config)
    }
}

#[derive(Clone)]
struct LlmEntry {
    name: String,
    priority: i64,
    timeout: Duration,
    fail_policy: FailPolicy,
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
            match run_stage(entry, entry.plugin.on_request(&mut cx)).await? {
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
            match run_stage(entry, entry.plugin.on_provider_request(&mut cx)).await? {
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
            match run_stage(entry, entry.plugin.on_response(&mut cx)).await? {
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
            match run_stage(entry, entry.plugin.on_error(&mut cx)).await? {
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
            if tokio::time::timeout(entry.timeout, entry.plugin.on_log(&cx))
                .await
                .is_err()
            {
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

async fn run_stage<F>(entry: &LlmEntry, future: F) -> std::result::Result<LlmAction, LlmChainError>
where
    F: std::future::Future<Output = LlmPluginResult>,
{
    match tokio::time::timeout(entry.timeout, future).await {
        Ok(Ok(action)) => Ok(action),
        Ok(Err(error)) => handle_plugin_error(entry, error),
        Err(_) => timeout_error(entry),
    }
}

async fn run_stream_stage<F>(
    entry: &LlmEntry,
    future: F,
) -> std::result::Result<LlmStreamAction, LlmChainError>
where
    F: std::future::Future<Output = LlmStreamResult>,
{
    match tokio::time::timeout(entry.timeout, future).await {
        Ok(Ok(action)) => Ok(action),
        Ok(Err(error)) => match handle_plugin_error(entry, error)? {
            LlmAction::Continue => Ok(LlmStreamAction::Continue),
            LlmAction::Reject(_) => unreachable!("stream errors never reject this way"),
        },
        Err(_) => match timeout_error(entry)? {
            LlmAction::Continue => Ok(LlmStreamAction::Continue),
            LlmAction::Reject(_) => unreachable!("stream timeouts never reject this way"),
        },
    }
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
    fn compile(data: PluginData, registry: &LlmPluginRegistry) -> Result<Self> {
        let plugins: HashMap<&str, &PluginRecord> = data
            .plugins
            .iter()
            .filter(|plugin| plugin.kind == "llm")
            .map(|plugin| (plugin.id.as_str(), plugin))
            .collect();
        let mut by_scope: HashMap<ScopeKey, Vec<LlmEntry>> = HashMap::new();
        for config in &data.configs {
            if let Some((key, entry)) = compile_config(config, &plugins, registry)? {
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

fn compile_config(
    config: &PluginConfigRecord,
    plugins: &HashMap<&str, &PluginRecord>,
    registry: &LlmPluginRegistry,
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
    let value: Value = serde_json::from_str(&config.config_json).unwrap_or(Value::Null);
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
            priority: plugin.priority,
            timeout: manifest.timeout(),
            fail_policy: manifest.fail_policy,
            plugin: instance,
        },
    )))
}

pub struct LlmPluginRuntime {
    storage: Option<Arc<Storage>>,
    registry: Arc<LlmPluginRegistry>,
    chains: RwLock<Arc<LlmPluginChains>>,
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
        }
    }

    pub async fn bootstrap(
        storage: Arc<Storage>,
        registry: Arc<LlmPluginRegistry>,
    ) -> Result<Self> {
        for manifest in registry.manifests() {
            storage.upsert_builtin_plugin(&manifest).await?;
        }
        let runtime = Self {
            storage: Some(storage),
            registry,
            chains: RwLock::new(Arc::new(LlmPluginChains::default())),
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
        let data = storage.load_plugin_data().await?;
        let chains = LlmPluginChains::compile(data, &self.registry)?;
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use octra_llm_core::{ChatRequest, ChatResponse, FinishReason, Protocol, Usage};
    use octra_llm_plugin_api::LlmPluginResult;
    use octra_plugin_api::{Capability, PluginKind, PluginRuntimeKind};

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
