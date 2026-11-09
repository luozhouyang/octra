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
            vec![Capability::RequestHeaders, Capability::Auth],
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
        vec![PluginPhase::Access, PluginPhase::Log]
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

    async fn access(&self, cx: &mut RequestContext<'_>) -> PluginResult {
        let credentials = cx
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Basic "))
            .and_then(|value| base64::engine::general_purpose::STANDARD.decode(value).ok())
            .and_then(|bytes| String::from_utf8(bytes).ok());

        if let Some(credentials) = credentials
            && let Some((username, password)) = credentials.split_once(':')
            && let Some(entry) = cx.state.basicauth.get(username).cloned()
        {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{RequestFixture, response_parts, short_circuit};
    use octra_api_plugin_types::{AuthBasic, ConsumerInfo};

    fn plugin(config: Value) -> Arc<dyn ApiGatewayPlugin> {
        BasicAuthFactory
            .create("basic-auth", config)
            .expect("basic-auth config")
    }

    fn credentials(username: &str, password: &str) -> HeaderValue {
        let mut raw = String::from(username);
        raw.push(':');
        raw.push_str(password);
        let encoded = base64::engine::general_purpose::STANDARD.encode(raw);
        HeaderValue::from_str(&format!("Basic {encoded}")).expect("header")
    }

    fn store_credential(fixture: &mut RequestFixture, password: &str) {
        let hash = bcrypt::hash(password, 4).expect("bcrypt hash");
        fixture.state.basicauth.insert(
            "alice".into(),
            AuthBasic {
                credential_id: "cred-1".into(),
                consumer_id: "consumer-1".into(),
                username: "alice".into(),
                password: hash,
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

    async fn challenge(response: axum::response::Response) -> String {
        let (status, headers, body) = response_parts(response).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let body: Value = serde_json::from_slice(&body).expect("json body");
        assert_eq!(body["message"], "Unauthorized");
        headers
            .get(header::WWW_AUTHENTICATE)
            .expect("challenge")
            .to_str()
            .expect("ascii")
            .to_string()
    }

    #[test]
    fn validates_config() {
        assert!(BasicAuthFactory.create("basic-auth", json!({})).is_ok());
        assert!(
            BasicAuthFactory
                .create("basic-auth", json!({"anonymous": "anon"}))
                .is_ok()
        );
        assert!(
            BasicAuthFactory
                .create("basic-auth", json!({"anonymous": 5}))
                .is_err()
        );
        assert!(
            BasicAuthFactory
                .create("basic-auth", json!({"realm": 5}))
                .is_err()
        );
        assert!(
            BasicAuthFactory
                .create(
                    "basic-auth",
                    json!({"hide_credentials": true, "realm": "internal"})
                )
                .is_ok()
        );
    }

    #[test]
    fn declares_request_and_log_phases() {
        assert_eq!(
            BasicAuthFactory.phases(),
            vec![PluginPhase::Access, PluginPhase::Log]
        );
    }

    #[tokio::test]
    async fn rejects_missing_malformed_and_unknown_credentials() {
        let plugin = plugin(json!({}));

        let mut fixture = RequestFixture::default();
        let action = plugin.access(&mut fixture.context()).await.unwrap();
        assert_eq!(
            challenge(short_circuit(action)).await,
            "Basic realm=\"service\""
        );

        let mut fixture = RequestFixture::default();
        fixture
            .headers
            .insert(header::AUTHORIZATION, HeaderValue::from_static("Basic !!!"));
        let action = plugin.access(&mut fixture.context()).await.unwrap();
        assert_eq!(
            challenge(short_circuit(action)).await,
            "Basic realm=\"service\""
        );

        let mut fixture = RequestFixture::default();
        store_credential(&mut fixture, "secret");
        fixture
            .headers
            .insert(header::AUTHORIZATION, credentials("unknown", "secret"));
        let action = plugin.access(&mut fixture.context()).await.unwrap();
        assert_eq!(
            challenge(short_circuit(action)).await,
            "Basic realm=\"service\""
        );

        let mut fixture = RequestFixture::default();
        store_credential(&mut fixture, "secret");
        fixture
            .headers
            .insert(header::AUTHORIZATION, credentials("alice", "wrong"));
        let action = plugin.access(&mut fixture.context()).await.unwrap();
        assert_eq!(
            challenge(short_circuit(action)).await,
            "Basic realm=\"service\""
        );
    }

    #[tokio::test]
    async fn accepts_valid_credentials_and_optionally_hides_them() {
        let visible = plugin(json!({"hide_credentials": false}));
        let mut fixture = RequestFixture::default();
        store_credential(&mut fixture, "secret");
        fixture
            .headers
            .insert(header::AUTHORIZATION, credentials("alice", "secret"));
        let action = visible.access(&mut fixture.context()).await.unwrap();
        assert!(matches!(action, PluginAction::Continue));
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
                .map(|credential| credential.kind.as_str()),
            Some("basic-auth")
        );
        assert!(fixture.headers.contains_key(header::AUTHORIZATION));

        let hidden = plugin(json!({"hide_credentials": true}));
        let mut fixture = RequestFixture::default();
        store_credential(&mut fixture, "secret");
        fixture
            .headers
            .insert(header::AUTHORIZATION, credentials("alice", "secret"));
        let action = hidden.access(&mut fixture.context()).await.unwrap();
        assert!(matches!(action, PluginAction::Continue));
        assert!(!fixture.headers.contains_key(header::AUTHORIZATION));
    }

    #[tokio::test]
    async fn uses_the_configured_realm() {
        let plugin = plugin(json!({"realm": "internal"}));
        let mut fixture = RequestFixture::default();
        let action = plugin.access(&mut fixture.context()).await.unwrap();
        assert_eq!(
            challenge(short_circuit(action)).await,
            "Basic realm=\"internal\""
        );
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
        assert_eq!(
            challenge(short_circuit(action)).await,
            "Basic realm=\"service\""
        );

        let empty = plugin(json!({"anonymous": " "}));
        let mut fixture = RequestFixture::default();
        let action = empty.access(&mut fixture.context()).await.unwrap();
        assert_eq!(
            challenge(short_circuit(action)).await,
            "Basic realm=\"service\""
        );
    }
}
