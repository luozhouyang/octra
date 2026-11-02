//! CORS preflight short-circuiting and response headers.

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::response::Response;
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use octra_plugin_api::{
    ApiGatewayPlugin, ApiGatewayPluginFactory, Capability, PluginAction, PluginError,
    PluginManifest, PluginResult, RequestContext, ResponseContext,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::manifest;

const HEADER_ORIGIN: &str = "origin";
const HEADER_REQUEST_METHOD: &str = "access-control-request-method";
const HEADER_REQUEST_HEADERS: &str = "access-control-request-headers";
const HEADER_ALLOW_ORIGIN: &str = "access-control-allow-origin";
const HEADER_ALLOW_METHODS: &str = "access-control-allow-methods";
const HEADER_ALLOW_HEADERS: &str = "access-control-allow-headers";
const HEADER_EXPOSE_HEADERS: &str = "access-control-expose-headers";
const HEADER_ALLOW_CREDENTIALS: &str = "access-control-allow-credentials";
const HEADER_MAX_AGE: &str = "access-control-max-age";
const HEADER_VARY: &str = "vary";

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
                    "allow_origins": { "type": "array", "items": { "type": "string" } },
                    "allow_methods": { "type": "array", "items": { "type": "string" } },
                    "allow_headers": { "type": "array", "items": { "type": "string" } },
                    "expose_headers": { "type": "array", "items": { "type": "string" } },
                    "allow_credentials": { "type": "boolean" },
                    "max_age": { "type": "integer", "minimum": 0 }
                }
            }),
        )
    }

    fn phases(&self) -> Vec<octra_plugin_api::PluginPhase> {
        vec![
            octra_plugin_api::PluginPhase::Request,
            octra_plugin_api::PluginPhase::Response,
            octra_plugin_api::PluginPhase::Log,
        ]
    }

    fn create(&self, _id: &str, config: Value) -> Result<Arc<dyn ApiGatewayPlugin>, PluginError> {
        let config: CorsConfig = serde_json::from_value(config)
            .map_err(|error| PluginError::fail_closed(format!("invalid cors config: {error}")))?;
        if config.allow_origins.is_empty() {
            return Err(PluginError::fail_closed(
                "cors.allow_origins must not be empty",
            ));
        }
        Ok(Arc::new(CorsPlugin {
            manifest: self.manifest(),
            config,
        }))
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct CorsConfig {
    allow_origins: Vec<String>,
    allow_methods: Vec<String>,
    allow_headers: Vec<String>,
    expose_headers: Vec<String>,
    allow_credentials: bool,
    max_age: Option<u64>,
}

impl Default for CorsConfig {
    fn default() -> Self {
        Self {
            allow_origins: vec!["*".to_string()],
            allow_methods: vec![
                "GET".to_string(),
                "POST".to_string(),
                "PUT".to_string(),
                "PATCH".to_string(),
                "DELETE".to_string(),
                "HEAD".to_string(),
                "OPTIONS".to_string(),
            ],
            allow_headers: vec!["*".to_string()],
            expose_headers: Vec::new(),
            allow_credentials: false,
            max_age: Some(600),
        }
    }
}

struct CorsPlugin {
    manifest: PluginManifest,
    config: CorsConfig,
}

#[async_trait]
impl ApiGatewayPlugin for CorsPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    async fn on_request(&self, cx: &mut RequestContext<'_>) -> PluginResult {
        if *cx.method != Method::OPTIONS {
            return Ok(PluginAction::Continue);
        }
        let Some(origin) = header_str(cx.headers, HEADER_ORIGIN) else {
            return Ok(PluginAction::Continue);
        };
        let Some(request_method) = header_str(cx.headers, HEADER_REQUEST_METHOD) else {
            return Ok(PluginAction::Continue);
        };
        if !self.origin_allowed(origin) {
            return Ok(PluginAction::ShortCircuit(
                Response::builder()
                    .status(StatusCode::FORBIDDEN)
                    .body(Body::empty())
                    .unwrap_or_else(|_| Response::new(Body::empty())),
            ));
        }

        let mut response = Response::new(Body::empty());
        *response.status_mut() = StatusCode::NO_CONTENT;
        self.write_common_headers(response.headers_mut(), origin);
        if !self.config.allow_methods.is_empty() {
            insert_joined(
                response.headers_mut(),
                HEADER_ALLOW_METHODS,
                &self.config.allow_methods,
            );
        } else {
            insert_str(response.headers_mut(), HEADER_ALLOW_METHODS, request_method);
        }
        if self.config.allow_headers.iter().any(|value| value == "*") {
            if let Some(requested) = header_str(cx.headers, HEADER_REQUEST_HEADERS) {
                insert_str(response.headers_mut(), HEADER_ALLOW_HEADERS, requested);
                append_vary(response.headers_mut(), HEADER_REQUEST_HEADERS);
            }
        } else {
            insert_joined(
                response.headers_mut(),
                HEADER_ALLOW_HEADERS,
                &self.config.allow_headers,
            );
        }
        if let Some(max_age) = self.config.max_age {
            insert_str(response.headers_mut(), HEADER_MAX_AGE, &max_age.to_string());
        }
        Ok(PluginAction::ShortCircuit(response))
    }

    async fn on_response(&self, cx: &mut ResponseContext<'_>) -> PluginResult {
        let Some(origin) = header_str(cx.request_headers, HEADER_ORIGIN) else {
            return Ok(PluginAction::Continue);
        };
        if !self.origin_allowed(origin) {
            return Ok(PluginAction::Continue);
        }
        self.write_common_headers(cx.headers, origin);
        if !self.config.expose_headers.is_empty() {
            insert_joined(
                cx.headers,
                HEADER_EXPOSE_HEADERS,
                &self.config.expose_headers,
            );
        }
        Ok(PluginAction::Continue)
    }
}

impl CorsPlugin {
    fn origin_allowed(&self, origin: &str) -> bool {
        self.config
            .allow_origins
            .iter()
            .any(|allowed| allowed == "*" || allowed.eq_ignore_ascii_case(origin))
    }

    fn write_common_headers(&self, headers: &mut HeaderMap, origin: &str) {
        // `*` is invalid together with credentials, so those responses echo the
        // origin. Explicit allowlists echo as well to keep `Vary: Origin`
        // meaningful for caches.
        let wildcard = self.config.allow_origins.iter().any(|value| value == "*");
        let allow_origin = if self.config.allow_credentials || !wildcard {
            origin
        } else {
            "*"
        };
        insert_str(headers, HEADER_ALLOW_ORIGIN, allow_origin);
        append_vary(headers, HEADER_ORIGIN);
        if self.config.allow_credentials {
            insert_str(headers, HEADER_ALLOW_CREDENTIALS, "true");
        }
    }
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

fn insert_str(headers: &mut HeaderMap, name: &'static str, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        headers.insert(HeaderName::from_static(name), value);
    }
}

fn insert_joined(headers: &mut HeaderMap, name: &'static str, values: &[String]) {
    if !values.is_empty() {
        insert_str(headers, name, &values.join(", "));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn plugin(config: Value) -> Arc<dyn ApiGatewayPlugin> {
        CorsFactory.create("test", config).unwrap()
    }

    #[test]
    fn wildcard_and_explicit_origins() {
        let wildcard = plugin(json!({})).manifest().name.clone();
        assert_eq!(wildcard, "cors");

        let explicit = CorsFactory
            .create("test", json!({"allow_origins": ["https://app.test"]}))
            .unwrap();
        assert_eq!(explicit.manifest().name, "cors");
    }

    #[test]
    fn rejects_empty_origins() {
        let Err(error) = CorsFactory.create("test", json!({"allow_origins": []})) else {
            panic!("expected an error");
        };
        assert!(error.message.contains("allow_origins"));
    }
}
