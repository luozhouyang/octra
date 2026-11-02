use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::body::Body;
use axum::response::Response;
use http::{HeaderMap, HeaderName, HeaderValue, StatusCode, Uri};
use octra_api_plugin_types::{
    ApiGatewayPlugin, Capability, LogContext, PluginAction, PluginError, PluginHost,
    PluginManifest, PluginResult, RequestContext, ResponseContext,
};
use parking_lot::Mutex;
use tokio::task::spawn_blocking;
use wasmtime::Store;
use wasmtime::component::{Component, HasSelf, Linker};

use crate::Plugin;
use crate::engine::WasmLimits;
use crate::gate::InflightGate;
use crate::host::HostState;
use crate::metrics::{Outcome, PluginStats};
use crate::octra::api_plugin::types as wit;

pub struct WasmApiGatewayPlugin {
    inner: Arc<WasmPluginInner>,
}

struct WasmPluginInner {
    manifest: PluginManifest,
    component: Arc<Component>,
    host: Arc<dyn PluginHost>,
    config_json: String,
    limits: WasmLimits,
    gate: Arc<InflightGate>,
    stats: Arc<PluginStats>,
    instance: Mutex<Option<RuntimeInstance>>,
}

struct RuntimeInstance {
    store: Store<HostState>,
    bindings: Plugin,
}

#[derive(Debug)]
enum WasmCallError {
    Plugin(String),
    Trap(String),
    Unavailable,
}

impl WasmCallError {
    fn is_timeout(&self) -> bool {
        match self {
            Self::Trap(message) => {
                let message = message.to_ascii_lowercase();
                message.contains("fuel")
                    || message.contains("epoch")
                    || message.contains("interrupted")
                    || message.contains("deadline")
            }
            Self::Plugin(_) | Self::Unavailable => false,
        }
    }

    fn into_plugin_error(self, manifest: &PluginManifest) -> PluginError {
        match self {
            Self::Plugin(message) => PluginError {
                message,
                fail_policy: manifest.fail_policy,
            },
            Self::Trap(message) => PluginError::fail_closed(message),
            Self::Unavailable => PluginError::fail_closed("wasm plugin instance is unavailable"),
        }
    }
}

impl WasmApiGatewayPlugin {
    pub(crate) fn new(
        manifest: PluginManifest,
        component: Arc<Component>,
        host: Arc<dyn PluginHost>,
        config_json: String,
        limits: WasmLimits,
        gate: Arc<InflightGate>,
        stats: Arc<PluginStats>,
    ) -> Result<Self, PluginError> {
        let mut limits = limits;
        limits.timeout = manifest.timeout();
        let inner = Arc::new(WasmPluginInner {
            manifest,
            component,
            host,
            config_json,
            limits,
            gate,
            stats,
            instance: Mutex::new(None),
        });
        inner.ensure_instance()?;
        Ok(Self { inner })
    }
}

#[async_trait]
impl ApiGatewayPlugin for WasmApiGatewayPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.inner.manifest
    }

    async fn on_request(&self, cx: &mut RequestContext<'_>) -> PluginResult {
        let started = Instant::now();
        let request = request_dto(cx);
        let inner = self.inner.clone();
        let result = match spawn_blocking(move || inner.call_request(request)).await {
            Ok(result) => result,
            Err(error) => {
                self.inner
                    .stats
                    .record(Outcome::Failure { timeout: false }, started.elapsed());
                return Err(PluginError::fail_closed(format!(
                    "wasm plugin task failed: {error}"
                )));
            }
        };
        match result {
            Ok(action) => {
                let outcome = if matches!(&action, wit::Action::Reject(_)) {
                    Outcome::Reject
                } else {
                    Outcome::Continue
                };
                let applied = apply_request_action(cx, action, self.inner.host.capabilities());
                let outcome = if applied.is_ok() {
                    outcome
                } else {
                    Outcome::Failure { timeout: false }
                };
                self.inner.stats.record(outcome, started.elapsed());
                applied
            }
            Err(error) => {
                let timeout = error.is_timeout();
                self.inner
                    .stats
                    .record(Outcome::Failure { timeout }, started.elapsed());
                Err(error.into_plugin_error(&self.inner.manifest))
            }
        }
    }

    async fn on_response(&self, cx: &mut ResponseContext<'_>) -> PluginResult {
        let started = Instant::now();
        let response = response_dto(cx);
        let inner = self.inner.clone();
        let result = match spawn_blocking(move || inner.call_response(response)).await {
            Ok(result) => result,
            Err(error) => {
                self.inner
                    .stats
                    .record(Outcome::Failure { timeout: false }, started.elapsed());
                return Err(PluginError::fail_closed(format!(
                    "wasm plugin task failed: {error}"
                )));
            }
        };
        match result {
            Ok(action) => {
                let outcome = if matches!(&action, wit::ResponseAction::Reject(_)) {
                    Outcome::Reject
                } else {
                    Outcome::Continue
                };
                let applied = apply_response_action(cx, action, self.inner.host.capabilities());
                let outcome = if applied.is_ok() {
                    outcome
                } else {
                    Outcome::Failure { timeout: false }
                };
                self.inner.stats.record(outcome, started.elapsed());
                applied
            }
            Err(error) => {
                let timeout = error.is_timeout();
                self.inner
                    .stats
                    .record(Outcome::Failure { timeout }, started.elapsed());
                Err(error.into_plugin_error(&self.inner.manifest))
            }
        }
    }

    async fn on_log(&self, cx: &LogContext<'_>) {
        let started = Instant::now();
        let entry = log_dto(cx);
        let inner = self.inner.clone();
        match spawn_blocking(move || inner.call_log(entry)).await {
            Ok(Ok(())) => self
                .inner
                .stats
                .record(Outcome::Continue, started.elapsed()),
            Ok(Err(error)) => {
                let timeout = error.is_timeout();
                self.inner
                    .stats
                    .record(Outcome::Failure { timeout }, started.elapsed());
                tracing::warn!(plugin = %self.inner.manifest.name, ?error, "wasm log hook failed");
            }
            Err(error) => {
                self.inner
                    .stats
                    .record(Outcome::Failure { timeout: false }, started.elapsed());
                tracing::warn!(plugin = %self.inner.manifest.name, %error, "wasm log task failed");
            }
        }
    }
}

impl WasmPluginInner {
    fn ensure_instance(&self) -> Result<(), PluginError> {
        let mut guard = self.instance.lock();
        if guard.is_some() {
            return Ok(());
        }

        let mut store = Store::new(
            self.component.engine(),
            HostState::new(self.host.clone(), self.limits.max_memory_bytes),
        );
        store.limiter(|state| &mut state.limits);

        store
            .set_fuel(self.limits.max_fuel)
            .map_err(|error| PluginError::fail_closed(format!("cannot arm wasm fuel: {error}")))?;
        store.set_epoch_deadline(epoch_ticks(self.limits.timeout));

        let mut linker = Linker::new(self.component.engine());
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)
            .map_err(|error| PluginError::fail_closed(format!("cannot link wasi: {error}")))?;
        Plugin::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state).map_err(|error| {
            PluginError::fail_closed(format!("cannot link wasm plugin: {error}"))
        })?;
        let bindings =
            Plugin::instantiate(&mut store, &self.component, &linker).map_err(|error| {
                PluginError::fail_closed(format!("cannot instantiate wasm plugin: {error}"))
            })?;
        store
            .set_fuel(self.limits.max_fuel)
            .map_err(|error| PluginError::fail_closed(format!("cannot arm wasm fuel: {error}")))?;
        store.set_epoch_deadline(epoch_ticks(self.limits.timeout));
        let configured = bindings
            .octra_api_plugin_plugin_api()
            .call_configure(&mut store, &self.config_json)
            .map_err(|error| {
                PluginError::fail_closed(format!("cannot configure wasm plugin: {error}"))
            })?;
        configured.map_err(|message| {
            PluginError::fail_closed(format!("plugin configure failed: {message}"))
        })?;

        *guard = Some(RuntimeInstance { store, bindings });
        Ok(())
    }

    fn call_request(&self, request: wit::Request) -> Result<wit::Action, WasmCallError> {
        let _permit = self.gate.acquire();
        let mut guard = self.instance.lock();
        let result = {
            let Some(instance) = guard.as_mut() else {
                return Err(WasmCallError::Unavailable);
            };
            arm(&mut instance.store, &self.limits)?;
            instance.store.data_mut().request_id = request.request_id.clone();
            instance
                .bindings
                .octra_api_plugin_plugin_api()
                .call_on_request(&mut instance.store, &request)
        };
        match result {
            Ok(Ok(action)) => Ok(action),
            Ok(Err(message)) => Err(WasmCallError::Plugin(message)),
            Err(error) => {
                *guard = None;
                Err(WasmCallError::Trap(error.to_string()))
            }
        }
    }

    fn call_response(&self, response: wit::Response) -> Result<wit::ResponseAction, WasmCallError> {
        let _permit = self.gate.acquire();
        let mut guard = self.instance.lock();
        let result = {
            let Some(instance) = guard.as_mut() else {
                return Err(WasmCallError::Unavailable);
            };
            arm(&mut instance.store, &self.limits)?;
            instance.store.data_mut().request_id = response.request_id.clone();
            instance
                .bindings
                .octra_api_plugin_plugin_api()
                .call_on_response(&mut instance.store, &response)
        };
        match result {
            Ok(Ok(action)) => Ok(action),
            Ok(Err(message)) => Err(WasmCallError::Plugin(message)),
            Err(error) => {
                *guard = None;
                Err(WasmCallError::Trap(error.to_string()))
            }
        }
    }

    fn call_log(&self, entry: wit::LogEntry) -> Result<(), WasmCallError> {
        let _permit = self.gate.acquire();
        let mut guard = self.instance.lock();
        let result = {
            let Some(instance) = guard.as_mut() else {
                return Err(WasmCallError::Unavailable);
            };
            arm(&mut instance.store, &self.limits)?;
            instance.store.data_mut().request_id = entry.request_id.clone();
            instance
                .bindings
                .octra_api_plugin_plugin_api()
                .call_on_log(&mut instance.store, &entry)
        };
        match result {
            Ok(()) => Ok(()),
            Err(error) => {
                *guard = None;
                Err(WasmCallError::Trap(error.to_string()))
            }
        }
    }
}

fn arm(store: &mut Store<HostState>, limits: &WasmLimits) -> Result<(), WasmCallError> {
    store
        .set_fuel(limits.max_fuel)
        .map_err(|error| WasmCallError::Trap(error.to_string()))?;
    store.set_epoch_deadline(epoch_ticks(limits.timeout));
    Ok(())
}

fn epoch_ticks(timeout: Duration) -> u64 {
    let millis = timeout.as_millis().max(1) as u64;
    (millis / 10).max(1).saturating_sub(1).max(1)
}

fn request_dto(cx: &RequestContext<'_>) -> wit::Request {
    wit::Request {
        request_id: cx.request_id.to_string(),
        method: cx.method.as_str().to_string(),
        path: cx.uri.path().to_string(),
        query: cx.uri.query().unwrap_or("").to_string(),
        headers: header_pairs(cx.headers),
        client_ip: cx.client_ip.map(|ip| ip.to_string()),
        consumer_id: cx.consumer_id.map(str::to_string),
        route_id: cx.route.id.clone(),
        service_id: cx.service.id.clone(),
    }
}

fn response_dto(cx: &ResponseContext<'_>) -> wit::Response {
    wit::Response {
        request_id: cx.request_id.to_string(),
        status: cx.status.as_u16(),
        headers: header_pairs(cx.headers),
        consumer_id: cx.consumer_id.map(str::to_string),
        route_id: cx.route.id.clone(),
        service_id: cx.service.id.clone(),
    }
}

fn log_dto(cx: &LogContext<'_>) -> wit::LogEntry {
    wit::LogEntry {
        request_id: cx.request_id.to_string(),
        method: cx.method.as_str().to_string(),
        path: cx.path.to_string(),
        status: cx.status,
        latency_ms: cx.latency.as_millis() as u64,
        consumer_id: cx.consumer_id.map(str::to_string),
        route_id: cx.route.id.clone(),
        service_id: cx.service.id.clone(),
    }
}

fn header_pairs(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_string(), value.to_string()))
        })
        .collect()
}

fn apply_request_action(
    cx: &mut RequestContext<'_>,
    action: wit::Action,
    capabilities: &[Capability],
) -> PluginResult {
    match action {
        wit::Action::Continue(mutation) => {
            apply_request_mutation(cx, mutation, capabilities)?;
            Ok(PluginAction::Continue)
        }
        wit::Action::Reject(reject) => Ok(PluginAction::ShortCircuit(build_reject(reject)?)),
    }
}

fn apply_response_action(
    cx: &mut ResponseContext<'_>,
    action: wit::ResponseAction,
    capabilities: &[Capability],
) -> PluginResult {
    match action {
        wit::ResponseAction::Continue(mutation) => {
            apply_response_mutation(cx, mutation, capabilities)?;
            Ok(PluginAction::Continue)
        }
        wit::ResponseAction::Reject(reject) => {
            Ok(PluginAction::ShortCircuit(build_reject(reject)?))
        }
    }
}

fn apply_request_mutation(
    cx: &mut RequestContext<'_>,
    mutation: wit::RequestMutation,
    capabilities: &[Capability],
) -> Result<(), PluginError> {
    let mutates = mutation.path.is_some()
        || mutation.query.is_some()
        || !mutation.set_headers.is_empty()
        || !mutation.remove_headers.is_empty();
    if mutates && !capabilities.contains(&Capability::RequestHeaders) {
        return Err(PluginError::fail_closed(
            "plugin mutated the request without the request_headers capability",
        ));
    }

    if mutation.path.is_some() || mutation.query.is_some() {
        let path = mutation.path.unwrap_or_else(|| cx.uri.path().to_string());
        let query = mutation
            .query
            .unwrap_or_else(|| cx.uri.query().unwrap_or("").to_string());
        let path_and_query = if query.is_empty() {
            path
        } else {
            format!("{path}?{query}")
        };
        let uri: Uri = path_and_query.parse().map_err(|error| {
            PluginError::fail_closed(format!("plugin returned an invalid URI: {error}"))
        })?;
        *cx.uri = uri;
    }

    for (name, value) in mutation.set_headers {
        let name = parse_header_name(&name)?;
        let value = HeaderValue::from_str(&value).map_err(|error| {
            PluginError::fail_closed(format!("plugin returned an invalid header value: {error}"))
        })?;
        cx.headers.insert(name, value);
    }
    for name in mutation.remove_headers {
        let name = parse_header_name(&name)?;
        cx.headers.remove(&name);
    }
    Ok(())
}

fn apply_response_mutation(
    cx: &mut ResponseContext<'_>,
    mutation: wit::ResponseMutation,
    capabilities: &[Capability],
) -> Result<(), PluginError> {
    let mutates = mutation.status.is_some()
        || !mutation.set_headers.is_empty()
        || !mutation.remove_headers.is_empty();
    if mutates && !capabilities.contains(&Capability::ResponseHeaders) {
        return Err(PluginError::fail_closed(
            "plugin mutated the response without the response_headers capability",
        ));
    }

    if let Some(status) = mutation.status {
        *cx.status = parse_status(status)?;
    }
    for (name, value) in mutation.set_headers {
        let name = parse_header_name(&name)?;
        let value = HeaderValue::from_str(&value).map_err(|error| {
            PluginError::fail_closed(format!("plugin returned an invalid header value: {error}"))
        })?;
        cx.headers.insert(name, value);
    }
    for name in mutation.remove_headers {
        let name = parse_header_name(&name)?;
        cx.headers.remove(&name);
    }
    Ok(())
}

fn build_reject(reject: wit::Reject) -> Result<Response, PluginError> {
    let status = parse_status(reject.status)?;
    let mut response = Response::new(Body::from(reject.body));
    *response.status_mut() = status;
    for (name, value) in reject.headers {
        let name = parse_header_name(&name)?;
        let value = HeaderValue::from_str(&value).map_err(|error| {
            PluginError::fail_closed(format!("plugin returned an invalid header value: {error}"))
        })?;
        response.headers_mut().append(name, value);
    }
    Ok(response)
}

fn parse_status(status: u16) -> Result<StatusCode, PluginError> {
    if !(100..=599).contains(&status) {
        return Err(PluginError::fail_closed(format!(
            "plugin returned an invalid HTTP status {status}"
        )));
    }
    StatusCode::from_u16(status).map_err(|error| {
        PluginError::fail_closed(format!("plugin returned an invalid HTTP status: {error}"))
    })
}

fn parse_header_name(name: &str) -> Result<HeaderName, PluginError> {
    HeaderName::from_bytes(name.as_bytes()).map_err(|error| {
        PluginError::fail_closed(format!("plugin returned an invalid header name: {error}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::Method;
    use octra_api_core::{Route, Service};
    use octra_api_plugin_types::GatewayState;

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

    #[test]
    fn epoch_ticks_stays_below_the_async_deadline() {
        assert_eq!(epoch_ticks(Duration::from_millis(50)), 4);
        assert_eq!(epoch_ticks(Duration::from_millis(250)), 24);
        assert_eq!(epoch_ticks(Duration::from_millis(1)), 1);
    }

    #[test]
    fn rejects_invalid_statuses() {
        assert!(parse_status(99).is_err());
        assert!(parse_status(600).is_err());
        assert!(parse_status(200).is_ok());
    }

    #[test]
    fn applies_request_mutations_with_capability() {
        let mut uri: Uri = "/old?x=1".parse().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("x-old", HeaderValue::from_static("1"));
        let mut state = GatewayState::default();
        let route = route();
        let service = service();
        let method = Method::GET;
        let mut cx = RequestContext {
            request_id: "req",
            state: &mut state,
            matched_path: "/old",
            method: &method,
            uri: &mut uri,
            headers: &mut headers,
            client_ip: None,
            consumer_id: None,
            credential_id: None,
            route: &route,
            service: &service,
        };

        let mutation = wit::RequestMutation {
            path: Some("/new".to_string()),
            query: Some("y=2".to_string()),
            set_headers: vec![("x-new".to_string(), "yes".to_string())],
            remove_headers: vec!["x-old".to_string()],
        };
        apply_request_mutation(&mut cx, mutation, &[Capability::RequestHeaders]).unwrap();
        assert_eq!(cx.uri.path(), "/new");
        assert_eq!(cx.uri.query(), Some("y=2"));
        assert_eq!(cx.headers.get("x-new").unwrap().to_str().unwrap(), "yes");
        assert!(cx.headers.get("x-old").is_none());
    }

    #[test]
    fn rejects_mutations_without_capability() {
        let mut uri: Uri = "/old".parse().unwrap();
        let mut headers = HeaderMap::new();
        let mut state = GatewayState::default();
        let route = route();
        let service = service();
        let method = Method::GET;
        let mut cx = RequestContext {
            request_id: "req",
            state: &mut state,
            matched_path: "/old",
            method: &method,
            uri: &mut uri,
            headers: &mut headers,
            client_ip: None,
            consumer_id: None,
            credential_id: None,
            route: &route,
            service: &service,
        };
        let mutation = wit::RequestMutation {
            path: None,
            query: None,
            set_headers: vec![("x-new".to_string(), "yes".to_string())],
            remove_headers: Vec::new(),
        };
        assert!(apply_request_mutation(&mut cx, mutation, &[]).is_err());
    }

    #[test]
    fn builds_reject_responses() {
        let response = build_reject(wit::Reject {
            status: 429,
            headers: vec![("retry-after".to_string(), "10".to_string())],
            body: b"limited".to_vec(),
        })
        .unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            response
                .headers()
                .get("retry-after")
                .unwrap()
                .to_str()
                .unwrap(),
            "10"
        );
    }

    #[test]
    fn build_reject_rejects_invalid_statuses() {
        let error = build_reject(wit::Reject {
            status: 99,
            headers: Vec::new(),
            body: Vec::new(),
        })
        .unwrap_err();
        assert!(error.message.contains("invalid HTTP status"));
    }

    #[test]
    fn adapter_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<WasmApiGatewayPlugin>();
    }
}
