//! End-to-end tests for the catch-all data plane router and the control plane.

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
    CreateApiCredential, CreateConsumer, CreateRoute, CreateService, CreateTarget, CreateUpstream,
};
use octra_api_gateway::{
    ApiControlState, ApiDataState, ApiMetrics, control_router, data_plane_router,
};
use octra_config::OctraConfig;
use octra_gateway_runtime::GatewayRuntime;
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
}

async fn gateway() -> Gateway {
    let storage = Arc::new(Storage::connect_in_memory().await.unwrap());
    let runtime = Arc::new(GatewayRuntime::bootstrap(storage.clone()).await.unwrap());
    let metrics = Arc::new(ApiMetrics::default());
    let data = ApiDataState::with_metrics(
        runtime.clone(),
        Arc::new(OctraConfig::default()),
        metrics.clone(),
    );
    let control = control_router(ApiControlState::with_metrics(
        storage.clone(),
        runtime.clone(),
        metrics,
    ));
    Gateway {
        router: data_plane_router(data),
        control,
        storage,
        runtime,
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
            require_auth: Some(require_auth),
            ..Default::default()
        })
        .await
        .unwrap();
    let upstream = storage
        .create_upstream(&CreateUpstream {
            id: Some(format!("{service_id}-up")),
            service_id: service.id.clone(),
            name: "primary".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    storage
        .create_target(&CreateTarget {
            id: Some(format!("{service_id}-tgt")),
            upstream_id: upstream.id,
            url: target_url.to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    storage
        .create_route(&CreateRoute {
            id: Some(format!("{service_id}-route")),
            service_id: service.id,
            path_prefix: Some(prefix.into()),
            strip_prefix: Some(strip),
            ..Default::default()
        })
        .await
        .unwrap();
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
            require_auth: Some(false),
            ..Default::default()
        })
        .await
        .unwrap();
    let upstream = gateway
        .storage
        .create_upstream(&CreateUpstream {
            id: Some("up".into()),
            service_id: service.id.clone(),
            name: "pool".into(),
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
                url: format!("http://{url}"),
                ..Default::default()
            })
            .await
            .unwrap();
    }
    gateway
        .storage
        .create_route(&CreateRoute {
            id: Some("r".into()),
            service_id: service.id,
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
async fn rejects_without_api_key_and_accepts_bearer_and_header() {
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
            name: "team".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let key = "ok_test-key-value";
    gateway
        .storage
        .create_credential(
            &CreateApiCredential {
                consumer_id: consumer.id,
                ..Default::default()
            },
            "ok_test-",
            &octra_gateway_runtime::hash_api_key(key),
        )
        .await
        .unwrap();
    gateway.reload().await;

    let (status, headers, _) = gateway.send(get("/")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(headers.get(header::WWW_AUTHENTICATE).unwrap(), "Bearer");

    let mut bearer = get("/");
    bearer.headers_mut().insert(
        header::AUTHORIZATION,
        header_default(&format!("Bearer {key}")),
    );
    assert_eq!(gateway.send(bearer).await.0, StatusCode::CREATED);

    let mut api_key = get("/");
    api_key
        .headers_mut()
        .insert("x-api-key", header_default(key));
    assert_eq!(gateway.send(api_key).await.0, StatusCode::CREATED);

    let mut wrong = get("/");
    wrong
        .headers_mut()
        .insert(header::AUTHORIZATION, header_default("Bearer nope"));
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
            require_auth: Some(false),
            ..Default::default()
        })
        .await
        .unwrap();
    gateway
        .storage
        .create_upstream(&CreateUpstream {
            id: Some("empty-up".into()),
            service_id: service.id.clone(),
            name: "empty".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    gateway
        .storage
        .create_route(&CreateRoute {
            id: Some("empty-route".into()),
            service_id: service.id,
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
            serde_json::json!({ "id": "svc", "name": "svc", "require_auth": false }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    // A service pointing to a missing service is a 404.
    let response = control()
        .oneshot(post(
            "/api/v1/routes",
            serde_json::json!({ "service_id": "missing", "path_prefix": "/" }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let response = control()
        .oneshot(post(
            "/api/v1/upstreams",
            serde_json::json!({ "id": "up", "service_id": "svc", "name": "up", "algorithm": "round_robin" }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    let response = control()
        .oneshot(post(
            "/api/v1/targets",
            serde_json::json!({ "upstream_id": "up", "url": format!("http://{addr}"), "weight": 1001 }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let response = control()
        .oneshot(post(
            "/api/v1/targets",
            serde_json::json!({ "upstream_id": "up", "url": "ftp://nope", }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let response = control()
        .oneshot(post(
            "/api/v1/targets",
            serde_json::json!({ "upstream_id": "up", "url": format!("http://{addr}") }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    let response = control()
        .oneshot(post(
            "/api/v1/routes",
            serde_json::json!({ "id": "r", "service_id": "svc", "path_prefix": "/", "methods": ["BREW"] }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let response = control()
        .oneshot(post(
            "/api/v1/routes",
            serde_json::json!({ "id": "r", "service_id": "svc", "path_prefix": "/" }),
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
            serde_json::json!({ "name": "bad", "connect_timeout_ms": 0 }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // Mutations reloaded the runtime snapshot.
    assert_eq!(runtime.snapshot().await.routes.len(), 1);

    let response = control()
        .oneshot(post(
            "/api/v1/consumers",
            serde_json::json!({ "id": "c", "name": "consumer" }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    let response = control()
        .oneshot(post(
            "/api/v1/credentials",
            serde_json::json!({ "consumer_id": "c", "name": "prod key" }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let issued: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let plaintext = issued["key"].as_str().unwrap();
    assert!(plaintext.starts_with("ok_"));
    let prefix = issued["credential"]["key_prefix"].as_str().unwrap();
    assert!(plaintext.starts_with(prefix));
    assert!(issued["credential"].get("key_hash").is_some());

    let stored = storage
        .find_credential_by_hash(&octra_gateway_runtime::hash_api_key(plaintext))
        .await
        .unwrap()
        .unwrap();
    assert!(stored.key_hash.as_deref() != Some(plaintext));

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
