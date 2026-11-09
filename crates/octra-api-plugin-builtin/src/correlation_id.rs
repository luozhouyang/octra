//! Kong-compatible correlation ID propagation.
//!
//! Reuses an inbound correlation header when present, otherwise generates one.
//! Optional echoing mirrors Kong's `correlation-id` plugin.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use http::{HeaderName, HeaderValue};
use octra_api_plugin_types::{
    ApiGatewayPlugin, ApiGatewayPluginFactory, Capability, PluginAction, PluginError,
    PluginManifest, PluginPhase, PluginResult, RequestContext, ResponseContext,
};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::manifest;

const DEFAULT_HEADER: &str = "Kong-Request-ID";
const STATE_KEY: &str = "correlation-id";

pub struct CorrelationIdFactory;

impl ApiGatewayPluginFactory for CorrelationIdFactory {
    fn manifest(&self) -> PluginManifest {
        manifest(
            "correlation-id",
            100_001,
            vec![Capability::RequestHeaders, Capability::ResponseHeaders],
            "Propagates or generates a correlation ID header",
            json!({
                "type": "object",
                "properties": {
                    "header_name": {
                        "type": "string",
                        "default": DEFAULT_HEADER,
                        "description": "The HTTP header name to use for the correlation ID."
                    },
                    "generator": {
                        "type": "string",
                        "enum": ["uuid", "uuid#counter", "tracker"],
                        "default": "uuid#counter",
                        "description": "The generator to use for the correlation ID."
                    },
                    "echo_downstream": {
                        "type": "boolean",
                        "default": false,
                        "description": "Whether to echo the header back to the client."
                    }
                }
            }),
        )
    }

    fn create(
        &self,
        _id: &str,
        config: Value,
    ) -> Result<std::sync::Arc<dyn ApiGatewayPlugin>, PluginError> {
        let config: Config = serde_json::from_value(config).map_err(|error| {
            PluginError::fail_closed(format!("invalid correlation-id config: {error}"))
        })?;
        let header_name =
            HeaderName::from_bytes(config.header_name.as_bytes()).map_err(|error| {
                PluginError::fail_closed(format!("invalid correlation-id header_name: {error}"))
            })?;
        Ok(std::sync::Arc::new(CorrelationIdPlugin {
            manifest: self.manifest(),
            header_name,
            generator: config.generator,
            echo_downstream: config.echo_downstream,
        }))
    }

    fn phases(&self) -> Vec<PluginPhase> {
        vec![PluginPhase::Access, PluginPhase::Response, PluginPhase::Log]
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct Config {
    header_name: String,
    generator: Generator,
    echo_downstream: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            header_name: DEFAULT_HEADER.into(),
            generator: Generator::default(),
            echo_downstream: false,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
enum Generator {
    #[serde(rename = "uuid")]
    Uuid,
    #[default]
    #[serde(rename = "uuid#counter")]
    UuidCounter,
    #[serde(rename = "tracker")]
    Tracker,
}

struct CorrelationIdPlugin {
    manifest: PluginManifest,
    header_name: HeaderName,
    generator: Generator,
    echo_downstream: bool,
}

impl CorrelationIdPlugin {
    fn generate(&self, request_id: &str) -> String {
        match self.generator {
            Generator::Uuid => Uuid::new_v4().to_string(),
            Generator::UuidCounter => next_uuid_counter(),
            Generator::Tracker => request_id.to_string(),
        }
    }

    fn inbound(&self, headers: &http::HeaderMap) -> Option<String> {
        headers
            .get(&self.header_name)
            .and_then(|value| value.to_str().ok())
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    }
}

fn worker_uuid() -> &'static str {
    static WORKER_UUID: OnceLock<String> = OnceLock::new();
    WORKER_UUID.get_or_init(|| Uuid::new_v4().to_string())
}

fn next_uuid_counter() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
    format!("{}#{counter}", worker_uuid())
}

#[async_trait]
impl ApiGatewayPlugin for CorrelationIdPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    async fn access(&self, cx: &mut RequestContext<'_>) -> PluginResult {
        let correlation_id = match self.inbound(cx.headers) {
            Some(value) => value,
            None => {
                let value = self.generate(cx.request_id);
                let header = HeaderValue::from_str(&value).map_err(|error| {
                    PluginError::fail_closed(format!("invalid correlation id value: {error}"))
                })?;
                cx.headers.insert(self.header_name.clone(), header);
                value
            }
        };
        cx.state
            .values
            .insert(STATE_KEY.to_string(), Value::String(correlation_id));
        Ok(PluginAction::Continue)
    }

    async fn response(&self, cx: &mut ResponseContext<'_>) -> PluginResult {
        if !self.echo_downstream {
            return Ok(PluginAction::Continue);
        }

        let correlation_id = cx
            .state
            .values
            .get(STATE_KEY)
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| self.inbound(cx.request_headers))
            .unwrap_or_else(|| self.generate(cx.request_id));

        if let Ok(value) = HeaderValue::from_str(&correlation_id) {
            cx.headers.insert(self.header_name.clone(), value);
        }
        Ok(PluginAction::Continue)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn factory(config: Value) -> std::sync::Arc<dyn ApiGatewayPlugin> {
        CorrelationIdFactory.create("test", config).unwrap()
    }

    #[test]
    fn generator_defaults_and_overrides() {
        assert_eq!(factory(json!({})).manifest().name, "correlation-id");
        assert_eq!(
            factory(json!({"generator": "uuid"})).manifest().name,
            "correlation-id"
        );
    }

    #[test]
    fn rejects_unknown_generator_and_invalid_header() {
        assert!(
            CorrelationIdFactory
                .create("test", json!({"generator": "sequential"}))
                .is_err()
        );
        assert!(
            CorrelationIdFactory
                .create("test", json!({"header_name": "bad header"}))
                .is_err()
        );
    }
}
