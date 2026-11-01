//! Ensures every request carries an `X-Request-Id` header.

use async_trait::async_trait;
use http::{HeaderValue, header};
use octra_plugin_api::{
    ApiGatewayPlugin, ApiGatewayPluginFactory, Capability, PluginError, PluginManifest,
    PluginResult, RequestContext,
};
use serde_json::{Value, json};

use crate::manifest;

pub struct RequestIdFactory;

impl ApiGatewayPluginFactory for RequestIdFactory {
    fn manifest(&self) -> PluginManifest {
        manifest(
            "request-id",
            10,
            vec![Capability::RequestHeaders],
            "Propagates or generates an X-Request-Id header",
            json!({
                "type": "object",
                "additionalProperties": false
            }),
        )
    }

    fn create(&self, _config: Value) -> Result<std::sync::Arc<dyn ApiGatewayPlugin>, PluginError> {
        Ok(std::sync::Arc::new(RequestIdPlugin {
            manifest: self.manifest(),
        }))
    }
}

struct RequestIdPlugin {
    manifest: PluginManifest,
}

#[async_trait]
impl ApiGatewayPlugin for RequestIdPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    async fn on_request(&self, cx: &mut RequestContext<'_>) -> PluginResult {
        if !cx.headers.contains_key("x-request-id")
            && let Ok(value) = HeaderValue::from_str(cx.request_id)
        {
            cx.headers
                .insert(header::HeaderName::from_static("x-request-id"), value);
        }
        Ok(octra_plugin_api::PluginAction::Continue)
    }
}
