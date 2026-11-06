use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::body::Body;
use axum::response::Response;
use bytes::Bytes;
use http::{HeaderMap, HeaderName, HeaderValue, StatusCode, Uri};
use octra_api_plugin_types::{
    ApiGatewayPlugin, BodyContext, Capability, ConsumerInfo, CredentialInfo, ErrorContext,
    GatewayEvent, GatewayState, JobInvocation, LogContext, PluginAction, PluginError, PluginHost,
    PluginManifest, PluginResult, RequestBodyContext, RequestContext, ResponseBodyContext,
    ResponseContext, UpstreamRequestContext,
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

/// A request routed to a plugin-declared Admin API endpoint.
#[derive(Debug, Clone)]
pub struct AdminDispatch {
    pub request_id: String,
    pub method: String,
    pub path: String,
    pub query: String,
    pub headers: Vec<(String, String)>,
    pub params: Vec<(String, String)>,
    pub body: Bytes,
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

    fn into_fail_closed(self) -> PluginError {
        match self {
            Self::Plugin(message) | Self::Trap(message) => PluginError::fail_closed(message),
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

    fn ensure_body_size(&self, len: usize) -> Result<(), PluginError> {
        if len > self.inner.limits.max_body_bytes {
            return Err(PluginError::fail_closed(format!(
                "plugin body of {len} bytes exceeds the {} byte limit",
                self.inner.limits.max_body_bytes
            )));
        }
        Ok(())
    }

    /// Dispatches a request to a plugin-declared Admin API route. Unlike
    /// data-plane hooks this does not record plugin metrics, and any failure
    /// is fail-closed.
    pub async fn dispatch_admin(&self, request: AdminDispatch) -> Result<Response, PluginError> {
        if !self
            .inner
            .host
            .capabilities()
            .contains(&Capability::AdminApi)
        {
            return Err(PluginError::fail_closed(
                "plugin does not have the admin_api capability",
            ));
        }
        let body = request.body.to_vec();
        self.ensure_body_size(body.len())?;
        let dto = wit::AdminRequest {
            request_id: request.request_id,
            method: request.method,
            path: request.path,
            query: request.query,
            headers: request.headers,
            params: request.params,
            body,
        };
        let inner = self.inner.clone();
        let timeout = self.inner.limits.timeout + Duration::from_millis(50);
        let result = tokio::time::timeout(timeout, spawn_blocking(move || inner.call_admin(dto)))
            .await
            .map_err(|_| PluginError::fail_closed("admin plugin call timed out"))?
            .map_err(|error| {
                PluginError::fail_closed(format!("wasm plugin task failed: {error}"))
            })?;
        let response = result.map_err(WasmCallError::into_fail_closed)?;
        self.ensure_body_size(response.body.len())?;
        build_admin_response(response)
    }

    async fn dispatch<T, F>(&self, call: F) -> Result<T, PluginError>
    where
        T: Send + 'static,
        F: FnOnce(Arc<WasmPluginInner>) -> Result<(T, Outcome), WasmCallError> + Send + 'static,
    {
        let started = Instant::now();
        let inner = self.inner.clone();
        match spawn_blocking(move || call(inner)).await {
            Ok(Ok((value, outcome))) => {
                self.inner.stats.record(outcome, started.elapsed());
                Ok(value)
            }
            Ok(Err(error)) => {
                let timeout = error.is_timeout();
                self.inner
                    .stats
                    .record(Outcome::Failure { timeout }, started.elapsed());
                Err(error.into_plugin_error(&self.inner.manifest))
            }
            Err(error) => {
                self.inner
                    .stats
                    .record(Outcome::Failure { timeout: false }, started.elapsed());
                Err(PluginError::fail_closed(format!(
                    "wasm plugin task failed: {error}"
                )))
            }
        }
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
        let auth = current_identity(cx.state);
        let authenticated = is_authenticated(cx.state);
        let inner = self.inner.clone();
        let result =
            match spawn_blocking(move || inner.call_request(request, auth, authenticated)).await {
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
            Ok((action, pending)) => {
                let outcome = if matches!(&action, wit::Action::Reject(_)) {
                    Outcome::Reject
                } else {
                    Outcome::Continue
                };
                let applied = apply_request_auth(cx, pending).and_then(|()| {
                    apply_request_action(cx, action, self.inner.host.capabilities())
                });
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

    async fn on_rewrite(&self, cx: &mut RequestContext<'_>) -> PluginResult {
        let request = request_dto(cx);
        let action = self
            .dispatch(move |inner| {
                inner.call_rewrite(request).map(|action| {
                    let outcome = classify_action(&action);
                    (action, outcome)
                })
            })
            .await?;
        apply_request_action(cx, action, self.inner.host.capabilities())
    }

    async fn on_upstream_request(&self, cx: &mut UpstreamRequestContext<'_>) -> PluginResult {
        let request = upstream_request_dto(cx);
        let action = self
            .dispatch(move |inner| {
                inner.call_upstream_request(request).map(|action| {
                    let outcome = classify_action(&action);
                    (action, outcome)
                })
            })
            .await?;
        apply_upstream_action(cx, action, self.inner.host.capabilities())
    }

    async fn on_request_buffer(
        &self,
        cx: &mut RequestBodyContext<'_>,
        body: Bytes,
    ) -> Result<Bytes, PluginError> {
        if !self
            .inner
            .host
            .capabilities()
            .contains(&Capability::RequestBody)
        {
            return Ok(body);
        }
        self.ensure_body_size(body.len())?;
        let request = request_body_dto(cx);
        let body = body.to_vec();
        let action = self
            .dispatch(move |inner| {
                inner.call_request_body(request, body).map(|action| {
                    let outcome = classify_request_body(&action);
                    (action, outcome)
                })
            })
            .await?;
        apply_request_body_action(cx, action, self.inner.host.capabilities())
    }

    async fn on_response_buffer(
        &self,
        cx: &mut ResponseBodyContext<'_>,
        body: Bytes,
    ) -> Result<Bytes, PluginError> {
        if !self
            .inner
            .host
            .capabilities()
            .contains(&Capability::ResponseBody)
        {
            return Ok(body);
        }
        self.ensure_body_size(body.len())?;
        let response = response_body_dto(cx);
        let body = body.to_vec();
        let action = self
            .dispatch(move |inner| {
                inner.call_response_buffer(response, body).map(|action| {
                    let outcome = classify_body(&action);
                    (action, outcome)
                })
            })
            .await?;
        apply_body_action(action, self.inner.host.capabilities())
    }

    async fn on_response_body(
        &self,
        cx: &BodyContext<'_>,
        chunk: Bytes,
    ) -> Result<Bytes, PluginError> {
        if !self
            .inner
            .host
            .capabilities()
            .contains(&Capability::ResponseBody)
        {
            return Ok(chunk);
        }
        self.ensure_body_size(chunk.len())?;
        let info = body_info(cx);
        let chunk = chunk.to_vec();
        let action = self
            .dispatch(move |inner| {
                inner.call_response_chunk(info, chunk).map(|action| {
                    let outcome = classify_body(&action);
                    (action, outcome)
                })
            })
            .await?;
        apply_body_action(action, self.inner.host.capabilities())
    }

    async fn on_error(&self, cx: &mut ErrorContext<'_>) -> PluginResult {
        let error = error_dto(cx);
        let action = self
            .dispatch(move |inner| {
                inner.call_error(error).map(|action| {
                    let outcome = classify_error(&action);
                    (action, outcome)
                })
            })
            .await?;
        apply_error_action(action)
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

    async fn on_event(&self, event: &GatewayEvent) -> Result<(), PluginError> {
        let event = wit::Event {
            id: event.id.clone(),
            topic: event.topic.clone(),
            source: event.source.clone(),
            timestamp: event.timestamp,
            payload_json: event.payload.to_string(),
        };
        self.dispatch(move |inner| inner.call_event(event).map(|()| ((), Outcome::Continue)))
            .await
    }

    async fn on_job(&self, job: &JobInvocation) -> Result<(), PluginError> {
        let job = wit::Job {
            id: job.id.clone(),
            name: job.name.clone(),
            scheduled_for: job.scheduled_for,
            attempt: job.attempt,
        };
        self.dispatch(move |inner| inner.call_job(job).map(|()| ((), Outcome::Continue)))
            .await
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

    fn invoke<T>(
        &self,
        request_id: &str,
        call: impl FnOnce(&mut Store<HostState>, &Plugin) -> wasmtime::Result<Result<T, String>>,
    ) -> Result<T, WasmCallError> {
        let mut guard = self.instance.lock();
        let Some(instance) = guard.as_mut() else {
            return Err(WasmCallError::Unavailable);
        };
        arm(&mut instance.store, &self.limits)?;
        instance.store.data_mut().request_id = request_id.to_string();
        match call(&mut instance.store, &instance.bindings) {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(message)) => Err(WasmCallError::Plugin(message)),
            Err(error) => {
                *guard = None;
                Err(WasmCallError::Trap(error.to_string()))
            }
        }
    }

    fn call_rewrite(&self, request: wit::Request) -> Result<wit::Action, WasmCallError> {
        let request_id = request.request_id.clone();
        self.invoke(&request_id, |store, bindings| {
            bindings
                .octra_api_plugin_plugin_api()
                .call_on_rewrite(store, &request)
        })
    }

    fn call_upstream_request(
        &self,
        request: wit::UpstreamRequest,
    ) -> Result<wit::Action, WasmCallError> {
        let request_id = request.request_id.clone();
        self.invoke(&request_id, |store, bindings| {
            bindings
                .octra_api_plugin_plugin_api()
                .call_on_upstream_request(store, &request)
        })
    }

    fn call_request_body(
        &self,
        request: wit::Request,
        body: Vec<u8>,
    ) -> Result<wit::RequestBodyAction, WasmCallError> {
        let request_id = request.request_id.clone();
        self.invoke(&request_id, |store, bindings| {
            bindings
                .octra_api_plugin_plugin_api()
                .call_on_request_body(store, &request, &body)
        })
    }

    fn call_response_buffer(
        &self,
        response: wit::Response,
        body: Vec<u8>,
    ) -> Result<wit::BodyAction, WasmCallError> {
        let request_id = response.request_id.clone();
        self.invoke(&request_id, |store, bindings| {
            bindings
                .octra_api_plugin_plugin_api()
                .call_on_response_buffer(store, &response, &body)
        })
    }

    fn call_response_chunk(
        &self,
        info: wit::BodyInfo,
        chunk: Vec<u8>,
    ) -> Result<wit::BodyAction, WasmCallError> {
        let request_id = info.request_id.clone();
        self.invoke(&request_id, |store, bindings| {
            bindings
                .octra_api_plugin_plugin_api()
                .call_on_response_chunk(store, &info, &chunk)
        })
    }

    fn call_error(&self, error: wit::ErrorInfo) -> Result<wit::ErrorAction, WasmCallError> {
        let request_id = error.request_id.clone();
        self.invoke(&request_id, |store, bindings| {
            bindings
                .octra_api_plugin_plugin_api()
                .call_on_error(store, &error)
        })
    }

    fn call_admin(&self, request: wit::AdminRequest) -> Result<wit::AdminResponse, WasmCallError> {
        let _permit = self.gate.acquire();
        let request_id = request.request_id.clone();
        self.invoke(&request_id, |store, bindings| {
            bindings
                .octra_api_plugin_plugin_api()
                .call_on_admin_request(store, &request)
        })
    }

    fn call_request(
        &self,
        request: wit::Request,
        auth: Option<wit::AuthIdentity>,
        authenticated: bool,
    ) -> Result<(wit::Action, Option<wit::AuthIdentity>), WasmCallError> {
        let _permit = self.gate.acquire();
        let mut guard = self.instance.lock();
        let result = {
            let Some(instance) = guard.as_mut() else {
                return Err(WasmCallError::Unavailable);
            };
            arm(&mut instance.store, &self.limits)?;
            {
                let data = instance.store.data_mut();
                data.request_id = request.request_id.clone();
                data.begin_auth(auth, authenticated);
            }
            let result = instance
                .bindings
                .octra_api_plugin_plugin_api()
                .call_on_request(&mut instance.store, &request);
            let pending = instance.store.data_mut().take_pending_auth();
            (result, pending)
        };
        match result {
            (Ok(Ok(action)), pending) => Ok((action, pending)),
            (Ok(Err(message)), _) => Err(WasmCallError::Plugin(message)),
            (Err(error), _) => {
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
    fn call_event(&self, event: wit::Event) -> Result<(), WasmCallError> {
        let id = event.id.clone();
        self.invoke(&id, |store, bindings| {
            bindings
                .octra_api_plugin_plugin_api()
                .call_on_event(store, &event)
        })
    }

    fn call_job(&self, job: wit::Job) -> Result<(), WasmCallError> {
        let id = job.id.clone();
        self.invoke(&id, |store, bindings| {
            bindings
                .octra_api_plugin_plugin_api()
                .call_on_job(store, &job)
        })
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

fn upstream_request_dto(cx: &UpstreamRequestContext<'_>) -> wit::UpstreamRequest {
    wit::UpstreamRequest {
        request_id: cx.request_id.to_string(),
        method: cx.method.as_str().to_string(),
        path: cx.uri.path().to_string(),
        query: cx.uri.query().unwrap_or("").to_string(),
        headers: header_pairs(cx.headers),
        consumer_id: cx.consumer_id.map(str::to_string),
        route_id: cx.route.id.clone(),
        service_id: cx.service.id.clone(),
        target_url: cx.target_url.to_string(),
    }
}

fn request_body_dto(cx: &RequestBodyContext<'_>) -> wit::Request {
    wit::Request {
        request_id: cx.request_id.to_string(),
        method: cx.method.as_str().to_string(),
        path: cx.uri.path().to_string(),
        query: cx.uri.query().unwrap_or("").to_string(),
        headers: header_pairs(cx.headers),
        client_ip: None,
        consumer_id: cx.consumer_id.map(str::to_string),
        route_id: cx.route.id.clone(),
        service_id: cx.service.id.clone(),
    }
}

fn response_body_dto(cx: &ResponseBodyContext<'_>) -> wit::Response {
    wit::Response {
        request_id: cx.request_id.to_string(),
        status: cx.status.as_u16(),
        headers: header_pairs(cx.headers),
        consumer_id: cx.consumer_id.map(str::to_string),
        route_id: cx.route.id.clone(),
        service_id: cx.service.id.clone(),
    }
}

fn body_info(cx: &BodyContext<'_>) -> wit::BodyInfo {
    wit::BodyInfo {
        request_id: cx.request_id.to_string(),
        route_id: cx.route.id.clone(),
        service_id: cx.service.id.clone(),
        target_url: cx.target_url.to_string(),
    }
}

fn error_dto(cx: &ErrorContext<'_>) -> wit::ErrorInfo {
    wit::ErrorInfo {
        request_id: cx.request_id.to_string(),
        method: cx.method.as_str().to_string(),
        path: cx.uri.path().to_string(),
        query: cx.uri.query().unwrap_or("").to_string(),
        headers: header_pairs(cx.headers),
        consumer_id: cx.consumer_id.map(str::to_string),
        route_id: cx.route.id.clone(),
        service_id: cx.service.id.clone(),
        target_url: cx.target_url.to_string(),
        message: cx.error.to_string(),
    }
}

fn classify_action(action: &wit::Action) -> Outcome {
    match action {
        wit::Action::Reject(_) => Outcome::Reject,
        wit::Action::Continue(_) => Outcome::Continue,
    }
}

fn classify_request_body(action: &wit::RequestBodyAction) -> Outcome {
    match action {
        wit::RequestBodyAction::Reject(_) => Outcome::Reject,
        wit::RequestBodyAction::Continue(_) => Outcome::Continue,
    }
}

fn classify_body(action: &wit::BodyAction) -> Outcome {
    match action {
        wit::BodyAction::Reject(_) => Outcome::Reject,
        wit::BodyAction::Continue(_) => Outcome::Continue,
    }
}

fn classify_error(action: &wit::ErrorAction) -> Outcome {
    match action {
        wit::ErrorAction::Respond(_) => Outcome::Reject,
        wit::ErrorAction::Continue => Outcome::Continue,
    }
}

fn is_authenticated(state: &GatewayState) -> bool {
    state.consumer.is_some() && !state.anonymous
}

fn current_identity(state: &GatewayState) -> Option<wit::AuthIdentity> {
    let consumer = state.consumer.as_ref()?;
    Some(wit::AuthIdentity {
        consumer_id: Some(consumer.id.clone()),
        consumer_username: consumer.username.clone(),
        consumer_custom_id: consumer.custom_id.clone(),
        credential_id: state
            .credential
            .as_ref()
            .map(|credential| credential.id.clone()),
        credential_type: state
            .credential
            .as_ref()
            .map(|credential| credential.kind.clone()),
    })
}

fn apply_request_auth(
    cx: &mut RequestContext<'_>,
    pending: Option<wit::AuthIdentity>,
) -> Result<(), PluginError> {
    if let Some(identity) = pending {
        let (consumer, credential) = resolve_auth_identity(cx.state, identity)?;
        cx.state.consumer = Some(consumer);
        cx.state.credential = credential;
        cx.state.anonymous = false;
    }
    Ok(())
}

fn resolve_auth_identity(
    state: &GatewayState,
    identity: wit::AuthIdentity,
) -> Result<(ConsumerInfo, Option<CredentialInfo>), PluginError> {
    let mut consumer_id = None;
    if let Some(id) = identity.consumer_id {
        if !state.consumers.contains_key(&id) {
            return Err(identity_error(format!(
                "auth plugin returned an unknown consumer id '{id}'"
            )));
        }
        consumer_id = Some(id);
    }
    if let Some(username) = identity.consumer_username {
        let id = state.consumer_by_username.get(&username).ok_or_else(|| {
            identity_error(format!(
                "auth plugin returned an unknown consumer username '{username}'"
            ))
        })?;
        merge_consumer_id(&mut consumer_id, id)?;
    }
    if let Some(custom_id) = identity.consumer_custom_id {
        let id = state.consumer_by_custom_id.get(&custom_id).ok_or_else(|| {
            identity_error(format!(
                "auth plugin returned an unknown consumer custom id '{custom_id}'"
            ))
        })?;
        merge_consumer_id(&mut consumer_id, id)?;
    }

    let consumer_id = consumer_id.ok_or_else(|| {
        identity_error("auth plugin authenticated the request without a consumer identity")
    })?;
    let consumer = state.consumers.get(&consumer_id).cloned().ok_or_else(|| {
        identity_error(format!(
            "auth plugin returned an unknown consumer id '{consumer_id}'"
        ))
    })?;
    let credential = identity.credential_id.map(|id| CredentialInfo {
        identifier: id.clone(),
        id,
        kind: identity
            .credential_type
            .filter(|kind| !kind.trim().is_empty())
            .unwrap_or_else(|| "plugin".to_string()),
    });
    Ok((consumer, credential))
}

fn merge_consumer_id(current: &mut Option<String>, candidate: &str) -> Result<(), PluginError> {
    match current {
        Some(existing) if existing != candidate => Err(identity_error(format!(
            "auth plugin returned conflicting consumer identities '{existing}' and '{candidate}'"
        ))),
        Some(_) => Ok(()),
        None => {
            *current = Some(candidate.to_string());
            Ok(())
        }
    }
}

fn identity_error(message: impl Into<String>) -> PluginError {
    PluginError::fail_closed(message.into())
}

fn apply_upstream_action(
    cx: &mut UpstreamRequestContext<'_>,
    action: wit::Action,
    capabilities: &[Capability],
) -> PluginResult {
    match action {
        wit::Action::Continue(mutation) => {
            apply_upstream_mutation(cx, mutation, capabilities)?;
            Ok(PluginAction::Continue)
        }
        wit::Action::Reject(reject) => Ok(PluginAction::ShortCircuit(build_reject(reject)?)),
    }
}

fn apply_upstream_mutation(
    cx: &mut UpstreamRequestContext<'_>,
    mutation: wit::RequestMutation,
    capabilities: &[Capability],
) -> Result<(), PluginError> {
    let mutates = mutation.path.is_some()
        || mutation.query.is_some()
        || !mutation.set_headers.is_empty()
        || !mutation.remove_headers.is_empty();
    if mutates && !capabilities.contains(&Capability::RequestHeaders) {
        return Err(PluginError::fail_closed(
            "plugin mutated the upstream request without the request_headers capability",
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

    apply_headers(cx.headers, mutation.set_headers, mutation.remove_headers)
}

fn apply_request_body_action(
    cx: &mut RequestBodyContext<'_>,
    action: wit::RequestBodyAction,
    capabilities: &[Capability],
) -> Result<Bytes, PluginError> {
    match action {
        wit::RequestBodyAction::Continue(result) => {
            if !capabilities.contains(&Capability::RequestBody) {
                return Err(PluginError::fail_closed(
                    "plugin read the request body without the request_body capability",
                ));
            }
            if (!result.set_headers.is_empty() || !result.remove_headers.is_empty())
                && !capabilities.contains(&Capability::RequestHeaders)
            {
                return Err(PluginError::fail_closed(
                    "plugin mutated request headers without the request_headers capability",
                ));
            }
            apply_headers(cx.headers, result.set_headers, result.remove_headers)?;
            Ok(Bytes::from(result.body))
        }
        wit::RequestBodyAction::Reject(reject) => Err(PluginError::fail_closed(format!(
            "plugin rejected the request body with status {}",
            reject.status
        ))),
    }
}

fn apply_body_action(
    action: wit::BodyAction,
    capabilities: &[Capability],
) -> Result<Bytes, PluginError> {
    match action {
        wit::BodyAction::Continue(body) => {
            if !capabilities.contains(&Capability::ResponseBody) {
                return Err(PluginError::fail_closed(
                    "plugin read the response body without the response_body capability",
                ));
            }
            Ok(Bytes::from(body))
        }
        wit::BodyAction::Reject(reject) => Err(PluginError::fail_closed(format!(
            "plugin rejected the response body with status {}",
            reject.status
        ))),
    }
}

fn apply_error_action(action: wit::ErrorAction) -> PluginResult {
    match action {
        wit::ErrorAction::Continue => Ok(PluginAction::Continue),
        wit::ErrorAction::Respond(reject) => Ok(PluginAction::ShortCircuit(build_reject(reject)?)),
    }
}

fn apply_headers(
    headers: &mut HeaderMap,
    set: Vec<(String, String)>,
    remove: Vec<String>,
) -> Result<(), PluginError> {
    for (name, value) in set {
        let name = parse_header_name(&name)?;
        let value = HeaderValue::from_str(&value).map_err(|error| {
            PluginError::fail_closed(format!("plugin returned an invalid header value: {error}"))
        })?;
        headers.insert(name, value);
    }
    for name in remove {
        let name = parse_header_name(&name)?;
        headers.remove(&name);
    }
    Ok(())
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

fn build_admin_response(response: wit::AdminResponse) -> Result<Response, PluginError> {
    let status = parse_status(response.status)?;
    let mut result = Response::new(Body::from(response.body));
    *result.status_mut() = status;
    for (name, value) in response.headers {
        let name = parse_header_name(&name)?;
        if is_host_managed_header(&name) {
            continue;
        }
        let value = HeaderValue::from_str(&value).map_err(|error| {
            PluginError::fail_closed(format!("plugin returned an invalid header value: {error}"))
        })?;
        result.headers_mut().append(name, value);
    }
    Ok(result)
}

fn is_host_managed_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "content-length"
            | "keep-alive"
            | "proxy-connection"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    ) || name.as_str().starts_with("proxy-")
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

    fn auth_state() -> GatewayState {
        let mut state = GatewayState::default();
        state.consumers.insert(
            "c1".to_string(),
            ConsumerInfo {
                id: "c1".to_string(),
                username: Some("alice".to_string()),
                custom_id: Some("alice-1".to_string()),
                ..ConsumerInfo::default()
            },
        );
        state.consumers.insert(
            "c2".to_string(),
            ConsumerInfo {
                id: "c2".to_string(),
                username: Some("bob".to_string()),
                ..ConsumerInfo::default()
            },
        );
        state
            .consumer_by_username
            .insert("alice".to_string(), "c1".to_string());
        state
            .consumer_by_username
            .insert("bob".to_string(), "c2".to_string());
        state
            .consumer_by_custom_id
            .insert("alice-1".to_string(), "c1".to_string());
        state
    }

    fn empty_identity() -> wit::AuthIdentity {
        wit::AuthIdentity {
            consumer_id: None,
            consumer_username: None,
            consumer_custom_id: None,
            credential_id: None,
            credential_type: None,
        }
    }

    #[test]
    fn resolves_auth_identity_by_id_and_credential() {
        let state = auth_state();
        let identity = wit::AuthIdentity {
            consumer_id: Some("c1".to_string()),
            credential_id: Some("secret-1".to_string()),
            credential_type: Some("key-auth".to_string()),
            ..empty_identity()
        };
        let (consumer, credential) = resolve_auth_identity(&state, identity).unwrap();
        assert_eq!(consumer.id, "c1");
        let credential = credential.unwrap();
        assert_eq!(credential.id, "secret-1");
        assert_eq!(credential.kind, "key-auth");
        assert_eq!(credential.identifier, "secret-1");
    }

    #[test]
    fn resolves_auth_identity_by_username_and_custom_id() {
        let state = auth_state();
        let identity = wit::AuthIdentity {
            consumer_username: Some("alice".to_string()),
            ..empty_identity()
        };
        assert_eq!(resolve_auth_identity(&state, identity).unwrap().0.id, "c1");

        let identity = wit::AuthIdentity {
            consumer_custom_id: Some("alice-1".to_string()),
            ..empty_identity()
        };
        assert_eq!(resolve_auth_identity(&state, identity).unwrap().0.id, "c1");
    }

    #[test]
    fn rejects_unknown_auth_identities() {
        let state = auth_state();
        let identity = wit::AuthIdentity {
            consumer_id: Some("missing".to_string()),
            ..empty_identity()
        };
        assert!(resolve_auth_identity(&state, identity).is_err());

        let identity = wit::AuthIdentity {
            consumer_username: Some("missing".to_string()),
            ..empty_identity()
        };
        assert!(resolve_auth_identity(&state, identity).is_err());
    }

    #[test]
    fn rejects_conflicting_auth_identities() {
        let state = auth_state();
        let identity = wit::AuthIdentity {
            consumer_id: Some("c1".to_string()),
            consumer_username: Some("bob".to_string()),
            ..empty_identity()
        };
        assert!(resolve_auth_identity(&state, identity).is_err());
    }

    #[test]
    fn rejects_authentication_without_a_consumer() {
        let state = auth_state();
        let identity = wit::AuthIdentity {
            credential_id: Some("secret-1".to_string()),
            ..empty_identity()
        };
        assert!(resolve_auth_identity(&state, identity).is_err());
    }

    #[test]
    fn applies_pending_auth_identity() {
        let mut uri: Uri = "/".parse().unwrap();
        let mut headers = HeaderMap::new();
        let mut state = auth_state();
        let route = route();
        let service = service();
        let method = Method::GET;
        let mut cx = RequestContext {
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

        let identity = wit::AuthIdentity {
            consumer_id: Some("c1".to_string()),
            credential_id: Some("secret-1".to_string()),
            credential_type: Some("key-auth".to_string()),
            ..empty_identity()
        };
        apply_request_auth(&mut cx, Some(identity)).unwrap();
        assert_eq!(cx.state.consumer.as_ref().unwrap().id, "c1");
        assert_eq!(cx.state.credential.as_ref().unwrap().id, "secret-1");
        assert!(!cx.state.anonymous);
        assert!(is_authenticated(cx.state));

        let current = current_identity(cx.state).unwrap();
        assert_eq!(current.consumer_id.as_deref(), Some("c1"));
        assert_eq!(current.consumer_username.as_deref(), Some("alice"));
        assert_eq!(current.credential_id.as_deref(), Some("secret-1"));
    }

    #[test]
    fn builds_admin_responses_and_drops_host_managed_headers() {
        let response = build_admin_response(wit::AdminResponse {
            status: 200,
            headers: vec![
                ("content-type".to_string(), "application/json".to_string()),
                ("transfer-encoding".to_string(), "chunked".to_string()),
                ("content-length".to_string(), "999".to_string()),
                ("x-plugin".to_string(), "yes".to_string()),
            ],
            body: b"{}".to_vec(),
        })
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "application/json"
        );
        assert!(response.headers().get("transfer-encoding").is_none());
        assert!(response.headers().get("content-length").is_none());
        assert_eq!(response.headers().get("x-plugin").unwrap(), "yes");
    }

    #[test]
    fn rejects_invalid_admin_responses() {
        assert!(
            build_admin_response(wit::AdminResponse {
                status: 99,
                headers: Vec::new(),
                body: Vec::new(),
            })
            .is_err()
        );
        assert!(
            build_admin_response(wit::AdminResponse {
                status: 200,
                headers: vec![("bad header".to_string(), "value".to_string())],
                body: Vec::new(),
            })
            .is_err()
        );
    }

    #[test]
    fn applies_upstream_mutations_with_capability() {
        let mut uri: Uri = "/old?x=1".parse().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("x-old", HeaderValue::from_static("1"));
        let mut state = GatewayState::default();
        let route = route();
        let service = service();
        let method = Method::GET;
        let mut cx = UpstreamRequestContext {
            request_id: "req",
            state: &mut state,
            method: &method,
            uri: &mut uri,
            headers: &mut headers,
            consumer_id: None,
            route: &route,
            service: &service,
            target_url: "http://up",
        };
        let mutation = wit::RequestMutation {
            path: Some("/new".to_string()),
            query: Some("y=2".to_string()),
            set_headers: vec![("x-new".to_string(), "yes".to_string())],
            remove_headers: vec!["x-old".to_string()],
        };
        apply_upstream_mutation(&mut cx, mutation, &[Capability::RequestHeaders]).unwrap();

        assert_eq!(cx.uri.path(), "/new");
        assert_eq!(cx.uri.query(), Some("y=2"));
        assert_eq!(cx.headers.get("x-new").unwrap().to_str().unwrap(), "yes");
        assert!(cx.headers.get("x-old").is_none());
    }

    #[test]
    fn rejects_upstream_mutations_without_capability() {
        let mut uri: Uri = "/old".parse().unwrap();
        let mut headers = HeaderMap::new();
        let mut state = GatewayState::default();
        let route = route();
        let service = service();
        let method = Method::GET;
        let mut cx = UpstreamRequestContext {
            request_id: "req",
            state: &mut state,
            method: &method,
            uri: &mut uri,
            headers: &mut headers,
            consumer_id: None,
            route: &route,
            service: &service,
            target_url: "http://up",
        };
        let mutation = wit::RequestMutation {
            path: None,
            query: None,
            set_headers: vec![("x-new".to_string(), "yes".to_string())],
            remove_headers: Vec::new(),
        };
        assert!(apply_upstream_mutation(&mut cx, mutation, &[]).is_err());
    }

    #[test]
    fn applies_response_mutations_with_capability() {
        let mut status = StatusCode::OK;
        let mut headers = HeaderMap::new();
        headers.insert("x-old", HeaderValue::from_static("1"));
        let request_headers = HeaderMap::new();
        let mut state = GatewayState::default();
        let route = route();
        let service = service();
        let mut cx = ResponseContext {
            request_id: "req",
            state: &mut state,
            request_headers: &request_headers,
            status: &mut status,
            headers: &mut headers,
            consumer_id: None,
            route: &route,
            service: &service,
            target_url: "http://up",
        };
        let mutation = wit::ResponseMutation {
            status: Some(201),
            set_headers: vec![("x-new".to_string(), "yes".to_string())],
            remove_headers: vec!["x-old".to_string()],
        };
        apply_response_mutation(&mut cx, mutation, &[Capability::ResponseHeaders]).unwrap();

        assert_eq!(*cx.status, StatusCode::CREATED);
        assert_eq!(cx.headers.get("x-new").unwrap().to_str().unwrap(), "yes");
        assert!(cx.headers.get("x-old").is_none());
    }

    #[test]
    fn rejects_response_mutations_without_capability() {
        let mut status = StatusCode::OK;
        let mut headers = HeaderMap::new();
        let request_headers = HeaderMap::new();
        let mut state = GatewayState::default();
        let route = route();
        let service = service();
        let mut cx = ResponseContext {
            request_id: "req",
            state: &mut state,
            request_headers: &request_headers,
            status: &mut status,
            headers: &mut headers,
            consumer_id: None,
            route: &route,
            service: &service,
            target_url: "http://up",
        };
        let mutation = wit::ResponseMutation {
            status: Some(201),
            set_headers: Vec::new(),
            remove_headers: Vec::new(),
        };
        assert!(apply_response_mutation(&mut cx, mutation, &[]).is_err());
    }

    #[test]
    fn requires_body_and_header_capabilities_for_body_actions() {
        let mut uri: Uri = "/".parse().unwrap();
        let mut headers = HeaderMap::new();
        let mut state = GatewayState::default();
        let route = route();
        let service = service();
        let method = Method::POST;
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

        let action = wit::RequestBodyAction::Continue(wit::RequestBodyResult {
            body: b"body".to_vec(),
            set_headers: Vec::new(),
            remove_headers: Vec::new(),
        });
        assert!(apply_request_body_action(&mut cx, action, &[]).is_err());

        let action = wit::RequestBodyAction::Continue(wit::RequestBodyResult {
            body: b"body".to_vec(),
            set_headers: vec![("x-new".to_string(), "yes".to_string())],
            remove_headers: Vec::new(),
        });
        assert!(apply_request_body_action(&mut cx, action, &[Capability::RequestBody]).is_err());

        let action = wit::RequestBodyAction::Continue(wit::RequestBodyResult {
            body: b"body".to_vec(),
            set_headers: vec![("x-new".to_string(), "yes".to_string())],
            remove_headers: Vec::new(),
        });
        let body = apply_request_body_action(
            &mut cx,
            action,
            &[Capability::RequestBody, Capability::RequestHeaders],
        )
        .unwrap();
        assert_eq!(body, Bytes::from_static(b"body"));
        assert_eq!(cx.headers.get("x-new").unwrap().to_str().unwrap(), "yes");
    }
}
