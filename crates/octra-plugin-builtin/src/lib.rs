//! Built-in API Gateway plugins.
//!
//! These plugins validate their JSON configuration at creation time and keep
//! the hot path allocation-light. They are trusted, in-process Rust code;
//! third-party extensions will use the WASM runtime instead.

mod acl;
mod basic_auth;
mod circuit_breaker;
mod cors;
mod header_rewrite;
mod ip_restriction;
mod jwt;
mod key_auth;
mod rate_limit;
mod request_id;
mod request_size_limiting;
mod request_termination;

use axum::body::Body;
use axum::response::Response;
use http::{HeaderValue, StatusCode, header};
use octra_plugin_api::{
    Capability, ConsumerInfo, CredentialInfo, GatewayState, PluginKind, PluginManifest,
    PluginRuntimeKind,
};
use octra_plugin_runtime::PluginRegistry;
use serde_json::{Value, json};

pub use circuit_breaker::CircuitBreakerFactory;
pub use cors::CorsFactory;
pub use header_rewrite::HeaderRewriteFactory;
pub use rate_limit::RateLimitFactory;
pub use request_id::RequestIdFactory;

pub use acl::AclFactory;
pub use basic_auth::BasicAuthFactory;
pub use ip_restriction::IpRestrictionFactory;
pub use jwt::JwtFactory;
pub use key_auth::KeyAuthFactory;
pub use request_size_limiting::RequestSizeLimitingFactory;
pub use request_termination::RequestTerminationFactory;

pub fn register(registry: &mut PluginRegistry) {
    registry.register(RequestIdFactory);
    registry.register(CorsFactory);
    registry.register(RateLimitFactory);
    registry.register(CircuitBreakerFactory);
    registry.register(HeaderRewriteFactory);
    registry.register(KeyAuthFactory);
    registry.register(BasicAuthFactory);
    registry.register(JwtFactory);
    registry.register(AclFactory);
    registry.register(IpRestrictionFactory);
    registry.register(RequestSizeLimitingFactory);
    registry.register(RequestTerminationFactory);
}

pub(crate) fn plugin_response(status: StatusCode, message: &str) -> Response {
    let body = json!({ "message": message }).to_string();
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

pub(crate) fn set_consumer(
    state: &mut GatewayState,
    consumer_id: &str,
    credential_id: &str,
    kind: &str,
    identifier: &str,
) {
    let consumer = state
        .consumers
        .get(consumer_id)
        .cloned()
        .unwrap_or_else(|| ConsumerInfo {
            id: consumer_id.to_string(),
            ..Default::default()
        });
    state.consumer = Some(consumer);
    state.credential = Some(CredentialInfo {
        id: credential_id.to_string(),
        kind: kind.to_string(),
        identifier: identifier.to_string(),
    });
    state.anonymous = false;
}

pub(crate) fn remove_query_key(uri: &http::Uri, names: &[String]) -> Option<http::Uri> {
    let query = uri.query()?;
    let pairs: Vec<(String, String)> = url::form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect();
    let has = pairs
        .iter()
        .any(|(key, _)| names.iter().any(|name| name == key));
    if !has {
        return None;
    }
    let filtered: Vec<(String, String)> = pairs
        .into_iter()
        .filter(|(key, _)| !names.iter().any(|name| name == key))
        .collect();
    let encoded = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(filtered)
        .finish();
    let path = uri.path();
    let value = if encoded.is_empty() {
        path.to_string()
    } else {
        format!("{path}?{encoded}")
    };
    value.parse().ok()
}

pub(crate) fn resolve_anonymous(state: &GatewayState, anonymous: Option<&str>) -> Option<String> {
    let anonymous = anonymous?.trim();
    if anonymous.is_empty() {
        return None;
    }
    if state.consumers.contains_key(anonymous) {
        return Some(anonymous.to_string());
    }
    if let Some(id) = state.consumer_by_username.get(anonymous) {
        return Some(id.clone());
    }
    state.consumer_by_custom_id.get(anonymous).cloned()
}

pub(crate) fn manifest(
    name: &str,
    priority: i32,
    capabilities: Vec<Capability>,
    description: &str,
    config_schema: Value,
) -> PluginManifest {
    PluginManifest {
        name: name.to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        kind: PluginKind::Api,
        runtime: PluginRuntimeKind::Builtin,
        priority,
        description: description.to_string(),
        timeout_ms: 250,
        fail_policy: octra_plugin_api::FailPolicy::Open,
        capabilities,
        config_schema: Some(config_schema),
    }
}
