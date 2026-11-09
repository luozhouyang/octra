//! Integration coverage for the in-process LLM upstream.

mod common;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{StatusCode, header};
use axum::response::Response;
use common::{PEER, gateway_with_internal, post_json, request};
use octra_api_core::{CreatePluginInstance, CreateRoute, CreateService};
use octra_api_gateway::{InternalUpstream, InternalUpstreamContext};
use serde_json::json;

#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path: String,
    request_id: Option<String>,
    client_ip: Option<std::net::IpAddr>,
    body: String,
}

#[derive(Clone)]
struct FakeInternal {
    seen: Arc<Mutex<Vec<Seen>>>,
}

#[async_trait]
impl InternalUpstream for FakeInternal {
    async fn call(&self, request: Request, _context: InternalUpstreamContext) -> Response {
        let method = request.method().to_string();
        let path = request.uri().path().to_string();
        let client_ip = request
            .extensions()
            .get::<octra_data_plane::ClientIp>()
            .and_then(|client| client.0);
        let request_id = request
            .extensions()
            .get::<octra_data_plane::RequestId>()
            .map(|id| id.0.clone());
        let body = axum::body::to_bytes(request.into_body(), 1024 * 1024)
            .await
            .unwrap_or_default();
        self.seen.lock().unwrap().push(Seen {
            method,
            path: path.clone(),
            request_id: request_id.clone(),
            client_ip,
            body: String::from_utf8_lossy(&body).to_string(),
        });

        if path.ends_with("/error") {
            return Response::builder()
                .status(StatusCode::UNAUTHORIZED)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"error":{"message":"nope"}}"#))
                .unwrap();
        }
        if path.ends_with("/stream") {
            return Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "text/event-stream")
                .body(Body::from("data: one\n\ndata: two\n\n"))
                .unwrap();
        }

        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json")
            .header(
                "x-internal-request-id",
                request_id.unwrap_or_else(|| "missing".to_string()),
            )
            .body(Body::from(r#"{"ok":true}"#))
            .unwrap()
    }
}

async fn setup() -> (common::Gateway, FakeInternal) {
    let fake = FakeInternal {
        seen: Arc::new(Mutex::new(Vec::new())),
    };
    let gateway = gateway_with_internal(Arc::new(fake.clone())).await;
    let storage = &gateway.storage;

    storage
        .api
        .create_service(&CreateService {
            id: Some("svc_llm".into()),
            name: "llm-gateway".into(),
            protocol: Some("http".into()),
            host: Some("internal".into()),
            port: Some(80),
            tags: Some(vec!["llm:internal".into()]),
            ..Default::default()
        })
        .await
        .unwrap();
    storage
        .api
        .create_route(&CreateRoute {
            id: Some("rt_llm".into()),
            service_id: Some("svc_llm".into()),
            paths: Some(vec!["/v1".into()]),
            methods: Some(Vec::new()),
            strip_path: Some(false),
            protocols: Some(vec!["http".into(), "https".into()]),
            ..Default::default()
        })
        .await
        .unwrap();
    storage
        .api
        .create_plugin_instance(&CreatePluginInstance {
            name: "cors".into(),
            service_id: Some("svc_llm".into()),
            config: Some(json!({"origins": ["https://app.example.com"]})),
            ..Default::default()
        })
        .await
        .unwrap();
    // A global plugin that would reject the request if skip-global were not
    // applied for the internal service.
    storage
        .api
        .create_plugin_instance(&CreatePluginInstance {
            name: "ip-restriction".into(),
            config: Some(json!({"deny": ["127.0.0.1"]})),
            ..Default::default()
        })
        .await
        .unwrap();
    gateway.reload().await;
    (gateway, fake)
}

fn cors_post(uri: &str, body: serde_json::Value) -> axum::extract::Request {
    let mut request = post_json(uri, body);
    request
        .headers_mut()
        .insert(header::ORIGIN, "https://app.example.com".parse().unwrap());
    request
}

#[tokio::test]
async fn cors_preflight_short_circuits_without_calling_the_internal_upstream() {
    let (gateway, fake) = setup().await;
    let mut preflight = request("OPTIONS", "/v1/chat/completions");
    preflight
        .headers_mut()
        .insert(header::ORIGIN, "https://app.example.com".parse().unwrap());
    preflight
        .headers_mut()
        .insert("access-control-request-method", "POST".parse().unwrap());

    let (status, headers, _) = gateway.send(preflight).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .and_then(|value| value.to_str().ok()),
        Some("https://app.example.com")
    );
    assert!(fake.seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn internal_service_skips_global_plugins_and_forwards_identity() {
    let (gateway, fake) = setup().await;
    let response = gateway
        .send(cors_post(
            "/v1/chat/completions",
            json!({"model": "gpt-4o", "messages": []}),
        ))
        .await;

    assert_eq!(response.0, StatusCode::OK);
    assert_eq!(
        response
            .1
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .and_then(|value| value.to_str().ok()),
        Some("https://app.example.com")
    );
    let request_id = response
        .1
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .unwrap()
        .to_string();
    assert_eq!(
        response
            .1
            .get("x-internal-request-id")
            .and_then(|value| value.to_str().ok()),
        Some(request_id.as_str())
    );

    let seen = fake.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].method, "POST");
    assert_eq!(seen[0].path, "/v1/chat/completions");
    assert_eq!(
        seen[0].client_ip,
        Some(PEER.parse::<std::net::SocketAddr>().unwrap().ip())
    );
    assert_eq!(seen[0].request_id.as_deref(), Some(request_id.as_str()));
    assert!(seen[0].body.contains("\"gpt-4o\""));
}

#[tokio::test]
async fn internal_error_responses_keep_cors_headers() {
    let (gateway, _) = setup().await;
    let response = gateway
        .send(cors_post("/v1/error", json!({"model": "gpt-4o"})))
        .await;
    assert_eq!(response.0, StatusCode::UNAUTHORIZED);
    assert_eq!(
        response
            .1
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .and_then(|value| value.to_str().ok()),
        Some("https://app.example.com")
    );
}

#[tokio::test]
async fn internal_stream_is_passed_through() {
    let (gateway, _) = setup().await;
    let response = gateway
        .send_raw(post_json("/v1/stream", json!({"model": "gpt-4o"})))
        .await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(
        response
            .headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("text/event-stream")
    );
    assert_eq!(
        String::from_utf8_lossy(&response.body),
        "data: one\n\ndata: two\n\n"
    );
}
