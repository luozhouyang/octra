//! End-to-end tests for the catch-all data plane router and the control plane.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use http_body_util::BodyExt;
use octra_api_core::{
    CreateConsumer, CreateKeyauthCredential, CreatePluginInstance, CreateRoute, CreateService,
    CreateTarget, CreateUpstream,
};
use octra_api_gateway::{
    ApiControlState, ApiDataState, ApiMetrics, AuditState, control_router, data_plane_router,
    record_control_audit,
};
use octra_config::OctraConfig;
use octra_gateway_runtime::GatewayRuntime;
use octra_plugin_runtime::{PluginRegistry, PluginRuntime};
use octra_storage::Storage;
use tower::ServiceExt;

const PEER: &str = "127.0.0.1:4242";

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
    });
    Response::builder()
        .status(StatusCode::CREATED)
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-upstream", state.name)
        .body(Body::from(payload.to_string()))
        .unwrap()
        .into_response()
}

async fn spawn_mock(name: &'static str) -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
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

struct Gateway {
    router: Router,
    control: Router,
    storage: Arc<Storage>,
    runtime: Arc<GatewayRuntime>,
    plugins: Arc<PluginRuntime>,
}

async fn gateway() -> Gateway {
    gateway_with_config(OctraConfig::default()).await
}

async fn gateway_with_config(config: OctraConfig) -> Gateway {
    let config = Arc::new(config);
    let storage = Arc::new(Storage::connect_in_memory().await.unwrap());
    let runtime = Arc::new(GatewayRuntime::bootstrap(storage.clone()).await.unwrap());
    let mut registry = PluginRegistry::new();
    octra_plugin_builtin::register(&mut registry);
    let plugins = Arc::new(
        PluginRuntime::bootstrap(storage.clone(), Arc::new(registry))
            .await
            .unwrap(),
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

type ResponseParts = (StatusCode, HeaderMap, serde_json::Value);

async fn send(router: Router, request: Request<Body>) -> ResponseParts {
    let response = router.oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
    (status, headers, json)
}

impl Gateway {
    async fn reload(&self) {
        self.runtime.reload().await.unwrap();
        self.plugins.reload().await.unwrap();
    }

    async fn send(&self, request: Request<Body>) -> ResponseParts {
        send(self.router.clone(), request).await
    }

    async fn send_control(&self, request: Request<Body>) -> ResponseParts {
        send(self.control.clone(), request).await
    }
}

/// Builds a data plane request carrying connection info and a default host,
/// mirroring what the daemon's listener provides.
fn request(method: &str, uri: &str) -> axum::extract::Request {
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

fn get(uri: &str) -> axum::extract::Request {
    request("GET", uri)
}

fn header_default(value: &str) -> HeaderValue {
    HeaderValue::from_str(value).unwrap()
}

/// Converts an absolute URL into the `host:port` target form.
fn target_address(url: &str) -> String {
    let parsed = url::Url::parse(url).unwrap();
    let host = parsed.host_str().unwrap();
    format!("{host}:{}", parsed.port_or_known_default().unwrap())
}

/// Inserts a service with one upstream, one target, and one route.
async fn seed_service(
    storage: &Storage,
    service_id: &str,
    target_url: &str,
    require_auth: bool,
    prefix: &str,
    strip: bool,
) {
    let service = storage
        .create_service(&CreateService {
            id: Some(service_id.into()),
            name: service_id.into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let upstream = storage
        .create_upstream(&CreateUpstream {
            id: Some(format!("{service_id}-up")),
            name: service_id.into(),
            ..Default::default()
        })
        .await
        .unwrap();
    storage
        .create_target(&CreateTarget {
            id: Some(format!("{service_id}-tgt")),
            upstream_id: upstream.id,
            target: target_address(target_url),
            ..Default::default()
        })
        .await
        .unwrap();
    storage
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

#[tokio::test]
async fn proxies_with_rewrite_forwarded_headers_and_body() {
    let (addr, _calls) = spawn_mock("a").await;
    let gateway = gateway().await;
    seed_service(
        &gateway.storage,
        "svc-a",
        &format!("http://{addr}"),
        false,
        "/api",
        true,
    )
    .await;
    gateway.reload().await;

    let mut req = request("POST", "/api/v1/items?limit=5");
    let headers = req.headers_mut();
    headers.insert(header::HOST, header_default("localhost:8080"));
    headers.insert("x-request-id", header_default("req-123"));
    headers.insert("x-custom", header_default("keep"));
    headers.insert("connection", header_default("keep-alive"));
    *req.body_mut() = Body::from("hello-body");

    let (status, headers, json) = gateway.send(req).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(headers.get("x-upstream").unwrap(), "a");
    assert_eq!(headers.get("x-request-id").unwrap(), "req-123");

    assert_eq!(json["path"], "/v1/items");
    assert_eq!(json["query"], "limit=5");
    assert_eq!(json["body"], "hello-body");
    assert_eq!(json["host"], addr.to_string());
    assert_eq!(json["xfh"], "localhost:8080");
    assert_eq!(json["xfp"], "http");
    assert_eq!(json["request_id"], "req-123");
    assert_eq!(json["xff"], "127.0.0.1");
    // Hop-by-hop headers are stripped before forwarding.
    assert_eq!(json["connection"], "");
}

#[tokio::test]
async fn round_robins_across_two_targets() {
    let (addr_a, _calls_a) = spawn_mock("a").await;
    let (addr_b, _calls_b) = spawn_mock("b").await;

    let gateway = gateway().await;
    let service = gateway
        .storage
        .create_service(&CreateService {
            id: Some("svc".into()),
            name: "svc".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let upstream = gateway
        .storage
        .create_upstream(&CreateUpstream {
            id: Some("up".into()),
            name: "svc".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    for (id, url) in [("t1", addr_a), ("t2", addr_b)] {
        gateway
            .storage
            .create_target(&CreateTarget {
                id: Some(id.into()),
                upstream_id: upstream.id.clone(),
                target: url.to_string(),
                ..Default::default()
            })
            .await
            .unwrap();
    }
    gateway
        .storage
        .create_route(&CreateRoute {
            id: Some("r".into()),
            service_id: Some(service.id),
            ..Default::default()
        })
        .await
        .unwrap();
    gateway.reload().await;

    let mut servers = Vec::new();
    for _ in 0..4 {
        let (status, _, json) = gateway.send(get("/")).await;
        assert_eq!(status, StatusCode::CREATED);
        servers.push(json["server"].as_str().unwrap().to_string());
    }
    for pair in servers.chunks(2) {
        assert_ne!(pair[0], pair[1], "round-robin alternates: {servers:?}");
    }
}

#[tokio::test]
async fn rejects_without_api_key_and_accepts_key_header() {
    let (addr, _calls) = spawn_mock("a").await;
    let gateway = gateway().await;
    seed_service(
        &gateway.storage,
        "secure",
        &format!("http://{addr}"),
        true,
        "/",
        false,
    )
    .await;
    let consumer = gateway
        .storage
        .create_consumer(&CreateConsumer {
            username: Some("team".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    let key = "ok_test-key-value";
    gateway
        .storage
        .create_credential(&CreateKeyauthCredential {
            consumer_id: consumer.id,
            key: Some(key.into()),
            ..Default::default()
        })
        .await
        .unwrap();
    gateway.reload().await;

    let (status, headers, _) = gateway.send(get("/")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        headers.get(header::WWW_AUTHENTICATE).unwrap(),
        "Key realm=\"octra\""
    );

    let mut api_key = get("/");
    api_key.headers_mut().insert("apikey", header_default(key));
    assert_eq!(gateway.send(api_key).await.0, StatusCode::CREATED);

    let mut alternate = get("/");
    alternate
        .headers_mut()
        .insert("x-api-key", header_default(key));
    assert_eq!(gateway.send(alternate).await.0, StatusCode::CREATED);

    let mut wrong = get("/");
    wrong.headers_mut().insert("apikey", header_default("nope"));
    assert_eq!(gateway.send(wrong).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn unknown_route_is_404() {
    let gateway = gateway().await;
    gateway.reload().await;
    let (status, _, json) = gateway.send(get("/nothing-here")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json["error"]["code"], "no_route");
}

#[tokio::test]
async fn no_target_is_502_and_dead_target_is_502() {
    let gateway = gateway().await;
    let service = gateway
        .storage
        .create_service(&CreateService {
            id: Some("empty".into()),
            name: "empty".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    gateway
        .storage
        .create_upstream(&CreateUpstream {
            id: Some("empty-up".into()),
            name: "empty".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    gateway
        .storage
        .create_route(&CreateRoute {
            id: Some("empty-route".into()),
            service_id: Some(service.id),
            ..Default::default()
        })
        .await
        .unwrap();

    seed_service(
        &gateway.storage,
        "dead",
        "http://127.0.0.1:1",
        false,
        "/dead",
        false,
    )
    .await;
    gateway.reload().await;

    let (status, _, json) = gateway.send(get("/")).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(json["error"]["code"], "upstream_unavailable");

    let (status, _, _) = gateway.send(get("/dead")).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn metrics_endpoint_counts_requests() {
    let gateway = gateway().await;
    gateway.reload().await;
    let _ = gateway.send(get("/missing")).await;

    let (status, _, json) = gateway.send_control(get("/api/v1/api/metrics")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["requests_total"], 1);
    assert_eq!(json["status_4xx"], 1);
}

fn post(uri: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

#[tokio::test]
async fn control_plane_crud_validation_and_key_issuance() {
    let (addr, _calls) = spawn_mock("a").await;
    let storage = Arc::new(Storage::connect_in_memory().await.unwrap());
    let runtime = Arc::new(GatewayRuntime::bootstrap(storage.clone()).await.unwrap());
    let control = || control_router(ApiControlState::new(storage.clone(), runtime.clone()));

    let response = control()
        .oneshot(post(
            "/api/v1/services",
            serde_json::json!({ "id": "svc", "name": "svc" }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    // A service pointing to a missing service is a 404.
    let response = control()
        .oneshot(post(
            "/api/v1/routes",
            serde_json::json!({ "service_id": "missing", "paths": ["/"] }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let response = control()
        .oneshot(post(
            "/api/v1/upstreams",
            serde_json::json!({ "id": "up", "name": "svc", "algorithm": "round-robin" }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    let response = control()
        .oneshot(post(
            "/api/v1/targets",
            serde_json::json!({ "upstream_id": "up", "target": addr.to_string(), "weight": 1001 }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let response = control()
        .oneshot(post(
            "/api/v1/targets",
            serde_json::json!({ "upstream_id": "up", "target": "ftp://nope" }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let response = control()
        .oneshot(post(
            "/api/v1/targets",
            serde_json::json!({ "upstream_id": "up", "target": addr.to_string() }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    let response = control()
        .oneshot(post(
            "/api/v1/routes",
            serde_json::json!({ "id": "r", "service_id": "svc", "paths": ["/"], "methods": ["BREW"] }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let response = control()
        .oneshot(post(
            "/api/v1/routes",
            serde_json::json!({ "id": "r", "service_id": "svc", "paths": ["/"] }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    // Duplicate service name conflicts.
    let response = control()
        .oneshot(post(
            "/api/v1/services",
            serde_json::json!({ "id": "svc2", "name": "svc" }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);

    let response = control()
        .oneshot(post(
            "/api/v1/services",
            serde_json::json!({ "name": "bad", "connect_timeout": 0 }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // Mutations reloaded the runtime snapshot.
    assert_eq!(runtime.snapshot().await.routes.len(), 1);

    let response = control()
        .oneshot(post(
            "/api/v1/consumers",
            serde_json::json!({ "id": "c", "username": "consumer" }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    let response = control()
        .oneshot(post(
            "/api/v1/credentials",
            serde_json::json!({ "consumer_id": "c" }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let issued: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let plaintext = issued["key"].as_str().unwrap();
    assert!(plaintext.starts_with("ok_"));
    let stored = storage
        .list_credentials(Some("c"))
        .await
        .unwrap()
        .into_iter()
        .find(|credential| credential.key == plaintext)
        .unwrap();
    assert_eq!(stored.consumer_id, "c");

    let response = control()
        .oneshot(post(
            "/api/v1/credentials",
            serde_json::json!({ "consumer_id": "missing" }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    // List filters reach through the query string.
    let response = control()
        .oneshot(
            Request::builder()
                .uri("/api/v1/targets?upstream_id=up")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let targets: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(targets.as_array().unwrap().len(), 1);

    // The configured route is proxied end to end through a fresh data router.
    let data = data_plane_router(ApiDataState::new(
        runtime.clone(),
        Arc::new(OctraConfig::default()),
    ));
    let mut req = Request::builder().uri("/").body(Body::empty()).unwrap();
    req.extensions_mut()
        .insert(axum::extract::ConnectInfo::<SocketAddr>(
            PEER.parse().unwrap(),
        ));
    let (status, _, _) = send(data, req).await;
    assert_eq!(status, StatusCode::CREATED);
}

async fn create_plugin_config(
    storage: &Storage,
    plugin_name: &str,
    scope: octra_plugin_api::Scope,
    scope_id: Option<&str>,
    config: serde_json::Value,
) {
    let mut input = CreatePluginInstance {
        name: plugin_name.into(),
        config: Some(config),
        ..Default::default()
    };
    match scope {
        octra_plugin_api::Scope::Global => {}
        octra_plugin_api::Scope::Service => input.service_id = scope_id.map(str::to_string),
        octra_plugin_api::Scope::Route => input.route_id = scope_id.map(str::to_string),
        octra_plugin_api::Scope::Consumer => input.consumer_id = scope_id.map(str::to_string),
    }
    storage.create_plugin_instance(&input).await.unwrap();
}

#[tokio::test]
async fn cors_preflight_short_circuits_before_auth() {
    let (addr, _calls) = spawn_mock("a").await;
    let gateway = gateway().await;
    seed_service(
        &gateway.storage,
        "svc-cors",
        &format!("http://{addr}"),
        true,
        "/api",
        false,
    )
    .await;
    create_plugin_config(
        &gateway.storage,
        "cors",
        octra_plugin_api::Scope::Global,
        None,
        serde_json::json!({"allow_origins": ["https://app.test"]}),
    )
    .await;
    gateway.reload().await;

    let mut preflight = request("OPTIONS", "/api/items");
    preflight
        .headers_mut()
        .insert(header::ORIGIN, header_default("https://app.test"));
    preflight
        .headers_mut()
        .insert("access-control-request-method", header_default("GET"));
    preflight.headers_mut().insert(
        "access-control-request-headers",
        header_default("authorization"),
    );

    let (status, headers, _) = gateway.send(preflight).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        headers.get("access-control-allow-origin").unwrap(),
        "https://app.test"
    );
    assert!(
        headers
            .get("access-control-allow-methods")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("GET")
    );
    assert_eq!(
        headers.get("access-control-allow-headers").unwrap(),
        "authorization"
    );
    assert!(
        headers
            .get("vary")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("origin")
    );

    // Authentication is still enforced for real requests.
    let (status, _, _) = gateway.send(get("/api/items")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn header_rewrite_plugin_applies_to_request_and_response() {
    let (addr, _calls) = spawn_mock("a").await;
    let gateway = gateway().await;
    seed_service(
        &gateway.storage,
        "svc-headers",
        &format!("http://{addr}"),
        false,
        "/api",
        false,
    )
    .await;
    create_plugin_config(
        &gateway.storage,
        "header-rewrite",
        octra_plugin_api::Scope::Global,
        None,
        serde_json::json!({
            "request": {"set": {"x-plugin": "global"}},
            "response": {"set": {"x-plugin-response": "rewritten"}}
        }),
    )
    .await;
    gateway.reload().await;

    let (status, headers, json) = gateway.send(get("/api/items")).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(json["plugin_header"], "global");
    assert_eq!(headers.get("x-plugin-response").unwrap(), "rewritten");
}

#[tokio::test]
async fn route_scoped_plugin_overrides_global_config() {
    let (addr, _calls) = spawn_mock("a").await;
    let gateway = gateway().await;
    seed_service(
        &gateway.storage,
        "svc-scope",
        &format!("http://{addr}"),
        false,
        "/api",
        false,
    )
    .await;
    create_plugin_config(
        &gateway.storage,
        "header-rewrite",
        octra_plugin_api::Scope::Global,
        None,
        serde_json::json!({"request": {"set": {"x-plugin": "global"}}}),
    )
    .await;
    create_plugin_config(
        &gateway.storage,
        "header-rewrite",
        octra_plugin_api::Scope::Route,
        Some("svc-scope-route"),
        serde_json::json!({"request": {"set": {"x-plugin": "route"}}}),
    )
    .await;
    gateway.reload().await;

    let (status, _, json) = gateway.send(get("/api/items")).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(json["plugin_header"], "route");
}

#[tokio::test]
async fn rate_limit_plugin_rejects_after_burst() {
    let (addr, _calls) = spawn_mock("a").await;
    let gateway = gateway().await;
    seed_service(
        &gateway.storage,
        "svc-rate",
        &format!("http://{addr}"),
        false,
        "/api",
        false,
    )
    .await;
    create_plugin_config(
        &gateway.storage,
        "rate-limit",
        octra_plugin_api::Scope::Global,
        None,
        serde_json::json!({"requests_per_second": 1, "burst": 1, "key": "global"}),
    )
    .await;
    gateway.reload().await;

    let (first, _, _) = gateway.send(get("/api/items")).await;
    assert_eq!(first, StatusCode::CREATED);
    let (second, headers, json) = gateway.send(get("/api/items")).await;
    assert_eq!(second, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(json["error"]["code"], "rate_limited");
    assert!(headers.get(header::RETRY_AFTER).is_some());
}

#[tokio::test]
async fn plugin_configs_crud_via_control_plane() {
    let gateway = gateway().await;

    let list = Request::builder()
        .uri("/api/v1/plugins")
        .header(header::HOST, "localhost")
        .body(Body::empty())
        .unwrap();
    let (status, _, json) = gateway.send_control(list).await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<&str> = json
        .as_array()
        .unwrap()
        .iter()
        .map(|plugin| plugin["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"cors"));
    assert!(names.contains(&"rate-limit"));

    let create = Request::builder()
        .method("POST")
        .uri("/api/v1/plugins/request-id/instances")
        .header(header::HOST, "localhost")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"config":{}}"#))
        .unwrap();
    let (status, _, json) = gateway.send_control(create).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(json["name"], "request-id");
    let config_id = json["id"].as_str().unwrap().to_string();

    let update = Request::builder()
        .method("PUT")
        .uri(format!("/api/v1/plugin-instances/{config_id}"))
        .header(header::HOST, "localhost")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"enabled":false}"#))
        .unwrap();
    let (status, _, json) = gateway.send_control(update).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["enabled"], false);

    let delete = Request::builder()
        .method("DELETE")
        .uri(format!("/api/v1/plugin-instances/{config_id}"))
        .header(header::HOST, "localhost")
        .body(Body::empty())
        .unwrap();
    let (status, _, _) = gateway.send_control(delete).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn plugin_config_rejects_unknown_scope_target() {
    let gateway = gateway().await;
    let create = Request::builder()
        .method("POST")
        .uri("/api/v1/plugins/cors/instances")
        .header(header::HOST, "localhost")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"service_id":"missing","config":{}}"#))
        .unwrap();
    let (status, _, json) = gateway.send_control(create).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json["error"]["code"], "not_found");
}

#[tokio::test]
async fn retries_replay_idempotent_request_body() {
    let (addr, calls) = spawn_mock("live").await;
    let gateway = gateway().await;
    let service = gateway
        .storage
        .create_service(&CreateService {
            id: Some("svc-retry".into()),
            name: "svc-retry".into(),
            retries: Some(1),
            ..Default::default()
        })
        .await
        .unwrap();
    let upstream = gateway
        .storage
        .create_upstream(&CreateUpstream {
            id: Some("retry-up".into()),
            name: "svc-retry".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    for (id, target) in [
        ("retry-dead", "127.0.0.1:1".to_string()),
        ("retry-live", addr.to_string()),
    ] {
        gateway
            .storage
            .create_target(&CreateTarget {
                id: Some(id.into()),
                upstream_id: upstream.id.clone(),
                target,
                ..Default::default()
            })
            .await
            .unwrap();
    }
    gateway
        .storage
        .create_route(&CreateRoute {
            id: Some("retry-route".into()),
            service_id: Some(service.id),
            ..Default::default()
        })
        .await
        .unwrap();
    gateway.reload().await;

    for _ in 0..6 {
        let mut req = request("PUT", "/items");
        *req.body_mut() = Body::from("replay-me");
        let (status, _, json) = gateway.send(req).await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(json["body"], "replay-me");
    }
    assert_eq!(calls.lock().unwrap().len(), 6);
}

#[tokio::test]
async fn rejects_body_over_max_body_bytes() {
    let mut config = OctraConfig::default();
    config.gateway.max_body_bytes = 8;
    let gateway = gateway_with_config(config).await;
    seed_service(
        &gateway.storage,
        "svc-limit",
        "http://127.0.0.1:1",
        false,
        "/api",
        false,
    )
    .await;
    gateway.reload().await;

    let mut req = request("POST", "/api/items");
    req.headers_mut()
        .insert(header::CONTENT_LENGTH, header_default("9"));
    *req.body_mut() = Body::from("012345678");
    let (status, _, json) = gateway.send(req).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(json["error"]["code"], "payload_too_large");
}

#[tokio::test]
async fn falls_back_to_an_eligible_target() {
    let (addr, _calls) = spawn_mock("live").await;
    let gateway = gateway().await;
    let service = gateway
        .storage
        .create_service(&CreateService {
            id: Some("svc-fallback".into()),
            name: "svc-fallback".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let upstream = gateway
        .storage
        .create_upstream(&CreateUpstream {
            id: Some("svc-fallback-pool".into()),
            name: "svc-fallback".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    gateway
        .storage
        .create_target(&CreateTarget {
            id: Some("primary-target".into()),
            upstream_id: upstream.id.clone(),
            target: "127.0.0.1:1".into(),
            weight: Some(0),
            ..Default::default()
        })
        .await
        .unwrap();
    gateway
        .storage
        .create_target(&CreateTarget {
            id: Some("secondary-target".into()),
            upstream_id: upstream.id,
            target: addr.to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    gateway
        .storage
        .create_route(&CreateRoute {
            id: Some("fallback-route".into()),
            service_id: Some(service.id),
            ..Default::default()
        })
        .await
        .unwrap();
    gateway.reload().await;

    let (status, _, json) = gateway.send(get("/")).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(json["server"], "live");
}

#[tokio::test]
async fn prometheus_metrics_are_exposed_on_the_control_plane() {
    let gateway = gateway().await;
    gateway.reload().await;
    let _ = gateway.send(get("/missing")).await;

    let request = Request::builder()
        .uri("/api/v1/api/metrics/prometheus")
        .header(header::HOST, "localhost")
        .body(Body::empty())
        .unwrap();
    let response = gateway.control.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE).unwrap(),
        "text/plain; version=0.0.4; charset=utf-8"
    );
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("octra_api_requests_total 1"));
    assert!(text.contains(r#"octra_api_responses_total{class="4xx"} 1"#));
}

async fn wait_for_request_logs(gateway: &Gateway) -> Vec<octra_storage::RequestLog> {
    for _ in 0..50 {
        let logs = gateway
            .storage
            .list_request_logs(10, None, None)
            .await
            .unwrap();
        if !logs.is_empty() {
            return logs;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Vec::new()
}

#[tokio::test]
async fn request_logs_are_persisted_and_queryable() {
    let (addr, _calls) = spawn_mock("a").await;
    let gateway = gateway().await;
    seed_service(
        &gateway.storage,
        "svc-logs",
        &format!("http://{addr}"),
        false,
        "/api",
        false,
    )
    .await;
    gateway.reload().await;

    let (status, _, _) = gateway.send(get("/api/items")).await;
    assert_eq!(status, StatusCode::CREATED);

    let logs = wait_for_request_logs(&gateway).await;
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].method, "GET");
    assert_eq!(logs[0].path, "/api/items");
    assert_eq!(logs[0].status, 201);
    assert_eq!(logs[0].route_id.as_deref(), Some("svc-logs-route"));
    assert_eq!(logs[0].service_id.as_deref(), Some("svc-logs"));

    let query = Request::builder()
        .uri("/api/v1/system/logs?limit=10&route_id=svc-logs-route")
        .header(header::HOST, "localhost")
        .body(Body::empty())
        .unwrap();
    let (status, _, json) = gateway.send_control(query).await;
    assert_eq!(status, StatusCode::OK);
    let entries = json.as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["status"], 201);
    assert_eq!(entries[0]["request_id"], logs[0].request_id);
}

#[tokio::test]
async fn control_mutations_write_audit_rows() {
    let gateway = gateway().await;
    let create = Request::builder()
        .method("POST")
        .uri("/api/v1/services")
        .header(header::HOST, "localhost")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"id":"audited","name":"audited"}"#))
        .unwrap();
    let (status, _, _) = gateway.send_control(create).await;
    assert_eq!(status, StatusCode::CREATED);

    let mut rows = Vec::new();
    for _ in 0..50 {
        rows = gateway.storage.list_audit_logs(10).await.unwrap();
        if !rows.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!rows.is_empty());
    assert_eq!(rows[0].action, "control.post");
    assert_eq!(rows[0].target.as_deref(), Some("/api/v1/services"));

    let query = Request::builder()
        .uri("/api/v1/audit-logs?limit=10")
        .header(header::HOST, "localhost")
        .body(Body::empty())
        .unwrap();
    let (status, _, json) = gateway.send_control(query).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!json.as_array().unwrap().is_empty());
}
