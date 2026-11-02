//! End-to-end coverage for the `correlation-id` plugin.

use axum::http::{StatusCode, header};
use serde_json::json;

use crate::common::{
    Gateway, gateway, get, header_default, seed_service, service_plugin, spawn_mock,
};

async fn fixture(config: serde_json::Value, require_auth: bool) -> Gateway {
    let (addr, _calls) = spawn_mock("correlation").await;
    let gateway = gateway().await;
    seed_service(
        &gateway.storage,
        "svc-correlation",
        &format!("http://{addr}"),
        require_auth,
        "/correlation",
        true,
    )
    .await;
    service_plugin(
        &gateway.storage,
        "svc-correlation",
        "correlation-id",
        config,
    )
    .await;
    gateway.reload().await;
    gateway
}

#[tokio::test]
async fn generates_and_forwards_the_default_header() {
    let gateway = fixture(json!({}), false).await;
    let (status, headers, json) = gateway.send(get("/correlation/items")).await;

    assert_eq!(status, StatusCode::CREATED);
    let generated = json["kong_request_id"].as_str().unwrap();
    assert!(!generated.is_empty());
    // The default generator is `uuid#counter`.
    assert!(generated.contains('#'));
    assert!(!headers.contains_key("Kong-Request-ID"));
}

#[tokio::test]
async fn reuses_an_inbound_correlation_id() {
    let gateway = fixture(json!({}), false).await;
    let mut request = get("/correlation/items");
    request
        .headers_mut()
        .insert("Kong-Request-ID", header_default("client-correlation"));
    let (status, _, json) = gateway.send(request).await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(json["kong_request_id"], "client-correlation");
}

#[tokio::test]
async fn echoes_the_header_on_successful_responses() {
    let gateway = fixture(json!({"echo_downstream": true}), false).await;

    let mut request = get("/correlation/items");
    request
        .headers_mut()
        .insert("Kong-Request-ID", header_default("echoed-id"));
    let (status, headers, _) = gateway.send(request).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(headers.get("Kong-Request-ID").unwrap(), "echoed-id");
}

#[tokio::test]
async fn echoes_the_header_on_auth_short_circuits() {
    let gateway = fixture(json!({"echo_downstream": true}), true).await;

    // Auth fails, but the response phase still echoes the correlation ID.
    let (status, headers, _) = gateway.send(get("/correlation/items")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(headers.get("Kong-Request-ID").is_some());
}

#[tokio::test]
async fn supports_uuid_generator_and_custom_header_name() {
    let gateway = fixture(
        json!({"generator": "uuid", "header_name": "X-Correlation-ID"}),
        false,
    )
    .await;
    let (status, _, json) = gateway.send(get("/correlation/items")).await;

    assert_eq!(status, StatusCode::CREATED);
    let generated = json["x_correlation_id"].as_str().unwrap();
    assert!(!generated.contains('#'));
    assert_eq!(generated.matches('-').count(), 4);
    assert_eq!(json["kong_request_id"], "");
}

#[tokio::test]
async fn tracker_generator_matches_the_gateway_request_id() {
    let gateway = fixture(json!({"generator": "tracker"}), false).await;
    let (status, _, json) = gateway.send(get("/correlation/items")).await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(json["kong_request_id"], json["request_id"]);
}
