//! Per-service circuit breaking around upstream failures and 5xx responses.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::body::Body;
use axum::response::Response;
use http::{HeaderValue, StatusCode, header};
use octra_plugin_api::{
    ApiGatewayPlugin, ApiGatewayPluginFactory, Capability, ErrorContext, PluginAction, PluginError,
    PluginManifest, PluginResult, RequestContext, ResponseContext,
};
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::manifest;

const DEFAULT_FAILURE_THRESHOLD: u32 = 5;
const DEFAULT_OPEN_SECONDS: u64 = 30;

pub struct CircuitBreakerFactory;

impl ApiGatewayPluginFactory for CircuitBreakerFactory {
    fn manifest(&self) -> PluginManifest {
        manifest(
            "circuit-breaker",
            15,
            vec![Capability::Log],
            "Opens a per-service circuit after repeated upstream failures",
            json!({
                "type": "object",
                "properties": {
                    "failure_threshold": { "type": "integer", "minimum": 1 },
                    "open_seconds": { "type": "integer", "minimum": 1 }
                }
            }),
        )
    }

    fn phases(&self) -> Vec<octra_plugin_api::PluginPhase> {
        vec![
            octra_plugin_api::PluginPhase::Request,
            octra_plugin_api::PluginPhase::Response,
            octra_plugin_api::PluginPhase::Log,
        ]
    }

    fn create(&self, _id: &str, config: Value) -> Result<Arc<dyn ApiGatewayPlugin>, PluginError> {
        let config: CircuitConfig = serde_json::from_value(config).map_err(|error| {
            PluginError::fail_closed(format!("invalid circuit-breaker config: {error}"))
        })?;
        if config.failure_threshold == 0 {
            return Err(PluginError::fail_closed(
                "circuit-breaker.failure_threshold must be at least 1",
            ));
        }
        if config.open_seconds == 0 {
            return Err(PluginError::fail_closed(
                "circuit-breaker.open_seconds must be at least 1",
            ));
        }
        Ok(Arc::new(CircuitBreakerPlugin {
            manifest: self.manifest(),
            config,
            breakers: Mutex::new(HashMap::new()),
        }))
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct CircuitConfig {
    failure_threshold: u32,
    open_seconds: u64,
}

impl Default for CircuitConfig {
    fn default() -> Self {
        Self {
            failure_threshold: DEFAULT_FAILURE_THRESHOLD,
            open_seconds: DEFAULT_OPEN_SECONDS,
        }
    }
}

struct CircuitBreakerPlugin {
    manifest: PluginManifest,
    config: CircuitConfig,
    breakers: Mutex<HashMap<String, Breaker>>,
}

#[derive(Default)]
struct Breaker {
    failures: u32,
    open_until: Option<Instant>,
    probing: bool,
}

#[async_trait]
impl ApiGatewayPlugin for CircuitBreakerPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    async fn on_request(&self, cx: &mut RequestContext<'_>) -> PluginResult {
        let key = cx.service.id.as_str();
        let open_for = {
            let mut breakers = self.breakers.lock();
            breakers
                .entry(key.to_string())
                .or_default()
                .check(Instant::now())
        };
        match open_for {
            None => Ok(PluginAction::Continue),
            Some(retry_after) => Ok(PluginAction::ShortCircuit(open_circuit(retry_after))),
        }
    }

    async fn on_response(&self, cx: &mut ResponseContext<'_>) -> PluginResult {
        let key = cx.service.id.as_str();
        let mut breakers = self.breakers.lock();
        let breaker = breakers.entry(key.to_string()).or_default();
        if cx.status.is_server_error() {
            breaker.record_failure(&self.config, Instant::now());
        } else {
            breaker.record_success();
        }
        Ok(PluginAction::Continue)
    }

    async fn on_error(&self, cx: &mut ErrorContext<'_>) -> PluginResult {
        let key = cx.service.id.as_str();
        self.breakers
            .lock()
            .entry(key.to_string())
            .or_default()
            .record_failure(&self.config, Instant::now());
        Ok(PluginAction::Continue)
    }
}

impl Breaker {
    /// `None` allows the request. `Some(seconds)` reports how long the circuit
    /// stays open; after the cooldown one probe request is let through.
    fn check(&mut self, now: Instant) -> Option<u64> {
        let open_until = self.open_until?;
        if now < open_until {
            return Some(open_until.saturating_duration_since(now).as_secs().max(1));
        }
        if self.probing {
            return Some(1);
        }
        self.probing = true;
        None
    }

    fn record_failure(&mut self, config: &CircuitConfig, now: Instant) {
        self.failures = self.failures.saturating_add(1);
        if self.open_until.is_some() || self.failures >= config.failure_threshold {
            self.open(config, now);
        }
    }

    fn record_success(&mut self) {
        self.failures = 0;
        self.open_until = None;
        self.probing = false;
    }

    fn open(&mut self, config: &CircuitConfig, now: Instant) {
        self.open_until = Some(now + Duration::from_secs(config.open_seconds));
        self.probing = false;
    }
}

fn open_circuit(retry_after: u64) -> Response {
    let body = json!({
        "error": {
            "code": "circuit_open",
            "message": "upstream circuit is open"
        }
    })
    .to_string();
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    if let Ok(value) = HeaderValue::from_str(&retry_after.to_string()) {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{HeaderMap, Method, StatusCode, Uri};
    use octra_api_core::{Route, Service};
    use octra_plugin_api::GatewayState;

    fn service() -> Service {
        Service {
            id: "svc".into(),
            name: "svc".into(),
            protocol: "http".into(),
            host: "up.test".into(),
            port: 80,
            path: None,
            retries: 0,
            connect_timeout: 1000,
            write_timeout: 1000,
            read_timeout: 1000,
            enabled: true,
            description: String::new(),
            created_at: 0,
            updated_at: 0,
        }
    }

    fn route() -> Route {
        Route {
            id: "route".into(),
            name: None,
            protocols: vec!["http".into()],
            methods: Vec::new(),
            hosts: Vec::new(),
            paths: vec!["/".into()],
            headers: Default::default(),
            strip_path: false,
            preserve_host: false,
            request_buffering: true,
            response_buffering: true,
            regex_priority: 0,
            service_id: Some("svc".into()),
            enabled: true,
            tags: Vec::new(),
            created_at: 0,
            updated_at: 0,
        }
    }

    async fn request(plugin: &dyn ApiGatewayPlugin) -> PluginAction {
        let service = service();
        let route = route();
        let method = Method::GET;
        let mut uri: Uri = "/".parse().unwrap();
        let mut headers = HeaderMap::new();
        let mut state = GatewayState::default();
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
        plugin.on_request(&mut cx).await.unwrap()
    }

    async fn fail(plugin: &dyn ApiGatewayPlugin) {
        let service = service();
        let route = route();
        let method = Method::GET;
        let uri: Uri = "/".parse().unwrap();
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
            target_url: "http://up.test",
            error: "connect",
        };
        plugin.on_error(&mut cx).await.unwrap();
    }

    async fn respond(plugin: &dyn ApiGatewayPlugin, status: StatusCode) {
        let service = service();
        let route = route();
        let mut status = status;
        let mut headers = HeaderMap::new();
        let request_headers = HeaderMap::new();
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
            target_url: "http://up.test",
        };
        plugin.on_response(&mut cx).await.unwrap();
    }

    #[tokio::test]
    async fn opens_after_threshold_and_short_circuits() {
        let plugin = CircuitBreakerFactory
            .create("test", json!({"failure_threshold": 2, "open_seconds": 30}))
            .unwrap();
        assert!(matches!(
            request(plugin.as_ref()).await,
            PluginAction::Continue
        ));
        fail(plugin.as_ref()).await;
        assert!(matches!(
            request(plugin.as_ref()).await,
            PluginAction::Continue
        ));
        fail(plugin.as_ref()).await;

        match request(plugin.as_ref()).await {
            PluginAction::ShortCircuit(response) => {
                assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
                assert!(response.headers().get(header::RETRY_AFTER).is_some());
            }
            PluginAction::Continue => panic!("circuit should be open"),
        }
    }

    #[tokio::test]
    async fn success_resets_failure_count() {
        let plugin = CircuitBreakerFactory
            .create("test", json!({"failure_threshold": 2, "open_seconds": 30}))
            .unwrap();
        fail(plugin.as_ref()).await;
        respond(plugin.as_ref(), StatusCode::OK).await;
        fail(plugin.as_ref()).await;
        assert!(matches!(
            request(plugin.as_ref()).await,
            PluginAction::Continue
        ));
    }

    #[tokio::test]
    async fn upstream_5xx_counts_as_failure() {
        let plugin = CircuitBreakerFactory
            .create("test", json!({"failure_threshold": 1, "open_seconds": 30}))
            .unwrap();
        respond(plugin.as_ref(), StatusCode::BAD_GATEWAY).await;
        assert!(matches!(
            request(plugin.as_ref()).await,
            PluginAction::ShortCircuit(_)
        ));
    }

    #[test]
    fn rejects_zero_threshold() {
        let Err(error) = CircuitBreakerFactory.create("test", json!({"failure_threshold": 0}))
        else {
            panic!("expected an error");
        };
        assert!(error.message.contains("failure_threshold"));
    }
}
