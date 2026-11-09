//! Built-in API Gateway plugins.
//!
//! These plugins validate their JSON configuration at creation time and keep
//! the hot path allocation-light. They are trusted, in-process Rust code;
//! third-party extensions will use the WASM runtime instead.

mod acl;
mod basic_auth;
mod circuit_breaker;
mod correlation_id;
mod cors;
mod header_rewrite;
mod ip_restriction;
mod jwt;
mod key_auth;
mod rate_limiting;
mod request_size_limiting;
mod request_termination;
mod request_transformer;
mod response_transformer;
mod transform;

use axum::body::Body;
use axum::response::Response;
use http::{HeaderValue, StatusCode, header};
use octra_api_plugin_runtime::PluginRegistry;
use octra_api_plugin_types::{
    Capability, ConsumerInfo, CredentialInfo, GatewayState, PluginKind, PluginManifest,
    PluginRuntimeKind,
};
use serde_json::{Value, json};

pub use circuit_breaker::CircuitBreakerFactory;
pub use correlation_id::CorrelationIdFactory;
pub use cors::CorsFactory;
pub use header_rewrite::HeaderRewriteFactory;
pub use rate_limiting::RateLimitingFactory;

pub use acl::AclFactory;
pub use basic_auth::BasicAuthFactory;
pub use ip_restriction::IpRestrictionFactory;
pub use jwt::JwtFactory;
pub use key_auth::KeyAuthFactory;
pub use request_size_limiting::RequestSizeLimitingFactory;
pub use request_termination::RequestTerminationFactory;
pub use request_transformer::RequestTransformerFactory;
pub use response_transformer::ResponseTransformerFactory;

pub fn register(registry: &mut PluginRegistry) {
    registry.register(CorrelationIdFactory);
    registry.register(CorsFactory);
    registry.register(RateLimitingFactory);
    registry.register(CircuitBreakerFactory);
    registry.register(HeaderRewriteFactory);
    registry.register(KeyAuthFactory);
    registry.register(BasicAuthFactory);
    registry.register(JwtFactory);
    registry.register(AclFactory);
    registry.register(IpRestrictionFactory);
    registry.register(RequestSizeLimitingFactory);
    registry.register(RequestTerminationFactory);
    registry.register(RequestTransformerFactory);
    registry.register(ResponseTransformerFactory);
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
        fail_policy: octra_api_plugin_types::FailPolicy::Open,
        phases: Vec::new(),
        capabilities,
        config_schema: Some(config_schema),
        api_version: "3".to_string(),
        entities: Vec::new(),
        migrations: Vec::new(),
        admin_routes: Vec::new(),
        subscriptions: Vec::new(),
        jobs: Vec::new(),
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use bytes::Bytes;
    use http::{HeaderMap, Method, StatusCode, Uri};
    use octra_api_core::{Route, Service};
    use octra_api_plugin_types::{GatewayState, PluginAction, RequestContext};

    pub(crate) fn route() -> Route {
        Route {
            id: "route".into(),
            name: None,
            protocols: vec!["http".into()],
            methods: Vec::new(),
            hosts: Vec::new(),
            paths: vec!["/".into()],
            headers: Default::default(),
            strip_path: false,
            preserve_host: false,
            regex_priority: 0,
            service_id: Some("svc".into()),
            enabled: true,
            tags: Vec::new(),
            created_at: 0,
            updated_at: 0,
        }
    }

    pub(crate) fn service() -> Service {
        Service {
            id: "svc".into(),
            name: "svc".into(),
            protocol: "http".into(),
            host: "up.test".into(),
            port: 80,
            path: None,
            retries: 0,
            connect_timeout: 1000,
            write_timeout: 1000,
            read_timeout: 1000,
            enabled: true,
            description: String::new(),
            tags: Vec::new(),
            created_at: 0,
            updated_at: 0,
        }
    }

    pub(crate) struct RequestFixture {
        pub(crate) state: GatewayState,
        pub(crate) method: Method,
        pub(crate) uri: Uri,
        pub(crate) headers: HeaderMap,
        pub(crate) route: Route,
        pub(crate) service: Service,
        pub(crate) client_ip: Option<std::net::IpAddr>,
    }

    impl Default for RequestFixture {
        fn default() -> Self {
            Self {
                state: GatewayState::default(),
                method: Method::GET,
                uri: "/".parse().expect("uri"),
                headers: HeaderMap::new(),
                route: route(),
                service: service(),
                client_ip: None,
            }
        }
    }

    impl RequestFixture {
        pub(crate) fn context(&mut self) -> RequestContext<'_> {
            RequestContext {
                request_id: "req",
                state: &mut self.state,
                matched_path: "/",
                method: &self.method,
                uri: &mut self.uri,
                headers: &mut self.headers,
                client_ip: self.client_ip,
                consumer_id: None,
                credential_id: None,
                route: &self.route,
                service: &self.service,
            }
        }
    }

    pub(crate) fn short_circuit(action: PluginAction) -> axum::response::Response {
        match action {
            PluginAction::ShortCircuit(response) => response,
            PluginAction::Continue => panic!("expected a short-circuit response"),
        }
    }

    pub(crate) async fn response_parts(
        response: axum::response::Response,
    ) -> (StatusCode, HeaderMap, Bytes) {
        let status = response.status();
        let headers = response.headers().clone();
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("response body");
        (status, headers, body)
    }
}
