//! API key authentication.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use http::{HeaderMap, HeaderValue, StatusCode, header};
use octra_api_plugin_types::{
    ApiGatewayPlugin, ApiGatewayPluginFactory, Capability, PluginAction, PluginError,
    PluginManifest, PluginPhase, PluginResult, RequestContext,
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
            vec![
                Capability::RequestHeaders,
                Capability::RequestBody,
                Capability::Auth,
            ],
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
        vec![PluginPhase::Access, PluginPhase::Log]
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

    async fn access(&self, cx: &mut RequestContext<'_>) -> PluginResult {
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
        if key.is_none()
            && self.config.key_in_query
            && let Some(query) = cx.uri.query()
        {
            for (name, value) in url::form_urlencoded::parse(query.as_bytes()) {
                if self.config.key_names.iter().any(|key| key == name.as_ref()) {
                    key = Some(value.into_owned());
                    break;
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

        if let Some(key) = key.as_deref()
            && let Some(entry) = cx.state.keyauth.get(key)
        {
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
                    cx.state
                        .values
                        .insert(BODY_KEY.into(), Value::String(name.clone()));
                    let headers = cx.headers.clone();
                    if let Some(body) = cx.state.request_body.clone() {
                        cx.set_request_body(strip_body_key(&headers, body, &name));
                    }
                }
            }
            return Ok(PluginAction::Continue);
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

impl octra_api_plugin_types::ApiGatewayBackgroundPlugin for KeyAuthPlugin {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{RequestFixture, response_parts, short_circuit};
    use crate::transform::MultipartBody;
    use http::Method;
    use octra_api_plugin_types::{AuthKey, ConsumerInfo};

    const MULTIPART_TYPE: &str = "multipart/form-data; boundary=boundary";
    const MULTIPART_BODY: &str = "--boundary\r\nContent-Disposition: form-data; name=\"apikey\"\r\n\r\nsecret\r\n--boundary\r\nContent-Disposition: form-data; name=\"x\"\r\n\r\n1\r\n--boundary--\r\n";

    fn plugin(config: Value) -> Arc<dyn ApiGatewayPlugin> {
        KeyAuthFactory
            .create("key-auth", config)
            .expect("key-auth config")
    }

    fn headers(content_type: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(content_type) = content_type {
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_str(content_type).expect("content type"),
            );
        }
        headers
    }

    fn store_key(fixture: &mut RequestFixture, key: &str) {
        fixture.state.keyauth.insert(
            key.into(),
            AuthKey {
                credential_id: "cred-1".into(),
                consumer_id: "consumer-1".into(),
            },
        );
        fixture.state.consumers.insert(
            "consumer-1".into(),
            ConsumerInfo {
                id: "consumer-1".into(),
                username: Some("alice".into()),
                ..ConsumerInfo::default()
            },
        );
    }

    async fn assert_unauthorized(action: PluginAction, message: &str, challenge: &str) {
        let (status, headers, body) = response_parts(short_circuit(action)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let body: Value = serde_json::from_slice(&body).expect("json body");
        assert_eq!(body["message"], message);
        assert_eq!(
            headers
                .get(header::WWW_AUTHENTICATE)
                .expect("challenge")
                .to_str()
                .expect("ascii"),
            challenge
        );
    }

    #[test]
    fn validates_config() {
        assert!(KeyAuthFactory.create("key-auth", json!({})).is_ok());
        assert!(
            KeyAuthFactory
                .create("key-auth", json!({"key_names": []}))
                .is_ok()
        );
        assert!(
            KeyAuthFactory
                .create("key-auth", json!({"key_in_body": true}))
                .is_ok()
        );
        assert!(
            KeyAuthFactory
                .create("key-auth", json!({"anonymous": 5}))
                .is_err()
        );
        assert!(
            KeyAuthFactory
                .create("key-auth", json!({"key_names": "apikey"}))
                .is_err()
        );
        assert!(
            KeyAuthFactory
                .create("key-auth", json!({"unexpected": true}))
                .is_ok()
        );
    }

    #[test]
    fn declares_request_and_log_phases() {
        assert_eq!(
            KeyAuthFactory.phases(),
            vec![PluginPhase::Access, PluginPhase::Log]
        );
    }

    #[tokio::test]
    async fn authenticates_header_keys() {
        let plugin = plugin(json!({}));
        let mut fixture = RequestFixture::default();
        store_key(&mut fixture, "secret");
        fixture.headers.insert(
            header::HeaderName::from_static("apikey"),
            HeaderValue::from_static("secret"),
        );

        let action = plugin.access(&mut fixture.context()).await.unwrap();
        assert!(matches!(action, PluginAction::Continue));
        assert!(!fixture.state.anonymous);
        assert_eq!(
            fixture
                .state
                .consumer
                .as_ref()
                .map(|consumer| consumer.id.as_str()),
            Some("consumer-1")
        );
        assert_eq!(
            fixture
                .state
                .credential
                .as_ref()
                .map(|credential| (credential.kind.as_str(), credential.identifier.as_str())),
            Some(("key-auth", "secret"))
        );
    }

    #[tokio::test]
    async fn distinguishes_missing_and_invalid_keys() {
        let required = plugin(json!({}));

        let mut fixture = RequestFixture::default();
        let action = required.access(&mut fixture.context()).await.unwrap();
        assert_unauthorized(action, "No API key found in request", "Key").await;

        let mut fixture = RequestFixture::default();
        fixture.headers.insert(
            header::HeaderName::from_static("apikey"),
            HeaderValue::from_static("wrong"),
        );
        let action = required.access(&mut fixture.context()).await.unwrap();
        assert_unauthorized(action, "Invalid authentication credentials", "Key").await;

        let custom = plugin(json!({"realm": "internal"}));
        let mut fixture = RequestFixture::default();
        let action = custom.access(&mut fixture.context()).await.unwrap();
        assert_unauthorized(
            action,
            "No API key found in request",
            "Key realm=\"internal\"",
        )
        .await;
    }

    #[tokio::test]
    async fn honours_custom_key_names_and_sources() {
        let plugin = plugin(json!({"key_names": ["x-api-key"]}));
        let mut fixture = RequestFixture::default();
        store_key(&mut fixture, "secret");
        fixture.headers.insert(
            header::HeaderName::from_static("x-api-key"),
            HeaderValue::from_static("secret"),
        );
        let action = plugin.access(&mut fixture.context()).await.unwrap();
        assert!(matches!(action, PluginAction::Continue));

        let mut fixture = RequestFixture::default();
        store_key(&mut fixture, "secret");
        fixture.headers.insert(
            header::HeaderName::from_static("apikey"),
            HeaderValue::from_static("secret"),
        );
        let action = plugin.access(&mut fixture.context()).await.unwrap();
        assert!(matches!(action, PluginAction::ShortCircuit(_)));
    }

    #[tokio::test]
    async fn reads_keys_from_the_query_string() {
        let enabled = plugin(json!({}));
        let mut fixture = RequestFixture::default();
        store_key(&mut fixture, "secret");
        fixture.uri = "/p?apikey=secret".parse().expect("uri");
        let action = enabled.access(&mut fixture.context()).await.unwrap();
        assert!(matches!(action, PluginAction::Continue));

        let disabled = plugin(json!({"key_in_query": false}));
        let mut fixture = RequestFixture::default();
        store_key(&mut fixture, "secret");
        fixture.uri = "/p?apikey=secret".parse().expect("uri");
        let action = disabled.access(&mut fixture.context()).await.unwrap();
        assert_unauthorized(action, "No API key found in request", "Key").await;
    }

    #[tokio::test]
    async fn reads_and_strips_json_body_keys() {
        let plugin = plugin(json!({"key_in_body": true, "hide_credentials": true}));
        let mut fixture = RequestFixture::default();
        store_key(&mut fixture, "secret");
        fixture.headers = headers(Some("application/json"));
        fixture.state.request_body = Some(Bytes::from_static(br#"{"apikey":"secret","x":1}"#));

        let action = plugin.access(&mut fixture.context()).await.unwrap();
        assert!(matches!(action, PluginAction::Continue));
        assert_eq!(
            fixture.state.values.get(BODY_KEY).and_then(Value::as_str),
            Some("apikey")
        );

        let stripped: Value =
            serde_json::from_slice(fixture.state.request_body.as_deref().expect("json body"))
                .expect("json body");
        assert!(stripped.get("apikey").is_none());
        assert_eq!(stripped["x"], 1);
    }

    #[tokio::test]
    async fn reads_and_strips_form_body_keys() {
        let plugin = plugin(json!({"key_in_body": true, "hide_credentials": true}));
        let mut fixture = RequestFixture::default();
        store_key(&mut fixture, "secret");
        fixture.headers = headers(Some("application/x-www-form-urlencoded"));
        fixture.state.request_body = Some(Bytes::from_static(b"x=1&apikey=secret"));

        let action = plugin.access(&mut fixture.context()).await.unwrap();
        assert!(matches!(action, PluginAction::Continue));

        assert_eq!(
            fixture.state.request_body.as_deref(),
            Some(b"x=1".as_slice())
        );
    }

    #[tokio::test]
    async fn reads_and_strips_multipart_body_keys() {
        let plugin = plugin(json!({"key_in_body": true, "hide_credentials": true}));
        let mut fixture = RequestFixture::default();
        store_key(&mut fixture, "secret");
        fixture.headers = headers(Some(MULTIPART_TYPE));
        fixture.state.request_body = Some(Bytes::from_static(MULTIPART_BODY.as_bytes()));

        let action = plugin.access(&mut fixture.context()).await.unwrap();
        assert!(matches!(action, PluginAction::Continue));

        let stripped = fixture
            .state
            .request_body
            .as_deref()
            .expect("multipart body");
        let multipart = MultipartBody::parse(MULTIPART_TYPE, stripped).expect("multipart");
        assert_eq!(multipart.value("apikey"), None);
        assert_eq!(multipart.value("x").as_deref(), Some("1"));
    }

    #[tokio::test]
    async fn body_keys_are_ignored_unless_enabled() {
        let plugin = plugin(json!({}));
        let mut fixture = RequestFixture::default();
        store_key(&mut fixture, "secret");
        fixture.headers = headers(Some("application/json"));
        fixture.state.request_body = Some(Bytes::from_static(br#"{"apikey":"secret"}"#));

        let action = plugin.access(&mut fixture.context()).await.unwrap();
        assert_unauthorized(action, "No API key found in request", "Key").await;
    }

    #[tokio::test]
    async fn hides_headers_and_query_parameters() {
        let plugin = plugin(json!({"hide_credentials": true}));

        let mut fixture = RequestFixture::default();
        store_key(&mut fixture, "secret");
        fixture.headers.insert(
            header::HeaderName::from_static("apikey"),
            HeaderValue::from_static("secret"),
        );
        let action = plugin.access(&mut fixture.context()).await.unwrap();
        assert!(matches!(action, PluginAction::Continue));
        assert!(!fixture.headers.contains_key("apikey"));

        let mut fixture = RequestFixture::default();
        store_key(&mut fixture, "secret");
        fixture.uri = "/p?apikey=secret&x=1".parse().expect("uri");
        let action = plugin.access(&mut fixture.context()).await.unwrap();
        assert!(matches!(action, PluginAction::Continue));
        assert_eq!(fixture.uri.query(), Some("x=1"));
    }

    #[tokio::test]
    async fn skips_preflight_only_when_configured() {
        let default = plugin(json!({}));
        let mut fixture = RequestFixture {
            method: Method::OPTIONS,
            ..RequestFixture::default()
        };
        let action = default.access(&mut fixture.context()).await.unwrap();
        assert_unauthorized(action, "No API key found in request", "Key").await;

        let skipped = plugin(json!({"run_on_preflight": false}));
        let mut fixture = RequestFixture {
            method: Method::OPTIONS,
            ..RequestFixture::default()
        };
        let action = skipped.access(&mut fixture.context()).await.unwrap();
        assert!(matches!(action, PluginAction::Continue));
        assert!(fixture.state.consumer.is_none());
    }

    #[tokio::test]
    async fn falls_back_to_a_known_anonymous_consumer() {
        let known = plugin(json!({"anonymous": "anon"}));
        let mut fixture = RequestFixture::default();
        fixture.state.consumers.insert(
            "anon".into(),
            ConsumerInfo {
                id: "anon".into(),
                username: Some("anonymous".into()),
                ..ConsumerInfo::default()
            },
        );
        let action = known.access(&mut fixture.context()).await.unwrap();
        assert!(matches!(action, PluginAction::Continue));
        assert!(fixture.state.anonymous);
        assert_eq!(
            fixture
                .state
                .consumer
                .as_ref()
                .map(|consumer| consumer.id.as_str()),
            Some("anon")
        );

        let unknown = plugin(json!({"anonymous": "ghost"}));
        let mut fixture = RequestFixture::default();
        let action = unknown.access(&mut fixture.context()).await.unwrap();
        assert_unauthorized(action, "No API key found in request", "Key").await;
    }

    #[test]
    fn extracts_body_keys_from_supported_content_types() {
        let names = vec!["apikey".to_string()];

        let json = headers(Some("application/json"));
        assert_eq!(
            body_key(&json, br#"{"apikey":"secret","x":1}"#, &names),
            Some(("apikey".into(), "secret".into()))
        );
        assert_eq!(body_key(&json, br#"{"other":1}"#, &names), None);
        assert_eq!(body_key(&json, b"not json", &names), None);

        let form = headers(Some("application/x-www-form-urlencoded"));
        assert_eq!(
            body_key(&form, b"x=1&apikey=secret", &names),
            Some(("apikey".into(), "secret".into()))
        );
        assert_eq!(body_key(&form, b"x=1", &names), None);

        let multipart = headers(Some(MULTIPART_TYPE));
        assert_eq!(
            body_key(&multipart, MULTIPART_BODY.as_bytes(), &names),
            Some(("apikey".into(), "secret".into()))
        );

        assert_eq!(body_key(&HeaderMap::new(), b"apikey=secret", &names), None);
        assert_eq!(
            body_key(&headers(Some(MULTIPART_TYPE)), b"not multipart", &names),
            None
        );
    }

    #[test]
    fn strips_body_keys_only_for_supported_content_types() {
        let json = headers(Some("application/json"));
        let stripped = strip_body_key(
            &json,
            Bytes::from_static(br#"{"apikey":"secret","x":1}"#),
            "apikey",
        );
        let stripped: Value = serde_json::from_slice(&stripped).expect("json body");
        assert!(stripped.get("apikey").is_none());
        assert_eq!(stripped["x"], 1);

        let original = Bytes::from_static(b"not json");
        assert_eq!(strip_body_key(&json, original.clone(), "apikey"), original);

        let form = headers(Some("application/x-www-form-urlencoded"));
        assert_eq!(
            strip_body_key(&form, Bytes::from_static(b"x=1&apikey=secret"), "apikey"),
            Bytes::from_static(b"x=1")
        );

        let multipart = headers(Some(MULTIPART_TYPE));
        let stripped = strip_body_key(
            &multipart,
            Bytes::from_static(MULTIPART_BODY.as_bytes()),
            "apikey",
        );
        let parsed = MultipartBody::parse(MULTIPART_TYPE, &stripped).expect("multipart");
        assert_eq!(parsed.value("apikey"), None);

        let original = Bytes::from_static(b"apikey=secret");
        assert_eq!(
            strip_body_key(&HeaderMap::new(), original.clone(), "apikey"),
            original
        );
    }
}

