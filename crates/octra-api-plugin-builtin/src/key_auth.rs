//! API key authentication.

use std::sync::Arc;

use async_trait::async_trait;
use http::{HeaderValue, StatusCode, header};
use octra_api_plugin_types::{
    ApiGatewayPlugin, ApiGatewayPluginFactory, Capability, PluginAction, PluginError,
    PluginManifest, PluginPhase, PluginResult, RequestContext,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{manifest, plugin_response, remove_query_key, resolve_anonymous, set_consumer};

pub struct KeyAuthFactory;

impl ApiGatewayPluginFactory for KeyAuthFactory {
    fn manifest(&self) -> PluginManifest {
        manifest(
            "key-auth",
            1250,
            vec![Capability::RequestHeaders, Capability::RequestBody],
            "Authenticates clients with an API key",
            json!({
                "type": "object",
                "properties": {
                    "key_names": { "type": "array", "items": { "type": "string" } },
                    "key_in_header": { "type": "boolean" },
                    "key_in_query": { "type": "boolean" },
                    "key_in_body": { "type": "boolean" },
                    "hide_credentials": { "type": "boolean" },
                    "anonymous": { "type": ["string", "null"] },
                    "run_on_preflight": { "type": "boolean" }
                }
            }),
        )
    }

    fn create(&self, _id: &str, config: Value) -> Result<Arc<dyn ApiGatewayPlugin>, PluginError> {
        let config: Config = serde_json::from_value(config).map_err(|error| {
            PluginError::fail_closed(format!("invalid key-auth config: {error}"))
        })?;
        Ok(Arc::new(KeyAuthPlugin {
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
    key_names: Vec<String>,
    key_in_header: bool,
    key_in_query: bool,
    key_in_body: bool,
    hide_credentials: bool,
    anonymous: Option<String>,
    run_on_preflight: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            key_names: vec!["apikey".into()],
            key_in_header: true,
            key_in_query: true,
            key_in_body: false,
            hide_credentials: true,
            anonymous: None,
            run_on_preflight: true,
        }
    }
}

struct KeyAuthPlugin {
    manifest: PluginManifest,
    config: Config,
}

#[async_trait]
impl ApiGatewayPlugin for KeyAuthPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    async fn on_request(&self, cx: &mut RequestContext<'_>) -> PluginResult {
        if cx.method == http::Method::OPTIONS && !self.config.run_on_preflight {
            return Ok(PluginAction::Continue);
        }

        let mut key = None;
        if self.config.key_in_header {
            for name in &self.config.key_names {
                if let Some(value) = cx
                    .headers
                    .get(name.as_str())
                    .and_then(|value| value.to_str().ok())
                {
                    key = Some(value.to_string());
                    break;
                }
            }
        }
        if key.is_none() && self.config.key_in_query {
            if let Some(query) = cx.uri.query() {
                for (name, value) in url::form_urlencoded::parse(query.as_bytes()) {
                    if self.config.key_names.iter().any(|key| key == name.as_ref()) {
                        key = Some(value.into_owned());
                        break;
                    }
                }
            }
        }
        // key_in_body requires buffered request bodies; it is intentionally
        // handled by on_request_buffer in a future revision.

        if let Some(key) = key {
            if let Some(entry) = cx.state.keyauth.get(&key) {
                let consumer_id = entry.consumer_id.clone();
                let credential_id = entry.credential_id.clone();
                set_consumer(cx.state, &consumer_id, &credential_id, "key-auth", &key);
                if self.config.hide_credentials {
                    for name in &self.config.key_names {
                        cx.headers.remove(name.as_str());
                    }
                    if let Some(uri) = remove_query_key(cx.uri, &self.config.key_names) {
                        *cx.uri = uri;
                    }
                }
                return Ok(PluginAction::Continue);
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

        let mut response = plugin_response(StatusCode::UNAUTHORIZED, "No API key found in request");
        response.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            HeaderValue::from_static("Key realm=\"octra\""),
        );
        Ok(PluginAction::ShortCircuit(response))
    }
}
