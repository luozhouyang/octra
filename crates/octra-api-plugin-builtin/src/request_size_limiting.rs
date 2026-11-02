//! Request size limiting.

use std::sync::Arc;

use async_trait::async_trait;
use http::{StatusCode, header};
use octra_api_plugin_types::{
    ApiGatewayPlugin, ApiGatewayPluginFactory, Capability, PluginAction, PluginError,
    PluginManifest, PluginPhase, PluginResult, RequestContext,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{manifest, plugin_response};

pub struct RequestSizeLimitingFactory;

impl ApiGatewayPluginFactory for RequestSizeLimitingFactory {
    fn manifest(&self) -> PluginManifest {
        manifest(
            "request-size-limiting",
            951,
            vec![Capability::RequestHeaders],
            "Rejects requests above a configured payload size",
            json!({
                "type": "object",
                "properties": {
                    "allowed_payload_size": { "type": "integer", "minimum": 1 },
                    "size_unit": { "type": "string", "enum": ["bytes", "kilobytes", "megabytes"] },
                    "require_content_length": { "type": "boolean" }
                }
            }),
        )
    }

    fn create(&self, _id: &str, config: Value) -> Result<Arc<dyn ApiGatewayPlugin>, PluginError> {
        let config: Config = serde_json::from_value(config).map_err(|error| {
            PluginError::fail_closed(format!("invalid request-size-limiting config: {error}"))
        })?;
        if config.allowed_payload_size == 0 {
            return Err(PluginError::fail_closed(
                "request-size-limiting.allowed_payload_size must be positive",
            ));
        }
        Ok(Arc::new(RequestSizeLimitingPlugin {
            manifest: self.manifest(),
            config,
        }))
    }

    fn phases(&self) -> Vec<PluginPhase> {
        vec![PluginPhase::Request, PluginPhase::Log]
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct Config {
    allowed_payload_size: u64,
    size_unit: String,
    require_content_length: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            allowed_payload_size: 128,
            size_unit: "megabytes".into(),
            require_content_length: false,
        }
    }
}

struct RequestSizeLimitingPlugin {
    manifest: PluginManifest,
    config: Config,
}

#[async_trait]
impl ApiGatewayPlugin for RequestSizeLimitingPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    async fn on_request(&self, cx: &mut RequestContext<'_>) -> PluginResult {
        let limit = match self.config.size_unit.as_str() {
            "bytes" => self.config.allowed_payload_size,
            "kilobytes" => self.config.allowed_payload_size.saturating_mul(1024),
            _ => self.config.allowed_payload_size.saturating_mul(1024 * 1024),
        };
        let length = cx
            .headers
            .get(header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok());

        match length {
            Some(length) if length > limit => Ok(PluginAction::ShortCircuit(plugin_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Request size limit exceeded",
            ))),
            None if self.config.require_content_length => Ok(PluginAction::ShortCircuit(
                plugin_response(StatusCode::LENGTH_REQUIRED, "Content-Length required"),
            )),
            _ => Ok(PluginAction::Continue),
        }
    }
}
