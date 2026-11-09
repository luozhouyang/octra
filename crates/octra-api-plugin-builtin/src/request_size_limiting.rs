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
        vec![PluginPhase::Access, PluginPhase::Log]
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

    async fn access(&self, cx: &mut RequestContext<'_>) -> PluginResult {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{RequestFixture, response_parts, short_circuit};
    use http::HeaderValue;

    fn plugin(config: Value) -> Arc<dyn ApiGatewayPlugin> {
        RequestSizeLimitingFactory
            .create("request-size-limiting", config)
            .expect("request-size-limiting config")
    }

    async fn request(
        plugin: &Arc<dyn ApiGatewayPlugin>,
        content_length: Option<&str>,
    ) -> PluginAction {
        let mut fixture = RequestFixture::default();
        if let Some(content_length) = content_length {
            fixture.headers.insert(
                header::CONTENT_LENGTH,
                HeaderValue::from_str(content_length).expect("content length"),
            );
        }
        plugin.access(&mut fixture.context()).await.unwrap()
    }

    async fn status(action: PluginAction) -> StatusCode {
        if matches!(action, PluginAction::Continue) {
            return StatusCode::OK;
        }
        let (status, _, _) = response_parts(short_circuit(action)).await;
        status
    }

    #[test]
    fn validates_config() {
        assert!(
            RequestSizeLimitingFactory
                .create("size", json!({"allowed_payload_size": 0}))
                .is_err()
        );
        assert!(
            RequestSizeLimitingFactory
                .create("size", json!({"allowed_payload_size": 1}))
                .is_ok()
        );
        assert!(
            RequestSizeLimitingFactory
                .create(
                    "size",
                    json!({"allowed_payload_size": 1, "size_unit": "kilobytes"})
                )
                .is_ok()
        );
        assert!(
            RequestSizeLimitingFactory
                .create("size", json!({"allowed_payload_size": "1"}))
                .is_err()
        );
        assert!(
            RequestSizeLimitingFactory
                .create("size", json!({"unexpected": true}))
                .is_ok()
        );
    }

    #[test]
    fn declares_request_and_log_phases() {
        assert_eq!(
            RequestSizeLimitingFactory.phases(),
            vec![PluginPhase::Access, PluginPhase::Log]
        );
    }

    #[tokio::test]
    async fn enforces_byte_limits_at_the_boundary() {
        let plugin = plugin(json!({"allowed_payload_size": 10, "size_unit": "bytes"}));
        assert_eq!(
            status(request(&plugin, Some("10")).await).await,
            StatusCode::OK
        );
        assert_eq!(
            status(request(&plugin, Some("11")).await).await,
            StatusCode::PAYLOAD_TOO_LARGE
        );
    }

    #[tokio::test]
    async fn converts_kilobyte_and_megabyte_limits() {
        let kilobytes = plugin(json!({"allowed_payload_size": 1, "size_unit": "kilobytes"}));
        assert_eq!(
            status(request(&kilobytes, Some("1024")).await).await,
            StatusCode::OK
        );
        assert_eq!(
            status(request(&kilobytes, Some("1025")).await).await,
            StatusCode::PAYLOAD_TOO_LARGE
        );

        let megabytes = plugin(json!({"allowed_payload_size": 1, "size_unit": "megabytes"}));
        assert_eq!(
            status(request(&megabytes, Some("1048576")).await).await,
            StatusCode::OK
        );
        assert_eq!(
            status(request(&megabytes, Some("1048577")).await).await,
            StatusCode::PAYLOAD_TOO_LARGE
        );

        let default = plugin(json!({"allowed_payload_size": 1}));
        assert_eq!(
            status(request(&default, Some("1048577")).await).await,
            StatusCode::PAYLOAD_TOO_LARGE
        );
    }

    #[tokio::test]
    async fn requires_content_length_only_when_configured() {
        let optional = plugin(json!({"allowed_payload_size": 10, "size_unit": "bytes"}));
        assert_eq!(status(request(&optional, None).await).await, StatusCode::OK);
        assert_eq!(
            status(request(&optional, Some("not-a-number")).await).await,
            StatusCode::OK
        );

        let required = plugin(json!({
            "allowed_payload_size": 10,
            "size_unit": "bytes",
            "require_content_length": true
        }));
        assert_eq!(
            status(request(&required, None).await).await,
            StatusCode::LENGTH_REQUIRED
        );
        assert_eq!(
            status(request(&required, Some("bad")).await).await,
            StatusCode::LENGTH_REQUIRED
        );
        assert_eq!(
            status(request(&required, Some("10")).await).await,
            StatusCode::OK
        );
    }
}
