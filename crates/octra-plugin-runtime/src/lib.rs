//! Plugin registry, scope resolution, and ordered execution.
//!
//! Plugin configuration is compiled into immutable chains whenever the
//! control plane changes a plugin or one of its scoped configs. The data plane
//! only performs map lookups and in-memory plugin calls.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::response::Response;
use bytes::Bytes;
use octra_core::{Error, Result};
use octra_plugin_api::{
    ApiGatewayPlugin, ApiGatewayPluginFactory, BodyContext, ErrorContext, FailPolicy, LogContext,
    PluginAction, PluginError, PluginManifest, RequestContext, ResponseContext, Scope,
    UpstreamRequestContext,
};
use octra_storage::Storage;
use octra_storage::plugins::{PluginConfigRecord, PluginData, PluginRecord};
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
    name: String,
    priority: i64,
    timeout: Duration,
    plugin: Arc<dyn ApiGatewayPlugin>,
}

type ScopeKey = (Scope, String);
type ChainKey = (String, String, Option<String>);

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

    pub async fn on_request(
        &self,
        cx: &mut RequestContext<'_>,
    ) -> std::result::Result<Option<Response>, PluginError> {
        for entry in &self.entries {
            match run_stage(entry, entry.plugin.on_request(cx)).await? {
                PluginAction::Continue => {}
                PluginAction::ShortCircuit(response) => return Ok(Some(response)),
            }
        }
        Ok(None)
    }

    pub async fn on_upstream_request(
        &self,
        cx: &mut UpstreamRequestContext<'_>,
    ) -> std::result::Result<Option<Response>, PluginError> {
        for entry in &self.entries {
            match run_stage(entry, entry.plugin.on_upstream_request(cx)).await? {
                PluginAction::Continue => {}
                PluginAction::ShortCircuit(response) => return Ok(Some(response)),
            }
        }
        Ok(None)
    }

    pub async fn on_response(
        &self,
        cx: &mut ResponseContext<'_>,
    ) -> std::result::Result<Option<Response>, PluginError> {
        for entry in &self.entries {
            match run_stage(entry, entry.plugin.on_response(cx)).await? {
                PluginAction::Continue => {}
                PluginAction::ShortCircuit(response) => return Ok(Some(response)),
            }
        }
        Ok(None)
    }

    pub async fn on_response_body(
        &self,
        cx: &BodyContext<'_>,
        mut chunk: Bytes,
    ) -> std::result::Result<Bytes, PluginError> {
        for entry in &self.entries {
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

    pub async fn on_error(
        &self,
        cx: &mut ErrorContext<'_>,
    ) -> std::result::Result<Option<Response>, PluginError> {
        for entry in &self.entries {
            match run_stage(entry, entry.plugin.on_error(cx)).await? {
                PluginAction::Continue => {}
                PluginAction::ShortCircuit(response) => return Ok(Some(response)),
            }
        }
        Ok(None)
    }

    pub async fn on_log(&self, cx: &LogContext<'_>) {
        for entry in &self.entries {
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

#[derive(Default)]
struct PluginChains {
    by_scope: HashMap<ScopeKey, Vec<PluginEntry>>,
    cache: Mutex<HashMap<ChainKey, Arc<PluginChain>>>,
    empty: Arc<PluginChain>,
    has_any: bool,
}

impl PluginChains {
    fn compile(data: PluginData, registry: &PluginRegistry) -> Result<Self> {
        let plugins: HashMap<&str, &PluginRecord> = data
            .plugins
            .iter()
            .map(|plugin| (plugin.id.as_str(), plugin))
            .collect();
        let mut by_scope: HashMap<ScopeKey, Vec<PluginEntry>> = HashMap::new();

        for config in &data.configs {
            if let Some(entry) = compile_config(config, &plugins, registry)? {
                by_scope.entry(entry.0).or_default().push(entry.1);
            }
        }

        for entries in by_scope.values_mut() {
            entries.sort_by(|left, right| {
                right
                    .priority
                    .cmp(&left.priority)
                    .then_with(|| left.name.cmp(&right.name))
            });
        }

        Ok(Self {
            has_any: !by_scope.is_empty(),
            by_scope,
            cache: Mutex::new(HashMap::new()),
            empty: Arc::new(PluginChain::default()),
        })
    }

    fn chain_for(
        &self,
        route_id: &str,
        service_id: &str,
        consumer_id: Option<&str>,
    ) -> Arc<PluginChain> {
        if !self.has_any {
            return self.empty.clone();
        }

        let key = (
            route_id.to_string(),
            service_id.to_string(),
            consumer_id.map(str::to_string),
        );
        if let Some(chain) = self.cache.lock().get(&key) {
            return chain.clone();
        }

        let mut seen = std::collections::HashSet::new();
        let mut entries: Vec<PluginEntry> = Vec::new();
        let mut push_scope = |scope: Scope, id: &str| {
            let Some(list) = self.by_scope.get(&(scope, id.to_string())) else {
                return;
            };
            for entry in list {
                if seen.insert(entry.name.clone()) {
                    entries.push(entry.clone());
                }
            }
        };

        if let Some(consumer_id) = consumer_id {
            push_scope(Scope::Consumer, consumer_id);
        }
        push_scope(Scope::Route, route_id);
        push_scope(Scope::Service, service_id);
        push_scope(Scope::Global, "");

        entries.sort_by(|left, right| {
            right
                .priority
                .cmp(&left.priority)
                .then_with(|| left.name.cmp(&right.name))
        });

        let chain = Arc::new(PluginChain { entries });
        self.cache.lock().insert(key, chain.clone());
        chain
    }
}

fn compile_config(
    config: &PluginConfigRecord,
    plugins: &HashMap<&str, &PluginRecord>,
    registry: &PluginRegistry,
) -> Result<Option<(ScopeKey, PluginEntry)>> {
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
        tracing::warn!(plugin = %plugin.name, "plugin is not registered; skipping config");
        return Ok(None);
    };
    let value: Value = serde_json::from_str(&config.config_json).unwrap_or(Value::Null);
    let instance = factory
        .create(value)
        .map_err(|error| Error::Runtime(format!("plugin '{}': {error}", plugin.name)))?;
    let scope = Scope::parse(&config.scope)
        .ok_or_else(|| Error::Runtime(format!("unknown plugin scope '{}'", config.scope)))?;
    if scope != Scope::Global && config.scope_id.trim().is_empty() {
        return Err(Error::Runtime(format!(
            "plugin '{}': {scope} scope requires a scope id",
            plugin.name
        )));
    }
    let entry = PluginEntry {
        name: plugin.name.clone(),
        priority: plugin.priority,
        timeout: instance.manifest().timeout(),
        plugin: instance,
    };
    Ok(Some(((scope, config.scope_id.clone()), entry)))
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
        for manifest in registry.manifests() {
            storage.upsert_builtin_plugin(&manifest).await?;
        }
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
        let data = storage.load_plugin_data().await?;
        let chains = PluginChains::compile(data, &self.registry)?;
        *self.chains.write() = Arc::new(chains);
        Ok(())
    }

    pub fn chain_for(
        &self,
        route_id: &str,
        service_id: &str,
        consumer_id: Option<&str>,
    ) -> Arc<PluginChain> {
        let chains = self.chains.read().clone();
        chains.chain_for(route_id, service_id, consumer_id)
    }

    pub fn has_plugins(&self) -> bool {
        self.chains.read().has_any
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{HeaderName, HeaderValue};
    use octra_plugin_api::{
        ApiGatewayPlugin, Capability, PluginAction, PluginKind, PluginResult, PluginRuntimeKind,
        RequestContext,
    };
    use std::sync::Arc;

    struct MarkPlugin {
        manifest: PluginManifest,
        mark: &'static str,
    }

    #[async_trait::async_trait]
    impl ApiGatewayPlugin for MarkPlugin {
        fn manifest(&self) -> &PluginManifest {
            &self.manifest
        }

        async fn on_request(&self, cx: &mut RequestContext<'_>) -> PluginResult {
            cx.headers.insert(
                HeaderName::from_static("x-mark"),
                HeaderValue::from_static(self.mark),
            );
            Ok(PluginAction::Continue)
        }
    }

    struct MarkFactory {
        manifest: PluginManifest,
        mark: &'static str,
    }

    impl ApiGatewayPluginFactory for MarkFactory {
        fn manifest(&self) -> PluginManifest {
            self.manifest.clone()
        }

        fn create(
            &self,
            _config: Value,
        ) -> std::result::Result<Arc<dyn ApiGatewayPlugin>, PluginError> {
            Ok(Arc::new(MarkPlugin {
                manifest: self.manifest.clone(),
                mark: self.mark,
            }))
        }
    }

    fn manifest(name: &'static str, priority: i32) -> PluginManifest {
        PluginManifest {
            name: name.to_string(),
            version: "0.1.0".into(),
            kind: PluginKind::Api,
            runtime: PluginRuntimeKind::Builtin,
            priority,
            description: String::new(),
            timeout_ms: 1000,
            fail_policy: octra_plugin_api::FailPolicy::Open,
            capabilities: vec![Capability::RequestHeaders],
            config_schema: None,
        }
    }

    fn registry(names: &[(&'static str, i32)]) -> PluginRegistry {
        let mut registry = PluginRegistry::new();
        for (name, priority) in names {
            registry.register(MarkFactory {
                manifest: manifest(name, *priority),
                mark: name,
            });
        }
        registry
    }

    fn plugin_record(name: &str, priority: i64, enabled: bool) -> PluginRecord {
        PluginRecord {
            id: format!("plg_{name}"),
            name: name.to_string(),
            version: "0.1.0".into(),
            kind: "api".into(),
            runtime_type: "builtin".into(),
            enabled,
            priority,
            description: String::new(),
            manifest_json: "{}".into(),
            config_schema_json: "{}".into(),
            artifact_path: None,
            artifact_hash: None,
            updated_at: String::new(),
        }
    }

    fn config_record(
        id: &str,
        plugin: &str,
        scope: &str,
        scope_id: &str,
        enabled: bool,
        json: &str,
    ) -> PluginConfigRecord {
        PluginConfigRecord {
            id: id.into(),
            plugin_id: format!("plg_{plugin}"),
            scope: scope.into(),
            scope_id: scope_id.into(),
            enabled,
            config_json: json.into(),
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    fn compile(
        registry: &PluginRegistry,
        plugins: Vec<PluginRecord>,
        configs: Vec<PluginConfigRecord>,
    ) -> PluginChains {
        PluginChains::compile(PluginData { plugins, configs }, registry).unwrap()
    }

    fn service() -> octra_api_core::Service {
        let now = chrono::Utc::now();
        octra_api_core::Service {
            id: "s".into(),
            name: "s".into(),
            protocol: "http".into(),
            description: String::new(),
            enabled: true,
            connect_timeout_ms: 100,
            request_timeout_ms: 1000,
            retries: 0,
            require_auth: false,
            created_at: now,
            updated_at: now,
        }
    }

    fn route() -> octra_api_core::Route {
        let now = chrono::Utc::now();
        octra_api_core::Route {
            id: "r".into(),
            service_id: "s".into(),
            host: None,
            path_prefix: "/".into(),
            methods: vec![],
            headers: Default::default(),
            protocol: "http".into(),
            priority: 0,
            strip_prefix: false,
            enabled: true,
            created_at: now,
            updated_at: now,
        }
    }

    async fn run(chain: &PluginChain) -> String {
        let service = service();
        let route = route();
        let mut uri: http::Uri = "/".parse().unwrap();
        let mut headers = http::HeaderMap::new();
        let mut cx = RequestContext {
            request_id: "req",
            method: &http::Method::GET,
            uri: &mut uri,
            headers: &mut headers,
            client_ip: None,
            consumer_id: None,
            credential_id: None,
            route: &route,
            service: &service,
        };
        chain.on_request(&mut cx).await.unwrap();
        headers
            .get("x-mark")
            .map(|value| value.to_str().unwrap().to_string())
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn consumer_scope_overrides_route_service_and_global() {
        let registry = registry(&[("mark", 100)]);
        let chains = compile(
            &registry,
            vec![plugin_record("mark", 100, true)],
            vec![
                config_record("c-global", "mark", "global", "", true, "{}"),
                config_record("c-service", "mark", "service", "s", true, "{}"),
                config_record("c-route", "mark", "route", "r", true, "{}"),
                config_record("c-consumer", "mark", "consumer", "u", true, "{}"),
            ],
        );

        let chain = chains.chain_for("r", "s", None);
        assert_eq!(run(&chain).await, "mark");
        assert_eq!(chain.names().count(), 1);
    }

    #[tokio::test]
    async fn distinct_plugins_run_in_priority_order() {
        let registry = registry(&[("first", 10), ("second", 200)]);
        let chains = compile(
            &registry,
            vec![
                plugin_record("first", 10, true),
                plugin_record("second", 200, true),
            ],
            vec![
                config_record("c1", "first", "global", "", true, "{}"),
                config_record("c2", "second", "global", "", true, "{}"),
            ],
        );

        let chain = chains.chain_for("r", "s", None);
        let names: Vec<&str> = chain.names().collect();
        assert_eq!(names, vec!["second", "first"]);
    }

    #[tokio::test]
    async fn disabled_configs_and_plugins_are_skipped() {
        let registry = registry(&[("mark", 100)]);
        let chains = compile(
            &registry,
            vec![plugin_record("mark", 100, true)],
            vec![config_record("c", "mark", "global", "", false, "{}")],
        );
        let chain = chains.chain_for("r", "s", None);
        assert!(chain.is_empty());

        let chains = compile(
            &registry,
            vec![plugin_record("mark", 100, false)],
            vec![config_record("c", "mark", "global", "", true, "{}")],
        );
        assert!(chains.chain_for("r", "s", None).is_empty());
    }

    #[tokio::test]
    async fn invalid_config_fails_compilation() {
        let registry = registry(&[("mark", 100)]);
        let result = PluginChains::compile(
            PluginData {
                plugins: vec![plugin_record("mark", 100, true)],
                configs: vec![config_record("bad", "mark", "service", "", true, "{}")],
            },
            &registry,
        );
        assert!(result.is_err());
    }
}
