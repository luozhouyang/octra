//! JWT authentication.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use http::{HeaderValue, StatusCode, header};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode_header};
use octra_api_plugin_types::{
    ApiGatewayPlugin, ApiGatewayPluginFactory, Capability, PluginAction, PluginError,
    PluginManifest, PluginPhase, PluginResult, RequestContext,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{manifest, plugin_response, resolve_anonymous, set_consumer};

pub struct JwtFactory;

impl ApiGatewayPluginFactory for JwtFactory {
    fn manifest(&self) -> PluginManifest {
        manifest(
            "jwt",
            1005,
            vec![Capability::RequestHeaders, Capability::Auth],
            "Authenticates clients with a JWT",
            json!({
                "type": "object",
                "properties": {
                    "uri_param_names": { "type": "array", "items": { "type": "string" } },
                    "cookie_names": { "type": "array", "items": { "type": "string" } },
                    "header_names": { "type": "array", "items": { "type": "string" } },
                    "claims_to_verify": {
                        "type": "array",
                        "items": { "type": "string", "enum": ["exp", "nbf"] }
                    },
                    "key_claim_name": { "type": "string" },
                    "secret_is_base64": { "type": "boolean" },
                    "anonymous": { "type": ["string", "null"] },
                    "run_on_preflight": { "type": "boolean" },
                    "maximum_expiration": { "type": "number", "minimum": 0, "maximum": 31536000 },
                    "realm": { "type": ["string", "null"] }
                }
            }),
        )
    }

    fn create(&self, _id: &str, config: Value) -> Result<Arc<dyn ApiGatewayPlugin>, PluginError> {
        let config: Config = serde_json::from_value(config)
            .map_err(|error| PluginError::fail_closed(format!("invalid jwt config: {error}")))?;
        validate(&config)?;
        Ok(Arc::new(JwtPlugin {
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
    uri_param_names: Vec<String>,
    cookie_names: Vec<String>,
    header_names: Vec<String>,
    claims_to_verify: Vec<String>,
    key_claim_name: String,
    secret_is_base64: bool,
    anonymous: Option<String>,
    run_on_preflight: bool,
    maximum_expiration: u64,
    realm: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            uri_param_names: vec!["jwt".into()],
            cookie_names: Vec::new(),
            header_names: vec!["authorization".into()],
            claims_to_verify: vec!["exp".into()],
            key_claim_name: "iss".into(),
            secret_is_base64: false,
            anonymous: None,
            run_on_preflight: true,
            maximum_expiration: 0,
            realm: None,
        }
    }
}

fn validate(config: &Config) -> Result<(), PluginError> {
    for claim in &config.claims_to_verify {
        if !matches!(claim.as_str(), "exp" | "nbf") {
            return Err(PluginError::fail_closed(format!(
                "unsupported jwt claim '{claim}'"
            )));
        }
    }
    if config.maximum_expiration > 31_536_000 {
        return Err(PluginError::fail_closed(
            "jwt.maximum_expiration cannot exceed 31536000",
        ));
    }
    if config.maximum_expiration > 0 && !config.claims_to_verify.iter().any(|claim| claim == "exp")
    {
        return Err(PluginError::fail_closed(
            "jwt.maximum_expiration requires claims_to_verify to include exp",
        ));
    }
    Ok(())
}

struct JwtPlugin {
    manifest: PluginManifest,
    config: Config,
}

#[async_trait]
impl ApiGatewayPlugin for JwtPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    async fn access(&self, cx: &mut RequestContext<'_>) -> PluginResult {
        if cx.method == http::Method::OPTIONS && !self.config.run_on_preflight {
            return Ok(PluginAction::Continue);
        }

        let token = self.find_token(cx);
        if let Some(token) = token
            && let Some(credentials) = self.verify(cx.state, &token)
        {
            set_consumer(
                cx.state,
                &credentials.consumer_id,
                &credentials.credential_id,
                "jwt",
                &credentials.key,
            );
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

        let mut response = plugin_response(StatusCode::UNAUTHORIZED, "Unauthorized");
        let challenge = self
            .config
            .realm
            .as_deref()
            .map(|realm| format!("Bearer realm=\"{realm}\""))
            .unwrap_or_else(|| "Bearer".into());
        if let Ok(value) = HeaderValue::from_str(&challenge) {
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, value);
        }
        Ok(PluginAction::ShortCircuit(response))
    }
}

fn decode_claims(token: &str) -> Option<Value> {
    use base64::Engine as _;
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.as_bytes())
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

struct JwtCredentials {
    consumer_id: String,
    credential_id: String,
    key: String,
}

impl JwtPlugin {
    fn find_token(&self, cx: &RequestContext<'_>) -> Option<String> {
        for name in &self.config.header_names {
            if let Some(value) = cx
                .headers
                .get(name.as_str())
                .and_then(|value| value.to_str().ok())
            {
                if let Some(token) = value.strip_prefix("Bearer ") {
                    return Some(token.trim().to_string());
                }
                if !value.contains(' ') {
                    return Some(value.trim().to_string());
                }
            }
        }
        if let Some(query) = cx.uri.query() {
            for (name, value) in url::form_urlencoded::parse(query.as_bytes()) {
                if self
                    .config
                    .uri_param_names
                    .iter()
                    .any(|key| key == name.as_ref())
                {
                    return Some(value.into_owned());
                }
            }
        }
        for name in &self.config.cookie_names {
            if let Some(cookie) = cx
                .headers
                .get(header::COOKIE)
                .and_then(|value| value.to_str().ok())
            {
                for pair in cookie.split(';') {
                    if let Some((key, value)) = pair.trim().split_once('=')
                        && key == name
                    {
                        return Some(value.to_string());
                    }
                }
            }
        }
        None
    }

    fn verify(
        &self,
        state: &octra_api_plugin_types::GatewayState,
        token: &str,
    ) -> Option<JwtCredentials> {
        let header = decode_header(token).ok()?;
        let claims = decode_claims(token)?;
        let key = claims
            .get(&self.config.key_claim_name)
            .and_then(Value::as_str)?;
        let entry = state.jwt.get(key)?.clone();
        let algorithm = entry.algorithm.parse::<Algorithm>().ok()?;
        if header.alg != algorithm {
            return None;
        }

        let secret = if self.config.secret_is_base64 {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD
                .decode(entry.secret.as_bytes())
                .ok()?
        } else {
            entry.secret.as_bytes().to_vec()
        };

        let decoding_key = match algorithm {
            Algorithm::RS256 | Algorithm::RS384 | Algorithm::RS512 => {
                let pem = entry.rsa_public_key.as_deref()?;
                DecodingKey::from_rsa_pem(pem.as_bytes()).ok()?
            }
            _ => DecodingKey::from_secret(&secret),
        };

        let mut validation = Validation::new(algorithm);
        validation.required_spec_claims.clear();
        let verify_exp = self
            .config
            .claims_to_verify
            .iter()
            .any(|claim| claim == "exp");
        validation.validate_exp = verify_exp;
        if verify_exp {
            validation.required_spec_claims.insert("exp".to_string());
        }
        validation.validate_nbf = self
            .config
            .claims_to_verify
            .iter()
            .any(|claim| claim == "nbf");
        jsonwebtoken::decode::<Value>(token, &decoding_key, &validation).ok()?;

        if self.config.maximum_expiration > 0 {
            let exp = claims.get("exp").and_then(Value::as_i64)?;
            let remaining = exp - Utc::now().timestamp();
            if remaining > self.config.maximum_expiration as i64 {
                return None;
            }
        }

        Some(JwtCredentials {
            consumer_id: entry.consumer_id,
            credential_id: entry.credential_id,
            key: entry.key,
        })
    }
}

impl octra_api_plugin_types::ApiGatewayBackgroundPlugin for JwtPlugin {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{RequestFixture, response_parts, short_circuit};
    use base64::Engine as _;
    use http::Method;
    use jsonwebtoken::{EncodingKey, Header, encode};
    use octra_api_plugin_types::{AuthJwt, ConsumerInfo};

    fn plugin(config: Value) -> Arc<dyn ApiGatewayPlugin> {
        JwtFactory.create("jwt", config).expect("jwt config")
    }

    fn token(claims: Value, secret: &str, algorithm: Algorithm) -> String {
        encode(
            &Header::new(algorithm),
            &claims,
            &EncodingKey::from_secret(secret.as_bytes()),
        )
        .expect("jwt")
    }

    fn store_jwt(fixture: &mut RequestFixture, key: &str, secret: &str, algorithm: &str) {
        fixture.state.jwt.insert(
            key.into(),
            AuthJwt {
                credential_id: "cred-1".into(),
                consumer_id: "consumer-1".into(),
                key: key.into(),
                secret: secret.into(),
                algorithm: algorithm.into(),
                rsa_public_key: None,
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

    fn bearer(token: &str) -> HeaderValue {
        HeaderValue::from_str(&format!("Bearer {token}")).expect("header")
    }

    async fn assert_unauthorized(action: PluginAction, challenge: &str) {
        let (status, headers, body) = response_parts(short_circuit(action)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let body: Value = serde_json::from_slice(&body).expect("json body");
        assert_eq!(body["message"], "Unauthorized");
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
        assert!(JwtFactory.create("jwt", json!({})).is_ok());
        assert!(
            JwtFactory
                .create("jwt", json!({"claims_to_verify": ["exp", "nbf"]}))
                .is_ok()
        );
        assert!(
            JwtFactory
                .create("jwt", json!({"claims_to_verify": ["iat"]}))
                .is_err()
        );
        assert!(
            JwtFactory
                .create("jwt", json!({"maximum_expiration": 31_536_001}))
                .is_err()
        );
        assert!(
            JwtFactory
                .create(
                    "jwt",
                    json!({"claims_to_verify": ["nbf"], "maximum_expiration": 60})
                )
                .is_err()
        );
        assert!(
            JwtFactory
                .create(
                    "jwt",
                    json!({"claims_to_verify": ["exp"], "maximum_expiration": 60})
                )
                .is_ok()
        );
        assert!(
            JwtFactory
                .create("jwt", json!({"maximum_expiration": -1}))
                .is_err()
        );
        assert!(
            JwtFactory
                .create("jwt", json!({"claims_to_verify": "exp"}))
                .is_err()
        );
    }

    #[test]
    fn declares_request_and_log_phases() {
        assert_eq!(
            JwtFactory.phases(),
            vec![PluginPhase::Access, PluginPhase::Log]
        );
    }

    #[tokio::test]
    async fn authenticates_hs256_tokens() {
        let plugin = plugin(json!({}));
        let now = Utc::now().timestamp();
        let token = token(
            json!({"iss": "key-1", "exp": now + 3600}),
            "secret",
            Algorithm::HS256,
        );

        let mut fixture = RequestFixture::default();
        store_jwt(&mut fixture, "key-1", "secret", "HS256");
        fixture
            .headers
            .insert(header::AUTHORIZATION, bearer(&token));

        let action = plugin.access(&mut fixture.context()).await.unwrap();
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
                .map(|credential| (credential.kind.as_str(), credential.identifier.as_str())),
            Some(("jwt", "key-1"))
        );
    }

    #[tokio::test]
    async fn rejects_invalid_expired_and_unknown_tokens() {
        let plugin = plugin(json!({}));
        let now = Utc::now().timestamp();

        let mut fixture = RequestFixture::default();
        store_jwt(&mut fixture, "key-1", "secret", "HS256");
        let bad = token(
            json!({"iss": "key-1", "exp": now + 3600}),
            "other-secret",
            Algorithm::HS256,
        );
        fixture.headers.insert(header::AUTHORIZATION, bearer(&bad));
        let action = plugin.access(&mut fixture.context()).await.unwrap();
        assert_unauthorized(action, "Bearer").await;

        let mut fixture = RequestFixture::default();
        store_jwt(&mut fixture, "key-1", "secret", "HS256");
        let expired = token(
            json!({"iss": "key-1", "exp": now - 120}),
            "secret",
            Algorithm::HS256,
        );
        fixture
            .headers
            .insert(header::AUTHORIZATION, bearer(&expired));
        let action = plugin.access(&mut fixture.context()).await.unwrap();
        assert_unauthorized(action, "Bearer").await;

        let mut fixture = RequestFixture::default();
        store_jwt(&mut fixture, "key-1", "secret", "HS256");
        let no_exp = token(json!({"iss": "key-1"}), "secret", Algorithm::HS256);
        fixture
            .headers
            .insert(header::AUTHORIZATION, bearer(&no_exp));
        let action = plugin.access(&mut fixture.context()).await.unwrap();
        assert_unauthorized(action, "Bearer").await;

        let mut fixture = RequestFixture::default();
        store_jwt(&mut fixture, "unknown", "secret", "HS256");
        let unknown = token(
            json!({"iss": "key-1", "exp": now + 3600}),
            "secret",
            Algorithm::HS256,
        );
        fixture
            .headers
            .insert(header::AUTHORIZATION, bearer(&unknown));
        let action = plugin.access(&mut fixture.context()).await.unwrap();
        assert_unauthorized(action, "Bearer").await;

        let mut fixture = RequestFixture::default();
        store_jwt(&mut fixture, "key-1", "secret", "HS512");
        let mismatch = token(
            json!({"iss": "key-1", "exp": now + 3600}),
            "secret",
            Algorithm::HS256,
        );
        fixture
            .headers
            .insert(header::AUTHORIZATION, bearer(&mismatch));
        let action = plugin.access(&mut fixture.context()).await.unwrap();
        assert_unauthorized(action, "Bearer").await;

        let mut fixture = RequestFixture::default();
        fixture
            .headers
            .insert(header::AUTHORIZATION, bearer("not-a-token"));
        let action = plugin.access(&mut fixture.context()).await.unwrap();
        assert_unauthorized(action, "Bearer").await;
    }

    #[tokio::test]
    async fn verifies_nbf_and_maximum_expiration_when_configured() {
        let required_nbf = plugin(json!({"claims_to_verify": ["exp", "nbf"]}));
        let now = Utc::now().timestamp();
        let future = token(
            json!({"iss": "key-1", "exp": now + 3600, "nbf": now + 120}),
            "secret",
            Algorithm::HS256,
        );
        let mut fixture = RequestFixture::default();
        store_jwt(&mut fixture, "key-1", "secret", "HS256");
        fixture
            .headers
            .insert(header::AUTHORIZATION, bearer(&future));
        let action = required_nbf.access(&mut fixture.context()).await.unwrap();
        assert_unauthorized(action, "Bearer").await;

        let past = token(
            json!({"iss": "key-1", "exp": now + 3600, "nbf": now - 120}),
            "secret",
            Algorithm::HS256,
        );
        let mut fixture = RequestFixture::default();
        store_jwt(&mut fixture, "key-1", "secret", "HS256");
        fixture.headers.insert(header::AUTHORIZATION, bearer(&past));
        let action = required_nbf.access(&mut fixture.context()).await.unwrap();
        assert!(matches!(action, PluginAction::Continue));

        let short = plugin(json!({"claims_to_verify": ["exp"], "maximum_expiration": 60}));
        let long_lived = token(
            json!({"iss": "key-1", "exp": now + 3600}),
            "secret",
            Algorithm::HS256,
        );
        let mut fixture = RequestFixture::default();
        store_jwt(&mut fixture, "key-1", "secret", "HS256");
        fixture
            .headers
            .insert(header::AUTHORIZATION, bearer(&long_lived));
        let action = short.access(&mut fixture.context()).await.unwrap();
        assert_unauthorized(action, "Bearer").await;

        let short_lived = token(
            json!({"iss": "key-1", "exp": now + 30}),
            "secret",
            Algorithm::HS256,
        );
        let mut fixture = RequestFixture::default();
        store_jwt(&mut fixture, "key-1", "secret", "HS256");
        fixture
            .headers
            .insert(header::AUTHORIZATION, bearer(&short_lived));
        let action = short.access(&mut fixture.context()).await.unwrap();
        assert!(matches!(action, PluginAction::Continue));
    }

    #[tokio::test]
    async fn reads_tokens_from_configured_sources() {
        let now = Utc::now().timestamp();
        let query_plugin = plugin(json!({}));
        let query_token = token(
            json!({"iss": "key-1", "exp": now + 3600}),
            "secret",
            Algorithm::HS256,
        );
        let mut fixture = RequestFixture::default();
        store_jwt(&mut fixture, "key-1", "secret", "HS256");
        fixture.uri = format!("/p?jwt={query_token}").parse().expect("uri");
        let action = query_plugin.access(&mut fixture.context()).await.unwrap();
        assert!(matches!(action, PluginAction::Continue));

        let cookie_token = token(
            json!({"iss": "key-1", "exp": now + 3600}),
            "secret",
            Algorithm::HS256,
        );
        let cookies = plugin(json!({"cookie_names": ["session"]}));
        let mut fixture = RequestFixture::default();
        store_jwt(&mut fixture, "key-1", "secret", "HS256");
        fixture.headers.insert(
            header::COOKIE,
            HeaderValue::from_str(&format!("a=1; session={cookie_token}")).expect("cookie"),
        );
        let action = cookies.access(&mut fixture.context()).await.unwrap();
        assert!(matches!(action, PluginAction::Continue));

        let header_token = token(
            json!({"iss": "key-1", "exp": now + 3600}),
            "secret",
            Algorithm::HS256,
        );
        let custom = plugin(json!({"header_names": ["x-jwt"]}));
        let mut fixture = RequestFixture::default();
        store_jwt(&mut fixture, "key-1", "secret", "HS256");
        fixture.headers.insert(
            header::HeaderName::from_static("x-jwt"),
            HeaderValue::from_str(&header_token).expect("token"),
        );
        let action = custom.access(&mut fixture.context()).await.unwrap();
        assert!(matches!(action, PluginAction::Continue));
    }

    #[tokio::test]
    async fn supports_base64_secrets_and_custom_key_claims() {
        let now = Utc::now().timestamp();
        let raw_secret = b"super-secret";
        let encoded = base64::engine::general_purpose::STANDARD.encode(raw_secret);
        let base64_plugin = plugin(json!({"secret_is_base64": true}));
        let signed = token(
            json!({"iss": "key-1", "exp": now + 3600}),
            "super-secret",
            Algorithm::HS256,
        );
        let mut fixture = RequestFixture::default();
        store_jwt(&mut fixture, "key-1", &encoded, "HS256");
        fixture
            .headers
            .insert(header::AUTHORIZATION, bearer(&signed));
        let action = base64_plugin.access(&mut fixture.context()).await.unwrap();
        assert!(matches!(action, PluginAction::Continue));

        let invalid = plugin(json!({"secret_is_base64": true}));
        let mut fixture = RequestFixture::default();
        store_jwt(&mut fixture, "key-1", "not base64!", "HS256");
        fixture
            .headers
            .insert(header::AUTHORIZATION, bearer(&signed));
        let action = invalid.access(&mut fixture.context()).await.unwrap();
        assert_unauthorized(action, "Bearer").await;

        let custom = plugin(json!({"key_claim_name": "sub"}));
        let custom_token = token(
            json!({"sub": "key-1", "exp": now + 3600}),
            "secret",
            Algorithm::HS256,
        );
        let mut fixture = RequestFixture::default();
        store_jwt(&mut fixture, "key-1", "secret", "HS256");
        fixture
            .headers
            .insert(header::AUTHORIZATION, bearer(&custom_token));
        let action = custom.access(&mut fixture.context()).await.unwrap();
        assert!(matches!(action, PluginAction::Continue));
    }

    #[tokio::test]
    async fn rsa_entries_without_a_public_key_are_rejected() {
        let now = Utc::now().timestamp();
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(json!({"alg": "RS256", "typ": "JWT"}).to_string());
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(json!({"iss": "key-1", "exp": now + 3600}).to_string());
        let token = format!("{header}.{payload}.signature");

        let plugin = plugin(json!({}));
        let mut fixture = RequestFixture::default();
        store_jwt(&mut fixture, "key-1", "", "RS256");
        fixture
            .headers
            .insert(header::AUTHORIZATION, bearer(&token));
        let action = plugin.access(&mut fixture.context()).await.unwrap();
        assert_unauthorized(action, "Bearer").await;
    }

    #[tokio::test]
    async fn skips_preflight_and_supports_anonymous_consumers() {
        let required = plugin(json!({}));
        let mut fixture = RequestFixture {
            method: Method::OPTIONS,
            ..RequestFixture::default()
        };
        let action = required.access(&mut fixture.context()).await.unwrap();
        assert_unauthorized(action, "Bearer").await;

        let skipped = plugin(json!({"run_on_preflight": false}));
        let mut fixture = RequestFixture {
            method: Method::OPTIONS,
            ..RequestFixture::default()
        };
        let action = skipped.access(&mut fixture.context()).await.unwrap();
        assert!(matches!(action, PluginAction::Continue));
        assert!(fixture.state.consumer.is_none());

        let anonymous = plugin(json!({"anonymous": "anon"}));
        let mut fixture = RequestFixture::default();
        fixture.state.consumers.insert(
            "anon".into(),
            ConsumerInfo {
                id: "anon".into(),
                username: Some("anonymous".into()),
                ..ConsumerInfo::default()
            },
        );
        let action = anonymous.access(&mut fixture.context()).await.unwrap();
        assert!(matches!(action, PluginAction::Continue));
        assert!(fixture.state.anonymous);

        let unknown = plugin(json!({"anonymous": "ghost"}));
        let mut fixture = RequestFixture::default();
        let action = unknown.access(&mut fixture.context()).await.unwrap();
        assert_unauthorized(action, "Bearer").await;
    }

    #[tokio::test]
    async fn uses_the_configured_realm() {
        let plugin = plugin(json!({"realm": "internal"}));
        let mut fixture = RequestFixture::default();
        let action = plugin.access(&mut fixture.context()).await.unwrap();
        assert_unauthorized(action, "Bearer realm=\"internal\"").await;
    }
}
