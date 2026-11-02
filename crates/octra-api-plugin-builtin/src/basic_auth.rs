//! HTTP Basic authentication.

use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine as _;
use http::{HeaderValue, StatusCode, header};
use octra_api_plugin_types::{
    ApiGatewayPlugin, ApiGatewayPluginFactory, Capability, PluginAction, PluginError,
    PluginManifest, PluginPhase, PluginResult, RequestContext,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{manifest, plugin_response, resolve_anonymous, set_consumer};

pub struct BasicAuthFactory;

impl ApiGatewayPluginFactory for BasicAuthFactory {
    fn manifest(&self) -> PluginManifest {
        manifest(
            "basic-auth",
            1100,
            vec![Capability::RequestHeaders],
            "Authenticates clients with HTTP Basic credentials",
            json!({
                "type": "object",
                "properties": {
                    "hide_credentials": { "type": "boolean" },
                    "anonymous": { "type": ["string", "null"] },
                    "realm": { "type": "string" }
                }
            }),
        )
    }

    fn create(&self, _id: &str, config: Value) -> Result<Arc<dyn ApiGatewayPlugin>, PluginError> {
        let config: Config = serde_json::from_value(config).map_err(|error| {
            PluginError::fail_closed(format!("invalid basic-auth config: {error}"))
        })?;
        Ok(Arc::new(BasicAuthPlugin {
            manifest: self.manifest(),
            config,
        }))
    }

    fn phases(&self) -> Vec<PluginPhase> {
        vec![PluginPhase::Request, PluginPhase::Log]
    }

    fn auth(&self) -> bool {
        true
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct Config {
    hide_credentials: bool,
    anonymous: Option<String>,
    realm: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            hide_credentials: false,
            anonymous: None,
            realm: "service".into(),
        }
    }
}

struct BasicAuthPlugin {
    manifest: PluginManifest,
    config: Config,
}

#[async_trait]
impl ApiGatewayPlugin for BasicAuthPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    async fn on_request(&self, cx: &mut RequestContext<'_>) -> PluginResult {
        let credentials = cx
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Basic "))
            .and_then(|value| base64::engine::general_purpose::STANDARD.decode(value).ok())
            .and_then(|bytes| String::from_utf8(bytes).ok());

        if let Some(credentials) = credentials {
            if let Some((username, password)) = credentials.split_once(':') {
                if let Some(entry) = cx.state.basicauth.get(username).cloned() {
                    let hash = entry.password.clone();
                    let password = password.to_string();
                    let valid = tokio::task::spawn_blocking(move || {
                        bcrypt::verify(password, &hash).unwrap_or(false)
                    })
                    .await
                    .unwrap_or(false);
                    if valid {
                        set_consumer(
                            cx.state,
                            &entry.consumer_id,
                            &entry.credential_id,
                            "basic-auth",
                            &entry.username,
                        );
                        if self.config.hide_credentials {
                            cx.headers.remove(header::AUTHORIZATION);
                        }
                        return Ok(PluginAction::Continue);
                    }
                }
            }
        }

        if let Some(consumer_id) = resolve_anonymous(cx.state, self.config.anonymous.as_deref()) {
            let consumer = cx
                .state
                .consumers
                .get(&consumer_id)
                .cloned()
                .unwrap_or_default();
            cx.state.consumer = Some(octra_api_plugin_types::ConsumerInfo {
                id: consumer_id,
                ..consumer
            });
            cx.state.anonymous = true;
            return Ok(PluginAction::Continue);
        }

        let mut response = plugin_response(StatusCode::UNAUTHORIZED, "Unauthorized");
        let realm = HeaderValue::from_str(&format!("Basic realm=\"{}\"", self.config.realm))
            .unwrap_or_else(|_| HeaderValue::from_static("Basic"));
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, realm);
        Ok(PluginAction::ShortCircuit(response))
    }
}
