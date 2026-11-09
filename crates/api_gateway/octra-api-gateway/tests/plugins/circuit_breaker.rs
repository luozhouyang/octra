//! End-to-end coverage for the `circuit-breaker` plugin.

use axum::http::{StatusCode, header};
use octra_api_plugin_types::Scope;
use serde_json::json;

use crate::common::{
    Gateway, MockBehaviour, MockUpstream, gateway, get, hit_count, plugin_instance,
    seed_service_with_routes, spawn_mock_advanced,
};

fn config() -> serde_json::Value {
    json!({"failure_threshold": 2, "open_seconds": 60})
}

async fn failing_fixture() -> (Gateway, MockUpstream) {
    let upstream = spawn_mock_advanced(MockBehaviour::Status(500)).await;
    let gateway = gateway().await;
    let seeded = seed_service_with_routes(
        &gateway.storage,
        "svc-breaker",
        &format!("http://{}", upstream.addr),
        &[("/breaker", true)],
    )
    .await;
    plugin_instance(
        &gateway.storage,
        "circuit-breaker",
        None,
        Scope::Service,
        Some(&seeded.service_id),
        &[],
        None,
        config(),
    )
    .await;
    gateway.reload().await;
    (gateway, upstream)
}

#[tokio::test]
async fn opens_after_the_failure_threshold_and_stops_calling_upstream() {
    let (gateway, upstream) = failing_fixture().await;

    assert_eq!(
        gateway.send(get("/breaker/items")).await.0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(
        gateway.send(get("/breaker/items")).await.0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(hit_count(&upstream), 2);

    let (status, headers, json) = gateway.send(get("/breaker/items")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(json["error"]["code"], "circuit_open");
    let retry_after: u64 = headers
        .get(header::RETRY_AFTER)
        .expect("retry-after")
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((1..=60).contains(&retry_after));
    assert_eq!(hit_count(&upstream), 2);
}

#[tokio::test]
async fn keeps_breakers_isolated_per_service() {
    let failing = spawn_mock_advanced(MockBehaviour::Status(500)).await;
    let healthy = spawn_mock_advanced(MockBehaviour::Status(201)).await;
    let gateway = gateway().await;
    let failing_service = seed_service_with_routes(
        &gateway.storage,
        "svc-breaker-fail",
        &format!("http://{}", failing.addr),
        &[("/breaker-fail", true)],
    )
    .await;
    seed_service_with_routes(
        &gateway.storage,
        "svc-breaker-ok",
        &format!("http://{}", healthy.addr),
        &[("/breaker-ok", true)],
    )
    .await;
    plugin_instance(
        &gateway.storage,
        "circuit-breaker",
        None,
        Scope::Global,
        None,
        &[],
        None,
        json!({"failure_threshold": 1, "open_seconds": 60}),
    )
    .await;
    gateway.reload().await;

    assert_eq!(
        gateway.send(get("/breaker-fail/items")).await.0,
        StatusCode::INTERNAL_SERVER_ERROR,
        "service {} opens the circuit",
        failing_service.service_id
    );
    assert_eq!(
        gateway.send(get("/breaker-fail/items")).await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(hit_count(&failing), 1);

    assert_eq!(
        gateway.send(get("/breaker-ok/items")).await.0,
        StatusCode::CREATED
    );
    assert_eq!(hit_count(&healthy), 1);
}
