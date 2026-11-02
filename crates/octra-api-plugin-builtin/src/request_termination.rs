//! Request termination.

use std::sync::Arc;

use async_trait::async_trait;
use http::{HeaderValue, StatusCode, header};
use octra_api_plugin_types::{
    ApiGatewayPlugin, ApiGatewayPluginFactory, PluginAction, PluginError, PluginManifest,
    PluginPhase, PluginResult, RequestContext,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{manifest, plugin_response};

pub struct RequestTerminationFactory;

impl ApiGatewayPluginFactory for RequestTerminationFactory {
    fn manifest(&self) -> PluginManifest {
        manifest(
            "request-termination",
            2,
            vec![],
            "Terminates a request with a configured response",
            json!({
                "type": "object",
                "properties": {
                    "status_code": { "type": "integer" },
                    "message": { "type": ["string", "null"] },
                    "body": { "type": ["string", "null"] },
                    "content_type": { "type": ["string", "null"] },
                    "echo": { "type": "boolean" },
                    "trigger": { "type": ["string", "null"] }
                }
            }),
        )
    }

    fn create(&self, _id: &str, config: Value) -> Result<Arc<dyn ApiGatewayPlugin>, PluginError> {
        let config: Config = serde_json::from_value(config).map_err(|error| {
            PluginError::fail_closed(format!("invalid request-termination config: {error}"))
        })?;
        Ok(Arc::new(RequestTerminationPlugin {
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
    status_code: u16,
    message: Option<String>,
    body: Option<String>,
    content_type: Option<String>,
    echo: bool,
    trigger: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            status_code: 503,
            message: None,
            body: None,
            content_type: None,
            echo: false,
            trigger: None,
        }
    }
}

struct RequestTerminationPlugin {
    manifest: PluginManifest,
    config: Config,
}

#[async_trait]
impl ApiGatewayPlugin for RequestTerminationPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    async fn on_request(&self, cx: &mut RequestContext<'_>) -> PluginResult {
        if let Some(trigger) = &self.config.trigger
            && !cx.headers.contains_key(trigger.as_str())
        {
            return Ok(PluginAction::Continue);
        }

        let status = StatusCode::from_u16(self.config.status_code)
            .unwrap_or(StatusCode::SERVICE_UNAVAILABLE);
        let mut response = if let Some(body) = &self.config.body {
            let mut response = axum::response::Response::new(axum::body::Body::from(body.clone()));
            *response.status_mut() = status;
            if let Some(content_type) = &self.config.content_type
                && let Ok(value) = HeaderValue::from_str(content_type)
            {
                response.headers_mut().insert(header::CONTENT_TYPE, value);
            }
            response
        } else {
            plugin_response(
                status,
                self.config
                    .message
                    .as_deref()
                    .unwrap_or("Service unavailable"),
            )
        };

        if self.config.echo {
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
        }
        Ok(PluginAction::ShortCircuit(response))
    }
}
