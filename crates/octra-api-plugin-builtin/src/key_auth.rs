//! API key authentication.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use http::{HeaderMap, HeaderValue, StatusCode, header};
use octra_api_plugin_types::{
    ApiGatewayPlugin, ApiGatewayPluginFactory, Capability, PluginAction, PluginError,
    PluginManifest, PluginPhase, PluginResult, RequestBodyContext, RequestContext,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::transform::{
    MultipartBody, encode_form, is_form_content_type, is_json_content_type,
    is_multipart_content_type, parse_form,
};
use crate::{manifest, plugin_response, remove_query_key, resolve_anonymous, set_consumer};

const BODY_KEY: &str = "key-auth-body-key";

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
                    "run_on_preflight": { "type": "boolean" },
                    "realm": { "type": ["string", "null"] }
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
    realm: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            key_names: vec!["apikey".into()],
            key_in_header: true,
            key_in_query: true,
            key_in_body: false,
            hide_credentials: false,
            anonymous: None,
            run_on_preflight: true,
            realm: None,
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
        let mut body_name = None;
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
        if key.is_none()
            && self.config.key_in_body
            && let Some(body) = cx.state.request_body.as_deref()
            && let Some((name, value)) = body_key(cx.headers, body, &self.config.key_names)
        {
            key = Some(value);
            body_name = Some(name);
        }

        if let Some(key) = key.as_deref() {
            if let Some(entry) = cx.state.keyauth.get(key) {
                let consumer_id = entry.consumer_id.clone();
                let credential_id = entry.credential_id.clone();
                set_consumer(cx.state, &consumer_id, &credential_id, "key-auth", key);
                if self.config.hide_credentials {
                    for name in &self.config.key_names {
                        cx.headers.remove(name.as_str());
                    }
                    if let Some(uri) = remove_query_key(cx.uri, &self.config.key_names) {
                        *cx.uri = uri;
                    }
                    if let Some(name) = body_name {
                        cx.state.values.insert(BODY_KEY.into(), Value::String(name));
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

        let message = if key.is_some() {
            "Invalid authentication credentials"
        } else {
            "No API key found in request"
        };
        Ok(PluginAction::ShortCircuit(self.unauthorized(message)))
    }

    async fn on_request_buffer(
        &self,
        cx: &mut RequestBodyContext<'_>,
        body: Bytes,
    ) -> Result<Bytes, PluginError> {
        if !self.config.hide_credentials {
            return Ok(body);
        }
        let Some(name) = cx
            .state
            .values
            .get(BODY_KEY)
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            return Ok(body);
        };
        Ok(strip_body_key(cx.headers, body, &name))
    }
}

impl KeyAuthPlugin {
    fn unauthorized(&self, message: &str) -> axum::response::Response {
        let mut response = plugin_response(StatusCode::UNAUTHORIZED, message);
        let value = self
            .config
            .realm
            .as_deref()
            .and_then(|realm| HeaderValue::from_str(&format!("Key realm=\"{realm}\"")).ok())
            .unwrap_or_else(|| HeaderValue::from_static("Key"));
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, value);
        response
    }
}

fn body_key(headers: &HeaderMap, body: &[u8], names: &[String]) -> Option<(String, String)> {
    if is_json_content_type(headers) {
        let Value::Object(object) = serde_json::from_slice::<Value>(body).ok()? else {
            return None;
        };
        return names.iter().find_map(|name| {
            object
                .get(name)
                .and_then(Value::as_str)
                .map(|value| (name.clone(), value.to_string()))
        });
    }
    if is_form_content_type(headers) {
        let raw = String::from_utf8_lossy(body);
        let pairs = parse_form(&raw);
        return names.iter().find_map(|name| {
            pairs
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| (name.clone(), value.clone()))
        });
    }
    if is_multipart_content_type(headers) {
        let content_type = headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())?;
        let multipart = MultipartBody::parse(content_type, body)?;
        return names
            .iter()
            .find_map(|name| multipart.value(name).map(|value| (name.clone(), value)));
    }
    None
}

fn strip_body_key(headers: &HeaderMap, body: Bytes, name: &str) -> Bytes {
    if is_json_content_type(headers) {
        let Ok(Value::Object(mut object)) = serde_json::from_slice::<Value>(&body) else {
            return body;
        };
        object.remove(name);
        return serde_json::to_vec(&Value::Object(object))
            .map(Bytes::from)
            .unwrap_or(body);
    }
    if is_form_content_type(headers) {
        let raw = String::from_utf8_lossy(&body);
        let mut pairs = parse_form(&raw);
        pairs.retain(|(key, _)| key != name);
        return Bytes::from(encode_form(&pairs));
    }
    if is_multipart_content_type(headers) {
        let Some(content_type) = headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
        else {
            return body;
        };
        let Some(mut multipart) = MultipartBody::parse(content_type, &body) else {
            return body;
        };
        multipart.remove(name);
        return Bytes::from(multipart.to_bytes());
    }
    body
}
