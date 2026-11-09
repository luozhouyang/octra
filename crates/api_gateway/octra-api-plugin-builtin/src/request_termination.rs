//! Request termination.

use std::sync::Arc;

use async_trait::async_trait;
use http::{HeaderValue, StatusCode, header};
use octra_api_plugin_types::{
    APIGatewayPlugin, APIGatewayPluginFactory, PluginAction, PluginError, PluginManifest,
    PluginPhase, PluginResult, RequestContext,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{manifest, plugin_response};

pub struct RequestTerminationFactory;

impl APIGatewayPluginFactory for RequestTerminationFactory {
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

    fn create(&self, _id: &str, config: Value) -> Result<Arc<dyn APIGatewayPlugin>, PluginError> {
        let config: Config = serde_json::from_value(config).map_err(|error| {
            PluginError::fail_closed(format!("invalid request-termination config: {error}"))
        })?;
        Ok(Arc::new(RequestTerminationPlugin {
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
impl APIGatewayPlugin for RequestTerminationPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    async fn access(&self, cx: &mut RequestContext<'_>) -> PluginResult {
        if let Some(trigger) = &self.config.trigger {
            let in_header = cx.headers.contains_key(trigger.as_str());
            let in_query = cx.uri.query().is_some_and(|query| {
                url::form_urlencoded::parse(query.as_bytes())
                    .any(|(name, _)| name.as_ref() == trigger.as_str())
            });
            if !in_header && !in_query {
                return Ok(PluginAction::Continue);
            }
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

impl octra_api_plugin_types::APIGatewayBackgroundPlugin for RequestTerminationPlugin {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{RequestFixture, response_parts, short_circuit};
    use bytes::Bytes;

    fn plugin(config: Value) -> Arc<dyn APIGatewayPlugin> {
        RequestTerminationFactory
            .create("request-termination", config)
            .expect("request-termination config")
    }

    #[test]
    fn declares_request_and_log_phases() {
        assert_eq!(
            RequestTerminationFactory.phases(),
            vec![PluginPhase::Access, PluginPhase::Log]
        );
    }

    #[tokio::test]
    async fn terminates_with_the_default_response() {
        let plugin = plugin(json!({}));
        let mut fixture = RequestFixture::default();
        let action = plugin.access(&mut fixture.context()).await.unwrap();
        let (status, headers, body) = response_parts(short_circuit(action)).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            headers
                .get(header::CONTENT_TYPE)
                .expect("content type")
                .to_str()
                .expect("ascii"),
            "application/json"
        );
        let body: Value = serde_json::from_slice(&body).expect("json body");
        assert_eq!(body["message"], "Service unavailable");
    }

    #[tokio::test]
    async fn supports_custom_status_and_message() {
        let plugin = plugin(json!({"status_code": 418, "message": "teapot"}));
        let mut fixture = RequestFixture::default();
        let action = plugin.access(&mut fixture.context()).await.unwrap();
        let (status, _, body) = response_parts(short_circuit(action)).await;
        assert_eq!(status, StatusCode::IM_A_TEAPOT);
        let body: Value = serde_json::from_slice(&body).expect("json body");
        assert_eq!(body["message"], "teapot");
    }

    #[tokio::test]
    async fn supports_raw_bodies_and_content_types() {
        let plugin = plugin(json!({
            "status_code": 200,
            "body": "hello",
            "content_type": "text/plain"
        }));
        let mut fixture = RequestFixture::default();
        let action = plugin.access(&mut fixture.context()).await.unwrap();
        let (status, headers, body) = response_parts(short_circuit(action)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            headers
                .get(header::CONTENT_TYPE)
                .expect("content type")
                .to_str()
                .expect("ascii"),
            "text/plain"
        );
        assert_eq!(body, Bytes::from_static(b"hello"));
    }

    #[tokio::test]
    async fn echo_forces_a_json_content_type() {
        let plugin = plugin(json!({
            "status_code": 201,
            "body": "created",
            "content_type": "text/plain",
            "echo": true
        }));
        let mut fixture = RequestFixture::default();
        let action = plugin.access(&mut fixture.context()).await.unwrap();
        let (status, headers, body) = response_parts(short_circuit(action)).await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(
            headers
                .get(header::CONTENT_TYPE)
                .expect("content type")
                .to_str()
                .expect("ascii"),
            "application/json"
        );
        assert_eq!(body, Bytes::from_static(b"created"));
    }

    #[tokio::test]
    async fn triggers_on_the_configured_header_or_query_parameter() {
        let plugin = plugin(json!({"trigger": "x-kill"}));
        let mut fixture = RequestFixture::default();
        let action = plugin.access(&mut fixture.context()).await.unwrap();
        assert!(matches!(action, PluginAction::Continue));

        let mut fixture = RequestFixture::default();
        fixture.headers.insert(
            header::HeaderName::from_static("x-kill"),
            HeaderValue::from_static("1"),
        );
        let action = plugin.access(&mut fixture.context()).await.unwrap();
        let (status, _, _) = response_parts(short_circuit(action)).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

        let mut fixture = RequestFixture {
            uri: "/p?x-kill=1".parse().expect("uri"),
            ..RequestFixture::default()
        };
        let action = plugin.access(&mut fixture.context()).await.unwrap();
        let (status, _, _) = response_parts(short_circuit(action)).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn invalid_statuses_fall_back_to_service_unavailable() {
        let plugin = plugin(json!({"status_code": 0}));
        let mut fixture = RequestFixture::default();
        let action = plugin.access(&mut fixture.context()).await.unwrap();
        let (status, _, _) = response_parts(short_circuit(action)).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }
}
