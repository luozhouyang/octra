//! Adapts a WASM component to the LLM plugin lifecycle.
//!
//! The ABI is a JSON envelope: the host serializes canonical gateway types and
//! the component returns either an unchanged decision or a mutated payload.
//! One component instance is kept per request id so stream plugins can buffer
//! across stages; the instance is dropped on the log stage and has a TTL as a
//! backstop.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use octra_api_plugin_types::{PluginError, PluginManifest, PluginPhase};
use octra_api_plugin_wasm::WasmLimits;
use octra_llm_core::{GatewayError, StreamEvent};
use octra_llm_plugin_types::{
    ALL_LLM_SCOPES, LlmAction, LlmErrorContext, LlmLogContext, LlmLogEvent, LlmPlugin,
    LlmPluginCreateContext, LlmPluginFactory, LlmPluginResult, LlmProviderInfo,
    LlmProviderRequestContext, LlmRequestContext, LlmRequestMeta, LlmResponseContext,
    LlmResponseHeaders, LlmScope, LlmStreamAction, LlmStreamContext, LlmStreamResult,
    REQUEST_LLM_SCOPES,
};
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::task::spawn_blocking;
use wasmtime::Store;
use wasmtime::component::{Component, HasSelf, Linker};

use crate::host::LlmHostState;
use crate::octra::llm_plugin::types as wit;

/// Instances older than this are recreated on the next call.
const INSTANCE_TTL: Duration = Duration::from_secs(300);

pub struct LlmWasmPluginFactory {
    manifest: PluginManifest,
    component: Arc<Component>,
    limits: WasmLimits,
    scopes: &'static [LlmScope],
    hash: String,
}

impl LlmWasmPluginFactory {
    pub(crate) fn new(
        manifest: PluginManifest,
        component: Arc<Component>,
        limits: WasmLimits,
        hash: String,
    ) -> Self {
        let scopes: &'static [LlmScope] =
            if manifest.phases.contains(&PluginPhase::Request) || manifest.phases.is_empty() {
                &REQUEST_LLM_SCOPES
            } else {
                &ALL_LLM_SCOPES
            };
        Self {
            manifest,
            component,
            limits,
            scopes,
            hash,
        }
    }

    /// Content hash of the artifact this factory was loaded from.
    pub fn component_hash(&self) -> &str {
        &self.hash
    }
}

impl LlmPluginFactory for LlmWasmPluginFactory {
    fn manifest(&self) -> PluginManifest {
        self.manifest.clone()
    }

    fn supported_scopes(&self) -> &'static [LlmScope] {
        self.scopes
    }

    fn create(&self, _config: Value) -> Result<Arc<dyn LlmPlugin>, PluginError> {
        Err(PluginError::fail_closed(
            "wasm plugins require the host create context",
        ))
    }

    fn validate(&self, config: &Value) -> Result<(), PluginError> {
        if !config.is_object() {
            return Err(PluginError::fail_closed(
                "wasm plugin configuration must be a JSON object",
            ));
        }
        Ok(())
    }

    fn create_with(
        &self,
        config: Value,
        context: &LlmPluginCreateContext<'_>,
    ) -> Result<Arc<dyn LlmPlugin>, PluginError> {
        let config_json = serde_json::to_string(&config).map_err(|error| {
            PluginError::fail_closed(format!("cannot serialize wasm plugin config: {error}"))
        })?;
        let plugin = LlmWasmPlugin {
            manifest: self.manifest.clone(),
            component: self.component.clone(),
            config_json,
            limits: self.limits.clone(),
            scope: context.scope.to_string(),
            scope_id: context.scope_id.to_string(),
            phases: phases(&self.manifest),
            instances: Mutex::new(HashMap::new()),
            max_instances: if self.limits.max_inflight == 0 {
                1024
            } else {
                self.limits.max_inflight
            },
        };
        // Instantiate once during chain compilation so a configuration the
        // guest rejects fails the control write instead of every request.
        plugin.instantiate()?;
        Ok(Arc::new(plugin))
    }
}

pub struct LlmWasmPlugin {
    manifest: PluginManifest,
    component: Arc<Component>,
    config_json: String,
    limits: WasmLimits,
    scope: String,
    scope_id: String,
    phases: Vec<PluginPhase>,
    instances: Mutex<HashMap<String, InstanceEntry>>,
    max_instances: usize,
}

struct InstanceEntry {
    instance: Arc<Mutex<WasmInstance>>,
    last_used: Instant,
}

struct WasmInstance {
    store: Store<LlmHostState>,
    bindings: crate::LlmPlugin,
}

enum CallError {
    Plugin(String),
    Trap(String),
}

impl LlmWasmPlugin {
    fn handles(&self, stage: PluginPhase) -> bool {
        self.phases.contains(&stage)
    }

    fn instance_for(&self, request_id: &str) -> Result<Arc<Mutex<WasmInstance>>, PluginError> {
        let now = Instant::now();
        let mut instances = self.instances.lock();
        instances.retain(|_, entry| now.duration_since(entry.last_used) < INSTANCE_TTL);
        if let Some(entry) = instances.get_mut(request_id) {
            entry.last_used = now;
            return Ok(entry.instance.clone());
        }
        if instances.len() >= self.max_instances {
            // Requests that never reach the log stage (early errors, client
            // disconnects) rely on this eviction instead of stamping out every
            // new request once the cap is reached.
            if let Some(oldest) = instances
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(request_id, _)| request_id.clone())
            {
                instances.remove(&oldest);
                tracing::warn!(
                    plugin = %self.manifest.name,
                    request_id = %oldest,
                    "evicting the oldest LLM wasm instance at the capacity limit"
                );
            }
        }
        let instance = Arc::new(Mutex::new(self.instantiate()?));
        instances.insert(
            request_id.to_string(),
            InstanceEntry {
                instance: instance.clone(),
                last_used: now,
            },
        );
        Ok(instance)
    }

    fn drop_instance(&self, request_id: &str) {
        self.instances.lock().remove(request_id);
    }

    fn instantiate(&self) -> Result<WasmInstance, PluginError> {
        let mut store = Store::new(
            self.component.engine(),
            LlmHostState::new(&self.manifest.name, self.limits.max_memory_bytes),
        );
        store.limiter(|state| &mut state.limits);
        arm(&mut store, &self.limits)?;
        let mut linker = Linker::new(self.component.engine());
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker).map_err(|error| {
            PluginError::fail_closed(format!("cannot link wasi for LLM plugin: {error}"))
        })?;
        crate::LlmPlugin::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state).map_err(
            |error| PluginError::fail_closed(format!("cannot link LLM plugin host: {error}")),
        )?;
        let bindings = crate::LlmPlugin::instantiate(&mut store, &self.component, &linker)
            .map_err(|error| {
                PluginError::fail_closed(format!("cannot instantiate LLM plugin: {error}"))
            })?;
        arm(&mut store, &self.limits)?;
        bindings
            .octra_llm_plugin_plugin_api()
            .call_configure(&mut store, &self.config_json)
            .map_err(|error| {
                PluginError::fail_closed(format!("cannot configure LLM plugin: {error}"))
            })?
            .map_err(|message| PluginError {
                message: format!("LLM plugin configure failed: {message}"),
                fail_policy: self.manifest.fail_policy,
            })?;
        Ok(WasmInstance { store, bindings })
    }

    async fn invoke(
        &self,
        meta: &LlmRequestMeta<'_>,
        stage: wit::Stage,
        payload: Value,
        state: &mut Value,
        headers: Option<&mut LlmResponseHeaders>,
    ) -> Result<wit::Outcome, PluginError> {
        let request_id = meta.request_id.to_string();
        let instance = self.instance_for(&request_id)?;
        let limits = self.limits.clone();
        let invocation = wit::Invocation {
            stage,
            request_id: request_id.clone(),
            scope: self.scope.clone(),
            scope_id: self.scope_id.clone(),
            config_json: self.config_json.clone(),
            meta_json: serde_json::to_string(&meta_json(meta)).map_err(|error| {
                PluginError::fail_closed(format!("cannot serialize LLM request metadata: {error}"))
            })?,
            payload_json: payload.to_string(),
            state_json: state.to_string(),
        };
        let fail_policy = self.manifest.fail_policy;
        let result = spawn_blocking(move || {
            let mut guard = instance.lock();
            let WasmInstance { store, bindings } = &mut *guard;
            arm(store, &limits).map_err(|error| CallError::Trap(error.message))?;
            match bindings
                .octra_llm_plugin_plugin_api()
                .call_on_stage(store, &invocation)
            {
                Ok(Ok(outcome)) => Ok(outcome),
                Ok(Err(message)) => Err(CallError::Plugin(message)),
                Err(error) => Err(CallError::Trap(error.to_string())),
            }
        })
        .await
        .map_err(|error| {
            PluginError::fail_closed(format!("LLM wasm plugin task failed: {error}"))
        })?;
        let outcome = match result {
            Ok(outcome) => outcome,
            Err(CallError::Plugin(message)) => {
                return Err(PluginError {
                    message: format!("LLM plugin failed: {message}"),
                    fail_policy,
                });
            }
            Err(CallError::Trap(message)) => {
                self.drop_instance(&request_id);
                return Err(PluginError::fail_closed(format!(
                    "LLM wasm plugin trapped: {message}"
                )));
            }
        };
        if let Some(headers) = headers {
            for (name, value) in &outcome.headers {
                headers.insert(name, value);
            }
        }
        let next_state = if outcome.state_json.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&outcome.state_json).map_err(|error| {
                PluginError::fail_closed(format!("LLM plugin returned invalid state JSON: {error}"))
            })?
        };
        *state = next_state;
        Ok(outcome)
    }

    fn parse_action(outcome: &wit::Outcome) -> Result<&str, PluginError> {
        match outcome.action.as_str() {
            "continue" => Ok("continue"),
            "ignore" => Ok("ignore"),
            "reject" => Ok("reject"),
            "replace" => Ok("replace"),
            other => Err(PluginError::fail_closed(format!(
                "LLM plugin returned unknown action '{other}'"
            ))),
        }
    }

    fn parse_rejection(outcome: &wit::Outcome) -> Result<GatewayError, PluginError> {
        let raw = outcome.error_json.as_deref().ok_or_else(|| {
            PluginError::fail_closed("LLM plugin rejected without an error payload")
        })?;
        serde_json::from_str(raw).map_err(|error| {
            PluginError::fail_closed(format!("LLM plugin returned invalid error JSON: {error}"))
        })
    }
}

#[async_trait]
impl LlmPlugin for LlmWasmPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    async fn on_request(&self, cx: &mut LlmRequestContext<'_>) -> LlmPluginResult {
        if !self.handles(PluginPhase::Request) {
            return Ok(LlmAction::Continue);
        }
        let payload = json!({"request": cx.request});
        let outcome = self
            .invoke(
                cx.meta,
                wit::Stage::Request,
                payload,
                cx.state,
                Some(cx.headers),
            )
            .await?;
        if Self::parse_action(&outcome)? == "reject" {
            return Ok(LlmAction::Reject(Self::parse_rejection(&outcome)?));
        }
        if let Some(payload) = &outcome.payload_json {
            let value: Value = serde_json::from_str(payload).map_err(|error| {
                PluginError::fail_closed(format!(
                    "LLM plugin returned invalid request JSON: {error}"
                ))
            })?;
            if let Some(request) = value.get("request") {
                *cx.request = serde_json::from_value(request.clone()).map_err(|error| {
                    PluginError::fail_closed(format!(
                        "LLM plugin returned an invalid ChatRequest: {error}"
                    ))
                })?;
            }
        }
        Ok(LlmAction::Continue)
    }

    async fn on_provider_request(&self, cx: &mut LlmProviderRequestContext<'_>) -> LlmPluginResult {
        if !self.handles(PluginPhase::UpstreamRequest) {
            return Ok(LlmAction::Continue);
        }
        let payload = json!({"provider": provider_json(cx.provider), "request": cx.request});
        let outcome = self
            .invoke(
                cx.meta,
                wit::Stage::ProviderRequest,
                payload,
                cx.state,
                None,
            )
            .await?;
        if Self::parse_action(&outcome)? == "reject" {
            return Ok(LlmAction::Reject(Self::parse_rejection(&outcome)?));
        }
        if let Some(payload) = &outcome.payload_json {
            let value: Value = serde_json::from_str(payload).map_err(|error| {
                PluginError::fail_closed(format!(
                    "LLM plugin returned invalid request JSON: {error}"
                ))
            })?;
            if let Some(request) = value.get("request") {
                *cx.request = serde_json::from_value(request.clone()).map_err(|error| {
                    PluginError::fail_closed(format!(
                        "LLM plugin returned an invalid ChatRequest: {error}"
                    ))
                })?;
            }
        }
        Ok(LlmAction::Continue)
    }

    async fn on_response(&self, cx: &mut LlmResponseContext<'_>) -> LlmPluginResult {
        if !self.handles(PluginPhase::Response) {
            return Ok(LlmAction::Continue);
        }
        let payload = json!({"response": cx.response, "usage": cx.usage});
        let outcome = self
            .invoke(cx.meta, wit::Stage::Response, payload, cx.state, None)
            .await?;
        if Self::parse_action(&outcome)? == "reject" {
            return Ok(LlmAction::Reject(Self::parse_rejection(&outcome)?));
        }
        if let Some(payload) = &outcome.payload_json {
            let value: Value = serde_json::from_str(payload).map_err(|error| {
                PluginError::fail_closed(format!(
                    "LLM plugin returned invalid response JSON: {error}"
                ))
            })?;
            if let Some(response) = value.get("response") {
                *cx.response = serde_json::from_value(response.clone()).map_err(|error| {
                    PluginError::fail_closed(format!(
                        "LLM plugin returned an invalid ChatResponse: {error}"
                    ))
                })?;
            }
            if let Some(usage) = value.get("usage") {
                *cx.usage = serde_json::from_value(usage.clone()).map_err(|error| {
                    PluginError::fail_closed(format!("LLM plugin returned invalid usage: {error}"))
                })?;
            }
        }
        Ok(LlmAction::Continue)
    }

    async fn on_stream_event(&self, cx: &mut LlmStreamContext<'_>) -> LlmStreamResult {
        if !self.handles(PluginPhase::ResponseBody) {
            return Ok(LlmStreamAction::Continue);
        }
        let payload = json!({"event": cx.event});
        let outcome = self
            .invoke(cx.meta, wit::Stage::StreamEvent, payload, cx.state, None)
            .await?;
        match Self::parse_action(&outcome)? {
            "reject" => return Ok(LlmStreamAction::Reject(Self::parse_rejection(&outcome)?)),
            "replace" => {
                let payload = outcome.payload_json.as_deref().ok_or_else(|| {
                    PluginError::fail_closed("LLM plugin replaced events without a payload")
                })?;
                let value: Value = serde_json::from_str(payload).map_err(|error| {
                    PluginError::fail_closed(format!(
                        "LLM plugin returned invalid stream payload: {error}"
                    ))
                })?;
                let events: Vec<StreamEvent> =
                    serde_json::from_value(value.get("events").cloned().unwrap_or(Value::Null))
                        .map_err(|error| {
                            PluginError::fail_closed(format!(
                                "LLM plugin returned invalid stream events: {error}"
                            ))
                        })?;
                return Ok(LlmStreamAction::Replace(events));
            }
            _ => {}
        }
        if let Some(payload) = &outcome.payload_json {
            let value: Value = serde_json::from_str(payload).map_err(|error| {
                PluginError::fail_closed(format!("LLM plugin returned invalid event JSON: {error}"))
            })?;
            if let Some(event) = value.get("event") {
                *cx.event = serde_json::from_value(event.clone()).map_err(|error| {
                    PluginError::fail_closed(format!(
                        "LLM plugin returned an invalid StreamEvent: {error}"
                    ))
                })?;
            }
        }
        Ok(LlmStreamAction::Continue)
    }

    async fn on_error(&self, cx: &mut LlmErrorContext<'_>) -> LlmPluginResult {
        if !self.handles(PluginPhase::Error) {
            return Ok(LlmAction::Continue);
        }
        let payload = json!({
            "provider_id": cx.provider_id,
            "account_id": cx.account_id,
            "error": cx.error,
        });
        let outcome = self
            .invoke(cx.meta, wit::Stage::Error, payload, cx.state, None)
            .await?;
        if Self::parse_action(&outcome)? == "reject" {
            *cx.error = Self::parse_rejection(&outcome)?;
            return Ok(LlmAction::Continue);
        }
        if let Some(payload) = &outcome.payload_json {
            let value: Value = serde_json::from_str(payload).map_err(|error| {
                PluginError::fail_closed(format!("LLM plugin returned invalid error JSON: {error}"))
            })?;
            if let Some(error) = value.get("error") {
                *cx.error = serde_json::from_value(error.clone()).map_err(|error| {
                    PluginError::fail_closed(format!(
                        "LLM plugin returned an invalid GatewayError: {error}"
                    ))
                })?;
            }
        }
        Ok(LlmAction::Continue)
    }

    async fn on_log(&self, cx: &mut LlmLogContext<'_>) {
        if self.handles(PluginPhase::Log) {
            let payload = json!({"event": log_json(cx.event)});
            if let Err(error) = self
                .invoke(cx.meta, wit::Stage::Log, payload, cx.state, None)
                .await
            {
                tracing::warn!(
                    plugin = %self.manifest.name,
                    request_id = cx.meta.request_id,
                    %error,
                    "LLM wasm plugin log stage failed"
                );
            }
        }
        self.drop_instance(cx.meta.request_id);
    }
}

fn arm(store: &mut Store<LlmHostState>, limits: &WasmLimits) -> Result<(), PluginError> {
    store
        .set_fuel(limits.max_fuel)
        .map_err(|error| PluginError::fail_closed(format!("cannot arm wasm fuel: {error}")))?;
    store.set_epoch_deadline(epoch_ticks(limits.timeout));
    Ok(())
}

/// Instantiates the component without calling `configure`, so an artifact
/// that does not export the LLM world is rejected at load time instead of on
/// the first request.
pub(crate) fn probe_component(
    component: &Component,
    limits: &WasmLimits,
    plugin: &str,
) -> Result<(), PluginError> {
    let mut store = Store::new(
        component.engine(),
        LlmHostState::new(plugin, limits.max_memory_bytes),
    );
    store.limiter(|state| &mut state.limits);
    arm(&mut store, limits)?;
    let mut linker = Linker::new(component.engine());
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker).map_err(|error| {
        PluginError::fail_closed(format!("cannot link wasi for LLM plugin: {error}"))
    })?;
    crate::LlmPlugin::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state).map_err(
        |error| PluginError::fail_closed(format!("cannot link LLM plugin host: {error}")),
    )?;
    crate::LlmPlugin::instantiate(&mut store, component, &linker).map_err(|error| {
        PluginError::fail_closed(format!("cannot instantiate LLM plugin '{plugin}': {error}"))
    })?;
    Ok(())
}

fn epoch_ticks(timeout: Duration) -> u64 {
    let millis = timeout.as_millis().max(1) as u64;
    (millis / 10).max(1).saturating_sub(1).max(1)
}

fn phases(manifest: &PluginManifest) -> Vec<PluginPhase> {
    if manifest.phases.is_empty() {
        vec![
            PluginPhase::Request,
            PluginPhase::Response,
            PluginPhase::Log,
        ]
    } else {
        manifest.phases.clone()
    }
}

fn meta_json(meta: &LlmRequestMeta<'_>) -> Value {
    json!({
        "request_id": meta.request_id,
        "ingress": meta.ingress.as_str(),
        "consumer_id": meta.consumer_id,
        "virtual_key_id": meta.virtual_key_id,
        "virtual_key_family": meta.virtual_key_family,
        "consumer_group_ids": meta.consumer_group_ids,
        "session_id": meta.session_id,
        "client_ip": meta.client_ip.map(|ip| ip.to_string()),
        "user_agent": meta.user_agent,
        "intent": meta.intent,
    })
}

fn provider_json(provider: &LlmProviderInfo<'_>) -> Value {
    json!({
        "attempt": provider.attempt,
        "target_model_id": provider.target_model_id,
        "provider_id": provider.provider_id,
        "provider_name": provider.provider_name,
        "protocol": provider.protocol.as_str(),
        "account_id": provider.account_id,
        "account_name": provider.account_name,
    })
}

fn log_json(event: &LlmLogEvent<'_>) -> Value {
    json!({
        "model": event.model,
        "requested_model": event.requested_model,
        "provider_id": event.provider_id,
        "provider_name": event.provider_name,
        "account_id": event.account_id,
        "account_name": event.account_name,
        "status": event.status,
        "latency_ms": event.latency_ms,
        "usage": event.usage,
        "finish_reason": event.finish_reason,
        "streaming": event.streaming,
        "error": event.error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use octra_llm_core::{Protocol, Usage};

    fn outcome(action: &str) -> wit::Outcome {
        wit::Outcome {
            action: action.to_string(),
            payload_json: None,
            state_json: "null".to_string(),
            error_json: None,
            headers: Vec::new(),
        }
    }

    fn meta() -> LlmRequestMeta<'static> {
        LlmRequestMeta {
            request_id: "req-1",
            ingress: Protocol::Openai,
            consumer_id: Some("consumer-1"),
            virtual_key_id: Some("key-1"),
            virtual_key_family: Some("family-1"),
            consumer_group_ids: &[],
            session_id: Some("session-1"),
            client_ip: Some("127.0.0.1".parse().unwrap()),
            user_agent: Some("agent/1.0"),
            intent: Some("coding"),
        }
    }

    #[test]
    fn epoch_ticks_floor_at_one() {
        assert_eq!(epoch_ticks(Duration::from_millis(1)), 1);
        assert_eq!(epoch_ticks(Duration::from_millis(50)), 4);
        assert_eq!(epoch_ticks(Duration::from_millis(250)), 24);
    }

    #[test]
    fn default_phases_cover_request_response_log() {
        let manifest = PluginManifest {
            name: "x".into(),
            version: "1".into(),
            kind: octra_api_plugin_types::PluginKind::Llm,
            runtime: octra_api_plugin_types::PluginRuntimeKind::Wasm,
            priority: 0,
            description: String::new(),
            timeout_ms: 100,
            fail_policy: octra_api_plugin_types::FailPolicy::Open,
            phases: Vec::new(),
            capabilities: Vec::new(),
            config_schema: None,
            api_version: "2".into(),
            entities: Vec::new(),
            migrations: Vec::new(),
            admin_routes: Vec::new(),
            subscriptions: Vec::new(),
            jobs: Vec::new(),
        };
        assert_eq!(
            phases(&manifest),
            vec![
                PluginPhase::Request,
                PluginPhase::Response,
                PluginPhase::Log
            ]
        );
    }

    #[test]
    fn parses_known_actions_and_rejects_unknown_ones() {
        for action in ["continue", "ignore", "reject", "replace"] {
            assert_eq!(
                LlmWasmPlugin::parse_action(&outcome(action)).unwrap(),
                action
            );
        }
        assert!(LlmWasmPlugin::parse_action(&outcome("explode")).is_err());
    }

    #[test]
    fn rejection_requires_a_valid_error_payload() {
        assert!(LlmWasmPlugin::parse_rejection(&outcome("reject")).is_err());

        let mut with_error = outcome("reject");
        with_error.error_json = Some(
            json!({"status": 429, "kind": "rate_limited", "message": "slow down"}).to_string(),
        );
        let error = LlmWasmPlugin::parse_rejection(&with_error).unwrap();
        assert_eq!(error.status, 429);
        assert_eq!(error.message, "slow down");

        let mut invalid = outcome("reject");
        invalid.error_json = Some("not-json".to_string());
        assert!(LlmWasmPlugin::parse_rejection(&invalid).is_err());
    }

    #[test]
    fn metadata_payload_contains_identity_without_headers() {
        let value = meta_json(&meta());
        assert_eq!(value["request_id"], "req-1");
        assert_eq!(value["consumer_id"], "consumer-1");
        assert_eq!(value["virtual_key_family"], "family-1");
        assert_eq!(value["client_ip"], "127.0.0.1");
        assert_eq!(value["ingress"], "openai");
        assert!(meta_json(&meta()).get("headers").is_none());
    }

    #[test]
    fn log_payload_is_content_free() {
        let usage = Usage::default();
        let event = LlmLogEvent {
            model: "effective",
            requested_model: Some("alias"),
            provider_id: Some("provider-1"),
            provider_name: Some("openai"),
            account_id: Some("account-1"),
            account_name: Some("primary"),
            status: 200,
            latency_ms: 12,
            usage: &usage,
            finish_reason: Some("stop"),
            streaming: false,
            error: None,
            cache_status: None,
        };
        let value = log_json(&event);
        assert_eq!(value["requested_model"], "alias");
        assert_eq!(value["model"], "effective");
        assert!(value.get("messages").is_none());
        assert!(value.get("prompt").is_none());
    }
}
