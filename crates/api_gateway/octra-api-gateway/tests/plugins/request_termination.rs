//! End-to-end coverage for the `request-termination` plugin.

use axum::http::{StatusCode, header};
use octra_api_plugin_types::Scope;
use serde_json::{Value, json};

use crate::common::{
    Gateway, MockBehaviour, MockUpstream, gateway, get, hit_count, plugin_instance,
    seed_simple_service, spawn_mock_advanced, with_header, with_query,
};

async fn fixture(config: Value) -> (Gateway, MockUpstream) {
    let upstream = spawn_mock_advanced(MockBehaviour::Status(201)).await;
    let gateway = gateway().await;
    let seeded = seed_simple_service(
        &gateway.storage,
        "svc-termination",
        &format!("http://{}", upstream.addr),
        "/terminate",
    )
    .await;
    plugin_instance(
        &gateway.storage,
        "request-termination",
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

#[tokio::test]
async fn terminates_with_the_default_response() {
    let (gateway, upstream) = fixture(json!({})).await;

    let (status, headers, json) = gateway.send(get("/terminate/items")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        headers.get(header::CONTENT_TYPE).unwrap(),
        "application/json"
    );
    assert_eq!(json["message"], "Service unavailable");
    assert_eq!(hit_count(&upstream), 0);
}

#[tokio::test]
async fn supports_custom_status_body_and_content_type() {
    let (gateway, upstream) = fixture(json!({
        "status_code": 418,
        "body": "teapot",
        "content_type": "text/plain"
    }))
    .await;

    let response = gateway.send_raw(get("/terminate/items")).await;
    assert_eq!(response.status, StatusCode::IM_A_TEAPOT);
    assert_eq!(
        response.headers.get(header::CONTENT_TYPE).unwrap(),
        "text/plain"
    );
    assert_eq!(response.body, b"teapot");
    assert_eq!(hit_count(&upstream), 0);
}

#[tokio::test]
async fn echo_forces_a_json_content_type() {
    let (gateway, _upstream) = fixture(json!({
        "status_code": 201,
        "body": "created",
        "content_type": "text/plain",
        "echo": true
    }))
    .await;

    let response = gateway.send_raw(get("/terminate/items")).await;
    assert_eq!(response.status, StatusCode::CREATED);
    assert_eq!(
        response.headers.get(header::CONTENT_TYPE).unwrap(),
        "application/json"
    );
    assert_eq!(response.body, b"created");
}

#[tokio::test]
async fn a_trigger_header_gates_termination() {
    let (gateway, upstream) = fixture(json!({"trigger": "x-kill"})).await;

    assert_eq!(
        gateway.send(get("/terminate/items")).await.0,
        StatusCode::CREATED
    );
    assert_eq!(hit_count(&upstream), 1);

    let request = with_header(get("/terminate/items"), "x-kill", "1");
    assert_eq!(
        gateway.send(request).await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(hit_count(&upstream), 1);
}

#[tokio::test]
async fn a_trigger_query_parameter_also_terminates() {
    let (gateway, upstream) = fixture(json!({"trigger": "x-kill"})).await;

    let request = with_query(get("/terminate/items"), "x-kill=1");
    assert_eq!(
        gateway.send(request).await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(hit_count(&upstream), 0);
}
