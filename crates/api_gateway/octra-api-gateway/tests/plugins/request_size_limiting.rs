//! End-to-end coverage for the `request-size-limiting` plugin.

use axum::http::StatusCode;
use octra_api_plugin_types::Scope;
use serde_json::{Value, json};

use crate::common::{
    Gateway, MockBehaviour, MockUpstream, gateway, hit_count, plugin_instance, request,
    seed_simple_service, spawn_mock_advanced, with_body, with_header,
};

async fn fixture(config: Value) -> (Gateway, MockUpstream) {
    let upstream = spawn_mock_advanced(MockBehaviour::Status(201)).await;
    let gateway = gateway().await;
    let seeded = seed_simple_service(
        &gateway.storage,
        "svc-size",
        &format!("http://{}", upstream.addr),
        "/size",
    )
    .await;
    plugin_instance(
        &gateway.storage,
        "request-size-limiting",
        None,
        Scope::Service,
        Some(&seeded.service_id),
        &[],
        None,
        config,
    )
    .await;
    gateway.reload().await;
    (gateway, upstream)
}

fn post_with_length(length: &str, bytes: usize) -> axum::extract::Request {
    with_header(
        with_body(
            request("POST", "/size/items"),
            "application/octet-stream",
            vec![b'a'; bytes],
        ),
        "content-length",
        length,
    )
}

#[tokio::test]
async fn enforces_byte_limits_at_the_boundary() {
    let (gateway, upstream) = fixture(json!({
        "allowed_payload_size": 10,
        "size_unit": "bytes"
    }))
    .await;

    assert_eq!(
        gateway.send(post_with_length("10", 10)).await.0,
        StatusCode::CREATED
    );
    assert_eq!(hit_count(&upstream), 1);

    let (status, _, json) = gateway.send(post_with_length("11", 11)).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(json["message"], "Request size limit exceeded");
    assert_eq!(hit_count(&upstream), 1);
}

#[tokio::test]
async fn converts_kilobyte_and_megabyte_units() {
    let (gateway, upstream) = fixture(json!({
        "allowed_payload_size": 1,
        "size_unit": "kilobytes"
    }))
    .await;
    assert_eq!(
        gateway.send(post_with_length("1024", 1024)).await.0,
        StatusCode::CREATED
    );
    assert_eq!(
        gateway.send(post_with_length("1025", 8)).await.0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert_eq!(hit_count(&upstream), 1);

    let (gateway, upstream) = fixture(json!({"allowed_payload_size": 1})).await;
    assert_eq!(
        gateway.send(post_with_length("1048577", 8)).await.0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert_eq!(hit_count(&upstream), 0);
}

#[tokio::test]
async fn requires_content_length_when_configured() {
    let (gateway, upstream) = fixture(json!({
        "allowed_payload_size": 10,
        "size_unit": "bytes",
        "require_content_length": true
    }))
    .await;

    let missing = with_body(
        request("POST", "/size/items"),
        "application/octet-stream",
        vec![b'a'; 4],
    );
    let (status, _, json) = gateway.send(missing).await;
    assert_eq!(status, StatusCode::LENGTH_REQUIRED);
    assert_eq!(json["message"], "Content-Length required");

    let invalid = post_with_length("not-a-number", 4);
    assert_eq!(gateway.send(invalid).await.0, StatusCode::LENGTH_REQUIRED);

    assert_eq!(
        gateway.send(post_with_length("10", 10)).await.0,
        StatusCode::CREATED
    );
    assert_eq!(hit_count(&upstream), 1);
}
