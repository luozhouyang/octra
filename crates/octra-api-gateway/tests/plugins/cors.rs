//! End-to-end coverage for the `cors` plugin.

use axum::http::{StatusCode, header};
use serde_json::json;

use crate::common::{
    Gateway, gateway, get, header_default, request, seed_service, service_plugin, spawn_mock,
};

const ORIGIN: &str = "https://app.test";

async fn fixture(config: serde_json::Value) -> Gateway {
    let (addr, _calls) = spawn_mock("cors").await;
    let gateway = gateway().await;
    seed_service(
        &gateway.storage,
        "svc-cors",
        &format!("http://{addr}"),
        false,
        "/cors",
        true,
    )
    .await;
    service_plugin(&gateway.storage, "svc-cors", "cors", config).await;
    gateway.reload().await;
    gateway
}

fn preflight(origin: &str) -> axum::extract::Request {
    let mut request = request("OPTIONS", "/cors/items");
    request
        .headers_mut()
        .insert(header::ORIGIN, header_default(origin));
    request
        .headers_mut()
        .insert("access-control-request-method", header_default("GET"));
    request
}

#[tokio::test]
async fn answers_preflight_with_the_configured_policy() {
    let gateway = fixture(json!({
        "origins": [ORIGIN],
        "methods": ["GET", "POST"],
        "headers": ["x-token"],
        "max_age": 60
    }))
    .await;
    let (status, headers, _) = gateway.send(preflight(ORIGIN)).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get("access-control-allow-origin").unwrap(), ORIGIN);
    assert_eq!(
        headers.get("access-control-allow-methods").unwrap(),
        "GET,POST"
    );
    assert_eq!(
        headers.get("access-control-allow-headers").unwrap(),
        "x-token"
    );
    assert_eq!(headers.get("access-control-max-age").unwrap(), "60");
    assert_eq!(headers.get("vary").unwrap(), "Origin");
}

#[tokio::test]
async fn adds_headers_to_actual_cross_origin_responses() {
    let gateway = fixture(json!({"origins": [ORIGIN]})).await;
    let mut request = get("/cors/items");
    request
        .headers_mut()
        .insert(header::ORIGIN, header_default(ORIGIN));
    let (status, headers, _) = gateway.send(request).await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(headers.get("access-control-allow-origin").unwrap(), ORIGIN);
    assert!(
        headers
            .get(header::VARY)
            .unwrap()
            .to_str()
            .unwrap()
            .contains("Origin")
    );
}

#[tokio::test]
async fn supports_private_network_preflights() {
    let gateway = fixture(json!({"origins": [ORIGIN], "private_network": true})).await;
    let mut request = preflight(ORIGIN);
    request.headers_mut().insert(
        "access-control-request-private-network",
        header_default("true"),
    );
    let (status, headers, _) = gateway.send(request).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get("access-control-allow-private-network").unwrap(),
        "true"
    );
}

#[tokio::test]
async fn preflight_continue_defers_to_the_upstream() {
    let gateway = fixture(json!({"origins": [ORIGIN], "preflight_continue": true})).await;
    let (status, headers, _) = gateway.send(preflight(ORIGIN)).await;

    assert_eq!(status, StatusCode::CREATED);
    assert!(headers.get("access-control-allow-origin").is_none());
}

#[tokio::test]
async fn wildcard_credentials_echo_the_origin() {
    let gateway = fixture(json!({"origins": ["*"], "credentials": true})).await;
    let mut request = get("/cors/items");
    request
        .headers_mut()
        .insert(header::ORIGIN, header_default(ORIGIN));
    let (status, headers, _) = gateway.send(request).await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(headers.get("access-control-allow-origin").unwrap(), ORIGIN);
    assert_eq!(
        headers.get("access-control-allow-credentials").unwrap(),
        "true"
    );
}

#[tokio::test]
async fn wildcard_headers_apply_when_origin_is_absent() {
    let gateway = fixture(json!({"origins": ["*"]})).await;
    let (status, headers, _) = gateway.send(get("/cors/items")).await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(headers.get("access-control-allow-origin").unwrap(), "*");
}

#[tokio::test]
async fn matches_regex_origins() {
    let gateway = fixture(json!({"origins": ["https://.*\\.example\\.com"]})).await;
    let (status, headers, _) = gateway.send(preflight("https://app.example.com")).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get("access-control-allow-origin").unwrap(),
        "https://app.example.com"
    );
}
