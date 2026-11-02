//! JWT authentication.

use std::sync::Arc;

use async_trait::async_trait;
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
            vec![Capability::RequestHeaders],
            "Authenticates clients with a JWT",
            json!({
                "type": "object",
                "properties": {
                    "uri_param_names": { "type": "array", "items": { "type": "string" } },
                    "cookie_names": { "type": "array", "items": { "type": "string" } },
                    "header_names": { "type": "array", "items": { "type": "string" } },
                    "claims_to_verify": { "type": "array", "items": { "type": "string" } },
                    "key_claim_name": { "type": "string" },
                    "secret_is_base64": { "type": "boolean" },
                    "anonymous": { "type": ["string", "null"] },
                    "run_on_preflight": { "type": "boolean" }
                }
            }),
        )
    }

    fn create(&self, _id: &str, config: Value) -> Result<Arc<dyn ApiGatewayPlugin>, PluginError> {
        let config: Config = serde_json::from_value(config)
            .map_err(|error| PluginError::fail_closed(format!("invalid jwt config: {error}")))?;
        Ok(Arc::new(JwtPlugin {
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
    uri_param_names: Vec<String>,
    cookie_names: Vec<String>,
    header_names: Vec<String>,
    claims_to_verify: Vec<String>,
    key_claim_name: String,
    secret_is_base64: bool,
    anonymous: Option<String>,
    run_on_preflight: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            uri_param_names: vec!["jwt".into()],
            cookie_names: Vec::new(),
            header_names: vec!["authorization".into()],
            claims_to_verify: vec!["exp".into(), "nbf".into()],
            key_claim_name: "iss".into(),
            secret_is_base64: false,
            anonymous: None,
            run_on_preflight: true,
        }
    }
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

    async fn on_request(&self, cx: &mut RequestContext<'_>) -> PluginResult {
        if cx.method == http::Method::OPTIONS && !self.config.run_on_preflight {
            return Ok(PluginAction::Continue);
        }

        let token = self.find_token(cx);
        if let Some(token) = token {
            if let Some(credentials) = self.verify(cx.state, &token) {
                set_consumer(
                    cx.state,
                    &credentials.consumer_id,
                    &credentials.credential_id,
                    "jwt",
                    &credentials.key,
                );
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
        response.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            HeaderValue::from_static("Bearer realm=\"octra\""),
        );
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
        validation.validate_exp = self
            .config
            .claims_to_verify
            .iter()
            .any(|claim| claim == "exp");
        validation.validate_nbf = self
            .config
            .claims_to_verify
            .iter()
            .any(|claim| claim == "nbf");
        jsonwebtoken::decode::<Value>(token, &decoding_key, &validation).ok()?;

        Some(JwtCredentials {
            consumer_id: entry.consumer_id,
            credential_id: entry.credential_id,
            key: entry.key,
        })
    }
}
