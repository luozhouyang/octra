//! Shared integration test harness: mock upstream, gateway construction, and
//! request helpers.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use http_body_util::BodyExt;
use octra_api_core::{
    CreatePluginInstance, CreateRoute, CreateService, CreateTarget, CreateUpstream,
};
use octra_api_gateway::{
    ApiControlState, ApiDataState, ApiMetrics, AuditState, control_router, data_plane_router,
    record_control_audit,
};
use octra_api_plugin_runtime::{PluginRegistry, PluginRuntime};
use octra_config::OctraConfig;
use octra_runtime::GatewayRuntime;
use octra_storage::Storage;
use serde_json::Value;
use tower::ServiceExt;

pub const PEER: &str = "127.0.0.1:4242";

/// Mock upstream: echoes the path, query, forwarded headers, body, and a
/// server name so tests can assert on exactly what the gateway forwarded.
#[derive(Clone)]
struct MockState {
    name: &'static str,
    calls: Arc<Mutex<Vec<String>>>,
}

async fn mock_echo(State(state): State<MockState>, request: Request<Body>) -> Response {
    use axum::body::to_bytes;

    let path = request.uri().path().to_string();
    let query = request.uri().query().unwrap_or_default().to_string();
    state.calls.lock().unwrap().push(path.clone());
    let headers = request.headers().clone();
    let body = to_bytes(request.into_body(), 1024 * 1024)
        .await
        .unwrap_or_default();

    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string()
    };

    let payload = serde_json::json!({
        "server": state.name,
        "path": path,
        "query": query,
        "body": String::from_utf8_lossy(&body),
        "host": header("host"),
        "xff": header("x-forwarded-for"),
        "xfh": header("x-forwarded-host"),
        "xfp": header("x-forwarded-proto"),
        "request_id": header("x-request-id"),
        "plugin_header": header("x-plugin"),
        "connection": header("connection"),
        "apikey": header("apikey"),
        "authorization": header("authorization"),
        "cookie": header("cookie"),
        "x_consumer_id": header("x-consumer-id"),
        "x_consumer_username": header("x-consumer-username"),
        "x_consumer_custom_id": header("x-consumer-custom-id"),
        "x_consumer_groups": header("x-consumer-groups"),
    });
    Response::builder()
        .status(StatusCode::CREATED)
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-upstream", state.name)
        .body(Body::from(payload.to_string()))
        .unwrap()
        .into_response()
}

pub async fn spawn_mock(name: &'static str) -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let app = Router::new()
        .fallback(any(mock_echo))
        .with_state(MockState {
            name,
            calls: calls.clone(),
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (addr, calls)
}

pub struct Gateway {
    pub router: Router,
    pub control: Router,
    pub storage: Arc<Storage>,
    pub runtime: Arc<GatewayRuntime>,
    pub plugins: Arc<PluginRuntime>,
}

pub async fn gateway() -> Gateway {
    gateway_with_config(OctraConfig::default()).await
}

pub async fn gateway_with_config(config: OctraConfig) -> Gateway {
    let config = Arc::new(config);
    let storage = Arc::new(Storage::connect_in_memory().await.unwrap());
    let runtime = Arc::new(GatewayRuntime::bootstrap(storage.clone()).await.unwrap());
    let mut registry = PluginRegistry::new();
    octra_api_plugin_builtin::register(&mut registry);
    let plugin_instances = storage.api.list_plugin_instances().await.unwrap();
    let plugins = Arc::new(
        PluginRuntime::with_plugins(Arc::new(registry), plugin_instances)
            .expect("plugin chains compile"),
    );
    let metrics = Arc::new(ApiMetrics::default());
    let data =
        ApiDataState::with_plugins(runtime.clone(), config, metrics.clone(), plugins.clone())
            .with_storage(storage.clone());
    let control = control_router(ApiControlState::with_plugins(
        storage.clone(),
        runtime.clone(),
        metrics,
        plugins.clone(),
    ))
    .layer(axum::middleware::from_fn_with_state(
        AuditState {
            storage: storage.clone(),
        },
        record_control_audit,
    ));
    Gateway {
        router: data_plane_router(data),
        control,
        storage,
        runtime,
        plugins,
    }
}

pub type ResponseParts = (StatusCode, HeaderMap, Value);

pub async fn send(router: Router, request: Request<Body>) -> ResponseParts {
    let response = router.oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&body).unwrap_or(Value::Null);
    (status, headers, json)
}

impl Gateway {
    pub async fn reload(&self) {
        self.runtime.reload().await.unwrap();
        let plugin_instances = self.storage.api.list_plugin_instances().await.unwrap();
        self.plugins.reload(plugin_instances).unwrap();
    }

    pub async fn send(&self, request: Request<Body>) -> ResponseParts {
        send(self.router.clone(), request).await
    }

    pub async fn send_control(&self, request: Request<Body>) -> ResponseParts {
        send(self.control.clone(), request).await
    }
}

/// Builds a data plane request carrying connection info and a default host,
/// mirroring what the daemon's listener provides.
pub fn request(method: &str, uri: &str) -> axum::extract::Request {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, "localhost")
        .body(Body::empty())
        .unwrap();
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        PEER.parse::<SocketAddr>().unwrap(),
    ));
    request
}

pub fn get(uri: &str) -> axum::extract::Request {
    request("GET", uri)
}

pub fn header_default(value: &str) -> HeaderValue {
    HeaderValue::from_str(value).unwrap()
}

/// Converts an absolute URL into the `host:port` target form.
pub fn target_address(url: &str) -> String {
    let parsed = url::Url::parse(url).unwrap();
    let host = parsed.host_str().unwrap();
    format!("{host}:{}", parsed.port_or_known_default().unwrap())
}

/// Inserts a service with one upstream, one target, and one route.
pub async fn seed_service(
    storage: &Storage,
    service_id: &str,
    target_url: &str,
    require_auth: bool,
    prefix: &str,
    strip: bool,
) {
    let service = storage
        .api
        .create_service(&CreateService {
            id: Some(service_id.into()),
            name: service_id.into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let upstream = storage
        .api
        .create_upstream(&CreateUpstream {
            id: Some(format!("{service_id}-up")),
            name: service_id.into(),
            ..Default::default()
        })
        .await
        .unwrap();
    storage
        .api
        .create_target(&CreateTarget {
            id: Some(format!("{service_id}-tgt")),
            upstream_id: upstream.id,
            target: target_address(target_url),
            ..Default::default()
        })
        .await
        .unwrap();
    storage
        .api
        .create_route(&CreateRoute {
            id: Some(format!("{service_id}-route")),
            service_id: Some(service.id.clone()),
            paths: Some(vec![prefix.into()]),
            strip_path: Some(strip),
            ..Default::default()
        })
        .await
        .unwrap();
    if require_auth {
        storage
            .api
            .create_plugin_instance(&CreatePluginInstance {
                name: "key-auth".into(),
                service_id: Some(service.id),
                config: Some(serde_json::json!({
                    "key_names": ["apikey", "x-api-key"],
                })),
                ..Default::default()
            })
            .await
            .unwrap();
    }
}

/// Adds a service-scoped plugin instance.
pub async fn service_plugin(storage: &Storage, service_id: &str, name: &str, config: Value) {
    storage
        .api
        .create_plugin_instance(&CreatePluginInstance {
            name: name.into(),
            service_id: Some(service_id.into()),
            config: Some(config),
            ..Default::default()
        })
        .await
        .unwrap();
}

pub async fn create_plugin_config(
    storage: &Storage,
    plugin_name: &str,
    scope: octra_api_plugin_types::Scope,
    scope_id: Option<&str>,
    config: Value,
) {
    let mut input = CreatePluginInstance {
        name: plugin_name.into(),
        config: Some(config),
        ..Default::default()
    };
    match scope {
        octra_api_plugin_types::Scope::Global => {}
        octra_api_plugin_types::Scope::Service => input.service_id = scope_id.map(str::to_string),
        octra_api_plugin_types::Scope::Route => input.route_id = scope_id.map(str::to_string),
        octra_api_plugin_types::Scope::Consumer => input.consumer_id = scope_id.map(str::to_string),
    }
    storage.api.create_plugin_instance(&input).await.unwrap();
}
