//! End-to-end coverage for the `rate-limit` plugin.

use axum::http::{StatusCode, header};
use serde_json::json;

use crate::common::{gateway, get, seed_service, service_plugin, spawn_mock};

#[tokio::test]
async fn allows_the_burst_then_rejects_with_retry_after() {
    let (addr, _calls) = spawn_mock("rate").await;
    let gateway = gateway().await;
    seed_service(
        &gateway.storage,
        "svc-rate",
        &format!("http://{addr}"),
        false,
        "/rate",
        true,
    )
    .await;
    service_plugin(
        &gateway.storage,
        "svc-rate",
        "rate-limit",
        json!({"requests_per_second": 0.001, "burst": 1, "key": "global"}),
    )
    .await;
    gateway.reload().await;

    assert_eq!(
        gateway.send(get("/rate/items")).await.0,
        StatusCode::CREATED
    );

    let (status, headers, json) = gateway.send(get("/rate/items")).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(headers.contains_key(header::RETRY_AFTER));
    assert_eq!(json["error"]["code"], "rate_limited");
}
