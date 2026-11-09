//! Kong-compatible CORS preflight handling and response headers.

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::response::Response;
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use octra_api_plugin_types::{
    ApiGatewayPlugin, ApiGatewayPluginFactory, Capability, PluginAction, PluginError,
    PluginManifest, PluginPhase, PluginResult, RequestContext, ResponseContext,
};
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};
use url::Url;

use crate::manifest;

const HEADER_ORIGIN: &str = "origin";
const HEADER_REQUEST_METHOD: &str = "access-control-request-method";
const HEADER_REQUEST_HEADERS: &str = "access-control-request-headers";
const HEADER_REQUEST_PRIVATE_NETWORK: &str = "access-control-request-private-network";
const HEADER_ALLOW_ORIGIN: &str = "access-control-allow-origin";
const HEADER_ALLOW_METHODS: &str = "access-control-allow-methods";
const HEADER_ALLOW_HEADERS: &str = "access-control-allow-headers";
const HEADER_EXPOSE_HEADERS: &str = "access-control-expose-headers";
const HEADER_ALLOW_CREDENTIALS: &str = "access-control-allow-credentials";
const HEADER_ALLOW_PRIVATE_NETWORK: &str = "access-control-allow-private-network";
const HEADER_MAX_AGE: &str = "access-control-max-age";
const HEADER_VARY: &str = "vary";
const STATE_SKIP: &str = "cors-skip-response-headers";

const DEFAULT_METHODS: [&str; 9] = [
    "GET", "HEAD", "PUT", "PATCH", "POST", "DELETE", "OPTIONS", "TRACE", "CONNECT",
];

pub struct CorsFactory;

impl ApiGatewayPluginFactory for CorsFactory {
    fn manifest(&self) -> PluginManifest {
        manifest(
            "cors",
            1300,
            vec![Capability::RequestHeaders, Capability::ResponseHeaders],
            "Handles CORS preflight requests and response headers",
            json!({
                "type": "object",
                "properties": {
                    "origins": { "type": "array", "items": { "type": "string" } },
                    "headers": { "type": "array", "items": { "type": "string" } },
                    "exposed_headers": { "type": "array", "items": { "type": "string" } },
                    "methods": {
                        "type": "array",
                        "items": { "type": "string", "enum": DEFAULT_METHODS }
                    },
                    "max_age": { "type": "number", "minimum": 0 },
                    "credentials": { "type": "boolean", "default": false },
                    "private_network": { "type": "boolean", "default": false },
                    "preflight_continue": { "type": "boolean", "default": false },
                    "allow_origin_absent": { "type": "boolean", "default": true }
                }
            }),
        )
    }

    fn create(&self, _id: &str, config: Value) -> Result<Arc<dyn ApiGatewayPlugin>, PluginError> {
        let mut config: Config = serde_json::from_value(config)
            .map_err(|error| PluginError::fail_closed(format!("invalid cors config: {error}")))?;
        let rules = compile_origins(&config.origins)?;
        config.methods = config
            .methods
            .iter()
            .map(|method| method.to_uppercase())
            .collect();
        for method in &config.methods {
            if !DEFAULT_METHODS.contains(&method.as_str()) {
                return Err(PluginError::fail_closed(format!(
                    "unsupported CORS method '{method}'"
                )));
            }
        }
        if config
            .max_age
            .is_some_and(|age| !age.is_finite() || age < 0.0)
        {
            return Err(PluginError::fail_closed(
                "cors.max_age must be non-negative",
            ));
        }
        Ok(Arc::new(CorsPlugin {
            manifest: self.manifest(),
            config,
            origins: rules,
        }))
    }

    fn phases(&self) -> Vec<PluginPhase> {
        vec![PluginPhase::Access, PluginPhase::Response, PluginPhase::Log]
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct Config {
    origins: Vec<String>,
    headers: Vec<String>,
    exposed_headers: Vec<String>,
    methods: Vec<String>,
    max_age: Option<f64>,
    credentials: bool,
    private_network: bool,
    preflight_continue: bool,
    allow_origin_absent: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            origins: Vec::new(),
            headers: Vec::new(),
            exposed_headers: Vec::new(),
            methods: DEFAULT_METHODS.iter().map(|m| m.to_string()).collect(),
            max_age: None,
            credentials: false,
            private_network: false,
            preflight_continue: false,
            allow_origin_absent: true,
        }
    }
}

enum OriginRule {
    Wildcard,
    Plain { normalized: String },
    Regex { regex: Regex, host_only: bool },
}

enum OriginMatch {
    Wildcard,
    Matched(String),
    Denied,
}

struct CorsPlugin {
    manifest: PluginManifest,
    config: Config,
    origins: Vec<OriginRule>,
}

fn compile_origins(origins: &[String]) -> Result<Vec<OriginRule>, PluginError> {
    let mut rules = Vec::with_capacity(origins.len());
    for origin in origins {
        if origin == "*" {
            rules.push(OriginRule::Wildcard);
            continue;
        }
        if is_regex(origin) {
            let pattern = format!("{}$", origin.trim_start_matches('^'));
            let regex = Regex::new(&pattern).map_err(|error| {
                PluginError::fail_closed(format!("invalid CORS origin regex '{origin}': {error}"))
            })?;
            rules.push(OriginRule::Regex {
                regex,
                host_only: !origin.contains(':'),
            });
        } else {
            rules.push(OriginRule::Plain {
                normalized: normalize_origin(origin),
            });
        }
    }
    Ok(rules)
}

fn is_regex(origin: &str) -> bool {
    origin
        .chars()
        .any(|value| !(value.is_ascii_alphanumeric() || matches!(value, '.' | ':' | '/' | '-')))
}

impl CorsPlugin {
    fn match_origin(&self, origin: Option<&str>) -> OriginMatch {
        if self.origins.is_empty()
            || self
                .origins
                .iter()
                .any(|rule| matches!(rule, OriginRule::Wildcard))
        {
            return OriginMatch::Wildcard;
        }
        let Some(origin) = origin else {
            return OriginMatch::Denied;
        };
        let normalized = normalize_origin(origin);
        let host = origin_host(origin);
        for rule in &self.origins {
            match rule {
                OriginRule::Wildcard => return OriginMatch::Wildcard,
                OriginRule::Plain {
                    normalized: expected,
                } => {
                    if expected == &normalized {
                        return OriginMatch::Matched(normalized);
                    }
                }
                OriginRule::Regex { regex, host_only } => {
                    let subject = if *host_only { &host } else { &normalized };
                    if regex.is_match(subject) {
                        return OriginMatch::Matched(normalized);
                    }
                }
            }
        }
        OriginMatch::Denied
    }

    fn apply_origin(&self, headers: &mut HeaderMap, matched: &OriginMatch, origin: Option<&str>) {
        match matched {
            OriginMatch::Wildcard => {
                if self.config.credentials
                    && let Some(origin) = origin
                {
                    insert_str(headers, HEADER_ALLOW_ORIGIN, origin);
                    append_vary(headers, "Origin");
                    insert_str(headers, HEADER_ALLOW_CREDENTIALS, "true");
                } else {
                    insert_str(headers, HEADER_ALLOW_ORIGIN, "*");
                }
            }
            OriginMatch::Matched(value) => {
                insert_str(headers, HEADER_ALLOW_ORIGIN, value);
                append_vary(headers, "Origin");
                if self.config.credentials {
                    insert_str(headers, HEADER_ALLOW_CREDENTIALS, "true");
                }
            }
            OriginMatch::Denied => {
                headers.remove(HEADER_ALLOW_ORIGIN);
            }
        }
    }
}

#[async_trait]
impl ApiGatewayPlugin for CorsPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    async fn access(&self, cx: &mut RequestContext<'_>) -> PluginResult {
        if *cx.method != Method::OPTIONS {
            return Ok(PluginAction::Continue);
        }
        let Some(origin) = header_str(cx.headers, HEADER_ORIGIN) else {
            return Ok(PluginAction::Continue);
        };
        if origin.is_empty() {
            return Ok(PluginAction::Continue);
        }
        let Some(request_method) = header_str(cx.headers, HEADER_REQUEST_METHOD) else {
            return Ok(PluginAction::Continue);
        };

        cx.state.values.insert(STATE_SKIP.into(), Value::Bool(true));
        if self.config.preflight_continue {
            return Ok(PluginAction::Continue);
        }

        let matched = self.match_origin(Some(origin));
        let mut response = Response::new(Body::empty());
        *response.status_mut() = StatusCode::OK;
        self.apply_origin(response.headers_mut(), &matched, Some(origin));

        if !self.config.methods.is_empty() {
            insert_str(
                response.headers_mut(),
                HEADER_ALLOW_METHODS,
                &self.config.methods.join(","),
            );
        } else {
            insert_str(response.headers_mut(), HEADER_ALLOW_METHODS, request_method);
        }

        if !self.config.headers.is_empty() {
            insert_str(
                response.headers_mut(),
                HEADER_ALLOW_HEADERS,
                &self.config.headers.join(","),
            );
        } else if let Some(requested) = header_str(cx.headers, HEADER_REQUEST_HEADERS) {
            insert_str(response.headers_mut(), HEADER_ALLOW_HEADERS, requested);
            append_vary(response.headers_mut(), "Access-Control-Request-Headers");
        }

        if let Some(max_age) = self.config.max_age {
            insert_str(response.headers_mut(), HEADER_MAX_AGE, &number(max_age));
        }

        if self.config.private_network
            && header_str(cx.headers, HEADER_REQUEST_PRIVATE_NETWORK) == Some("true")
        {
            insert_str(response.headers_mut(), HEADER_ALLOW_PRIVATE_NETWORK, "true");
            append_vary(
                response.headers_mut(),
                "Access-Control-Request-Private-Network",
            );
        }

        Ok(PluginAction::ShortCircuit(response))
    }

    async fn response(&self, cx: &mut ResponseContext<'_>) -> PluginResult {
        if cx
            .state
            .values
            .get(STATE_SKIP)
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return Ok(PluginAction::Continue);
        }

        let origin = header_str(cx.request_headers, HEADER_ORIGIN);
        if origin.is_none() && !self.config.allow_origin_absent {
            return Ok(PluginAction::Continue);
        }
        let matched = self.match_origin(origin);
        self.apply_origin(cx.headers, &matched, origin);

        if !self.config.exposed_headers.is_empty() {
            insert_str(
                cx.headers,
                HEADER_EXPOSE_HEADERS,
                &self.config.exposed_headers.join(","),
            );
        }
        Ok(PluginAction::Continue)
    }
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
}

fn insert_str(headers: &mut HeaderMap, name: &'static str, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        headers.insert(HeaderName::from_static(name), value);
    }
}

fn append_vary(headers: &mut HeaderMap, token: &str) {
    let name = HeaderName::from_static(HEADER_VARY);
    let existing = headers
        .get(&name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let already = existing
        .split(',')
        .any(|part| part.trim().eq_ignore_ascii_case(token));
    if already {
        return;
    }
    let value = if existing.is_empty() {
        token.to_string()
    } else {
        format!("{existing}, {token}")
    };
    if let Ok(value) = HeaderValue::from_str(&value) {
        headers.insert(name, value);
    }
}

fn normalize_origin(origin: &str) -> String {
    match Url::parse(origin) {
        Ok(url) => {
            let host = url.host_str().unwrap_or_default();
            match url.port() {
                Some(port) => format!("{}://{host}:{port}", url.scheme()),
                None => format!("{}://{host}", url.scheme()),
            }
        }
        Err(_) => origin.trim_end_matches('/').to_ascii_lowercase(),
    }
}

fn origin_host(origin: &str) -> String {
    Url::parse(origin)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
        .unwrap_or_default()
}

fn number(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

impl octra_api_plugin_types::ApiGatewayBackgroundPlugin for CorsPlugin {}

#[cfg(test)]
mod tests {
    use super::*;

    fn plugin(config: Value) -> Result<Arc<dyn ApiGatewayPlugin>, PluginError> {
        CorsFactory.create("test", config)
    }

    #[test]
    fn accepts_kong_style_origins() {
        assert!(plugin(json!({})).is_ok());
        assert!(plugin(json!({"origins": ["*"]})).is_ok());
        assert!(plugin(json!({"origins": ["https://app.test"]})).is_ok());
        assert!(plugin(json!({"origins": ["https://.*\\.example\\.com"]})).is_ok());
    }

    #[test]
    fn rejects_bad_methods_and_regex() {
        assert!(plugin(json!({"methods": ["BREW"]})).is_err());
        assert!(plugin(json!({"origins": ["["]})).is_err());
    }
}
