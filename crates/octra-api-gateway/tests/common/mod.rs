//! Shared integration test harness: mock upstream, gateway construction, and
//! request helpers.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use base64::Engine as _;
use http_body_util::BodyExt;
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use octra_api_core::{
    CreateBasicauthCredential, CreateConsumer, CreateJwtSecret, CreateKeyauthCredential,
    CreatePluginInstance, CreateRoute, CreateService, CreateTarget, CreateUpstream,
};
use octra_api_gateway::{
    ApiControlState, ApiDataState, ApiMetrics, AuditState, WasmControlState, control_router,
    data_plane_router, record_control_audit,
};
use octra_api_plugin_runtime::{
    CompositePluginFactoryResolver, PluginFactoryResolver, PluginRegistry, PluginRuntime,
};
use octra_api_plugin_wasm::{WasmEngine, WasmHostProvider, WasmLimits, WasmPluginCatalog};
use octra_config::OctraConfig;
use octra_plugin_storage::PluginStorageRoot;
use octra_runtime::GatewayRuntime;
use octra_storage::Storage;
use serde_json::{Value, json};
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

    let method = request.method().to_string();
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
    let mut header_values = serde_json::Map::new();
    for (name, value) in &headers {
        if let Ok(value) = value.to_str() {
            header_values.insert(name.to_string(), Value::String(value.to_string()));
        }
    }

    let payload = serde_json::json!({
        "server": state.name,
        "method": method,
        "path": path,
        "query": query,
        "headers": header_values,
        "body": String::from_utf8_lossy(&body),
        "host": header("host"),
        "xff": header("x-forwarded-for"),
        "xfh": header("x-forwarded-host"),
        "xfp": header("x-forwarded-proto"),
        "request_id": header("x-request-id"),
        "kong_request_id": header("kong-request-id"),
        "x_correlation_id": header("x-correlation-id"),
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
        .header("x-old", "old-value")
        .header("x-remove", "gone")
        .header("x-replace", "old")
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
    gateway_with_wasm_state(config, None).await
}

pub async fn gateway_with_wasm_state(
    config: OctraConfig,
    wasm: Option<Arc<WasmControlState>>,
) -> Gateway {
    let config = Arc::new(config);
    let storage = Arc::new(Storage::connect_in_memory().await.unwrap());
    let runtime = Arc::new(GatewayRuntime::bootstrap(storage.clone()).await.unwrap());
    let mut registry = PluginRegistry::new();
    octra_api_plugin_builtin::register(&mut registry);
    let plugin_instances = storage.api.list_plugin_instances().await.unwrap();
    let plugins = match &wasm {
        Some(wasm) => {
            let registry = Arc::new(registry);
            let resolver: Arc<dyn PluginFactoryResolver> = Arc::new(
                CompositePluginFactoryResolver::new(registry.clone(), wasm.catalog.clone()),
            );
            let runtime =
                PluginRuntime::with_resolver(registry, resolver, wasm.host_provider.clone());
            runtime
                .reload(plugin_instances)
                .expect("plugin chains compile");
            Arc::new(runtime)
        }
        None => Arc::new(
            PluginRuntime::with_plugins(Arc::new(registry), plugin_instances)
                .expect("plugin chains compile"),
        ),
    };
    let metrics = Arc::new(ApiMetrics::default());
    let data =
        ApiDataState::with_plugins(runtime.clone(), config, metrics.clone(), plugins.clone())
            .with_storage(storage.clone());
    let mut control_state =
        ApiControlState::with_plugins(storage.clone(), runtime.clone(), metrics, plugins.clone());
    if let Some(wasm) = wasm {
        control_state = control_state.with_wasm(wasm);
    }
    let control = control_router(control_state).layer(axum::middleware::from_fn_with_state(
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

/// Non-echo behaviour for the mock upstream.
#[derive(Clone, Copy, Debug)]
pub enum MockBehaviour {
    /// Respond with the given status and a small JSON body.
    Status(u16),
    /// Respond with `Content-Type: application/json` and an invalid body.
    InvalidJson,
    /// Wait before responding; use only with a plugin timeout shorter than the delay.
    Delay(Duration),
}

#[derive(Clone, Debug)]
pub struct MockCall {
    pub method: String,
    pub path: String,
    pub query: String,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

pub struct MockUpstream {
    pub addr: SocketAddr,
    pub calls: Arc<Mutex<Vec<MockCall>>>,
    pub hits: Arc<AtomicUsize>,
}

#[derive(Clone)]
struct MockVariantState {
    behaviour: MockBehaviour,
    calls: Arc<Mutex<Vec<MockCall>>>,
    hits: Arc<AtomicUsize>,
}

async fn mock_variant(State(state): State<MockVariantState>, request: Request<Body>) -> Response {
    use axum::body::to_bytes;

    state.hits.fetch_add(1, Ordering::SeqCst);
    let method = request.method().to_string();
    let path = request.uri().path().to_string();
    let query = request.uri().query().unwrap_or_default().to_string();
    let headers = request.headers().clone();
    let body = to_bytes(request.into_body(), 1024 * 1024)
        .await
        .unwrap_or_default();
    state.calls.lock().unwrap().push(MockCall {
        method,
        path: path.clone(),
        query: query.clone(),
        headers,
        body: body.to_vec(),
    });
    let respond = |status: StatusCode, content_type: &str, payload: String| {
        Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, content_type)
            .body(Body::from(payload))
            .unwrap()
    };
    match state.behaviour {
        MockBehaviour::Status(status) => {
            let status = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            respond(
                status,
                "application/json",
                json!({ "status": status.as_u16() }).to_string(),
            )
        }
        MockBehaviour::InvalidJson => {
            respond(StatusCode::OK, "application/json", "not-json{".to_string())
        }
        MockBehaviour::Delay(delay) => {
            tokio::time::sleep(delay).await;
            respond(
                StatusCode::CREATED,
                "application/json",
                json!({ "path": path, "query": query }).to_string(),
            )
        }
    }
}

pub async fn spawn_mock_advanced(behaviour: MockBehaviour) -> MockUpstream {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let hits = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .fallback(any(mock_variant))
        .with_state(MockVariantState {
            behaviour,
            calls: calls.clone(),
            hits: hits.clone(),
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    MockUpstream { addr, calls, hits }
}

pub async fn spawn_status_mock(status: u16) -> MockUpstream {
    spawn_mock_advanced(MockBehaviour::Status(status)).await
}

pub fn hit_count(upstream: &MockUpstream) -> usize {
    upstream.hits.load(Ordering::SeqCst)
}

pub fn recorded_calls(upstream: &MockUpstream) -> Vec<MockCall> {
    upstream.calls.lock().unwrap().clone()
}

pub struct RawResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

pub async fn send_raw(router: Router, request: Request<Body>) -> RawResponse {
    let response = router.oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    RawResponse {
        status,
        headers,
        body,
    }
}

impl Gateway {
    pub async fn send_raw(&self, request: Request<Body>) -> RawResponse {
        send_raw(self.router.clone(), request).await
    }
}

pub fn post_json(uri: &str, body: Value) -> Request<Body> {
    with_body(request("POST", uri), "application/json", body.to_string())
}

pub fn with_header(mut request: Request<Body>, name: &str, value: &str) -> Request<Body> {
    request.headers_mut().insert(
        name.parse::<axum::http::HeaderName>().unwrap(),
        header_default(value),
    );
    request
}

pub fn with_headers(mut request: Request<Body>, pairs: &[(&str, &str)]) -> Request<Body> {
    for (name, value) in pairs {
        request = with_header(request, name, value);
    }
    request
}

pub fn with_query(mut request: Request<Body>, query: &str) -> Request<Body> {
    let path = request.uri().path().to_string();
    let uri = if query.is_empty() {
        path
    } else {
        format!("{path}?{query}")
    };
    *request.uri_mut() = uri.parse().unwrap();
    request
}

pub fn with_body(
    mut request: Request<Body>,
    content_type: &str,
    body: impl Into<Vec<u8>>,
) -> Request<Body> {
    request
        .headers_mut()
        .insert(header::CONTENT_TYPE, header_default(content_type));
    *request.body_mut() = Body::from(body.into());
    request
}

pub fn with_peer(mut request: Request<Body>, peer: &str) -> Request<Body> {
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        peer.parse::<SocketAddr>().unwrap(),
    ));
    request
}

pub fn with_xff(request: Request<Body>, value: &str) -> Request<Body> {
    let request = with_header(request, "x-forwarded-for", value);
    with_header(request, "x-forwarded-proto", "https")
}

pub async fn create_consumer(storage: &Storage, username: &str) -> String {
    storage
        .api
        .create_consumer(&CreateConsumer {
            username: Some(username.into()),
            ..Default::default()
        })
        .await
        .unwrap()
        .id
}

pub async fn create_consumer_with_custom_id(
    storage: &Storage,
    username: &str,
    custom_id: &str,
) -> String {
    storage
        .api
        .create_consumer(&CreateConsumer {
            username: Some(username.into()),
            custom_id: Some(custom_id.into()),
            ..Default::default()
        })
        .await
        .unwrap()
        .id
}

pub async fn add_key_credential(storage: &Storage, consumer_id: &str, key: &str) -> String {
    storage
        .api
        .create_credential(&CreateKeyauthCredential {
            consumer_id: consumer_id.into(),
            key: Some(key.into()),
            ..Default::default()
        })
        .await
        .unwrap()
        .id
}

pub async fn add_basic_credential(
    storage: &Storage,
    consumer_id: &str,
    username: &str,
    credential_password: &str,
) -> String {
    storage
        .api
        .create_basicauth(&CreateBasicauthCredential {
            consumer_id: consumer_id.into(),
            username: username.into(),
            password: bcrypt::hash(credential_password, 4).unwrap(),
            ..Default::default()
        })
        .await
        .unwrap()
        .id
}

pub async fn add_jwt_credential(
    storage: &Storage,
    consumer_id: &str,
    key: &str,
    secret: &str,
    algorithm: &str,
) -> String {
    storage
        .api
        .create_jwt(&CreateJwtSecret {
            consumer_id: consumer_id.into(),
            key: key.into(),
            secret: secret.into(),
            algorithm: Some(algorithm.into()),
            ..Default::default()
        })
        .await
        .unwrap()
        .id
}

pub fn basic_auth_header(username: &str, credential_password: &str) -> HeaderValue {
    let encoded = base64::engine::general_purpose::STANDARD
        .encode(format!("{username}:{credential_password}"));
    header_default(&format!("Basic {encoded}"))
}

pub fn bearer_header(token: &str) -> HeaderValue {
    header_default(&format!("Bearer {token}"))
}

pub fn hs256_token(secret: &str, claims: Value) -> String {
    encode(
        &Header::new(Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .unwrap()
}

pub struct SeededService {
    pub service_id: String,
    pub upstream_id: String,
    pub route_ids: Vec<String>,
}

pub async fn seed_service_with_routes(
    storage: &Storage,
    service_id: &str,
    target_url: &str,
    routes: &[(&str, bool)],
) -> SeededService {
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
            upstream_id: upstream.id.clone(),
            target: target_address(target_url),
            ..Default::default()
        })
        .await
        .unwrap();
    let mut route_ids = Vec::new();
    for (index, (path, strip)) in routes.iter().enumerate() {
        let route = storage
            .api
            .create_route(&CreateRoute {
                id: Some(format!("{service_id}-route-{index}")),
                service_id: Some(service.id.clone()),
                paths: Some(vec![(*path).into()]),
                strip_path: Some(*strip),
                ..Default::default()
            })
            .await
            .unwrap();
        route_ids.push(route.id);
    }
    SeededService {
        service_id: service.id,
        upstream_id: upstream.id,
        route_ids,
    }
}

pub async fn seed_simple_service(
    storage: &Storage,
    service_id: &str,
    target_url: &str,
    path: &str,
) -> SeededService {
    seed_service_with_routes(storage, service_id, target_url, &[(path, true)]).await
}

#[allow(clippy::too_many_arguments)]
pub async fn plugin_instance(
    storage: &Storage,
    name: &str,
    instance_name: Option<&str>,
    scope: octra_api_plugin_types::Scope,
    scope_id: Option<&str>,
    protocols: &[&str],
    ordering: Option<Value>,
    config: Value,
) -> String {
    let mut input = CreatePluginInstance {
        name: name.into(),
        instance_name: instance_name.map(str::to_string),
        config: Some(config),
        protocols: (!protocols.is_empty())
            .then(|| protocols.iter().map(|value| (*value).to_string()).collect()),
        ordering,
        ..Default::default()
    };
    match scope {
        octra_api_plugin_types::Scope::Global => {}
        octra_api_plugin_types::Scope::Service => input.service_id = scope_id.map(str::to_string),
        octra_api_plugin_types::Scope::Route => input.route_id = scope_id.map(str::to_string),
        octra_api_plugin_types::Scope::Consumer => input.consumer_id = scope_id.map(str::to_string),
    }
    storage.api.create_plugin_instance(&input).await.unwrap().id
}

pub struct PluginFixture {
    pub gateway: Gateway,
    pub service_id: String,
}

pub async fn plugin_fixture(
    target_url: &str,
    path: &str,
    plugin: &str,
    instance: Option<&str>,
    config: Value,
) -> PluginFixture {
    let gateway = gateway().await;
    let service_id = "svc-plugin";
    let seeded = seed_simple_service(&gateway.storage, service_id, target_url, path).await;
    plugin_instance(
        &gateway.storage,
        plugin,
        instance,
        octra_api_plugin_types::Scope::Service,
        Some(&seeded.service_id),
        &[],
        None,
        config,
    )
    .await;
    gateway.reload().await;
    PluginFixture {
        gateway,
        service_id: seeded.service_id,
    }
}

/// Test environment mirroring the daemon's WASM bootstrap: plugin storage,
/// artifact directory, engine, catalog, host provider, and a control router
/// with WASM state attached.
pub struct WasmTestEnv {
    pub dir: tempfile::TempDir,
    pub gateway: Gateway,
    pub wasm: Arc<WasmControlState>,
    pub storage_root: Arc<PluginStorageRoot>,
    pub artifact_dir: PathBuf,
}

impl WasmTestEnv {
    pub async fn new() -> Self {
        Self::with_max_artifact_bytes(16 * 1024 * 1024).await
    }

    pub async fn with_max_artifact_bytes(max_artifact_bytes: u64) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let storage_root = Arc::new(PluginStorageRoot::new(dir.path().join("plugin-data")));
        let artifact_dir = dir.path().join("artifacts");
        std::fs::create_dir_all(&artifact_dir).unwrap();
        let engine = Arc::new(WasmEngine::new(WasmLimits::default()).unwrap());
        let catalog = Arc::new(WasmPluginCatalog::new());
        let host_provider = Arc::new(WasmHostProvider::new(storage_root.clone(), Vec::new()));
        let wasm = Arc::new(WasmControlState {
            engine,
            catalog,
            host_provider,
            storage_root: storage_root.clone(),
            artifact_dir: artifact_dir.clone(),
            max_artifact_bytes,
            trusted_keys: Vec::new(),
            allow_unsigned: true,
        });
        let gateway = gateway_with_wasm_state(OctraConfig::default(), Some(wasm.clone())).await;
        Self {
            dir,
            gateway,
            wasm,
            storage_root,
            artifact_dir,
        }
    }

    pub async fn reload(&self) {
        self.gateway.reload().await;
    }

    pub async fn send_control_json(&self, method: &str, uri: &str, body: Value) -> ResponseParts {
        let request = with_body(request(method, uri), "application/json", body.to_string());
        self.gateway.send_control(request).await
    }

    pub fn stage_example(&self) -> PathBuf {
        let destination = self.dir.path().join("staged-consumer-quota");
        octra_api_plugin_wasm::testing::stage_example_artifact(&destination)
            .expect("reference component not built");
        destination
    }

    pub async fn upload_example(&self) -> ResponseParts {
        let path = self.stage_example();
        self.send_control_json(
            "POST",
            "/admin/plugin-artifacts",
            json!({ "path": path.to_string_lossy() }),
        )
        .await
    }

    pub async fn install_example(&self, approve: &[&str]) -> ResponseParts {
        let (_, _, artifact) = self.upload_example().await;
        let artifact_id = artifact["id"]
            .as_str()
            .expect("upload response carries the artifact id");
        self.send_control_json(
            "POST",
            "/admin/plugins/consumer-quota/install",
            json!({
                "artifact_id": artifact_id,
                "approve_capabilities": approve,
            }),
        )
        .await
    }
}
