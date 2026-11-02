//! End-to-end coverage for the `ip-restriction` plugin.
//!
//! The harness attaches `ConnectInfo(127.0.0.1:4242)` to every request.

use axum::http::StatusCode;
use serde_json::json;

use crate::common::{Gateway, gateway, get, seed_service, service_plugin, spawn_mock};

async fn fixture(config: serde_json::Value) -> Gateway {
    let (addr, _calls) = spawn_mock("ip").await;
    let gateway = gateway().await;
    seed_service(
        &gateway.storage,
        "svc-ip",
        &format!("http://{addr}"),
        false,
        "/ip",
        true,
    )
    .await;
    service_plugin(&gateway.storage, "svc-ip", "ip-restriction", config).await;
    gateway.reload().await;
    gateway
}

#[tokio::test]
async fn allows_an_ip_inside_an_allowlist() {
    let gateway = fixture(json!({"allow": ["127.0.0.0/8"]})).await;
    assert_eq!(gateway.send(get("/ip/items")).await.0, StatusCode::CREATED);
}

#[tokio::test]
async fn rejects_an_ip_inside_a_denylist() {
    let gateway = fixture(json!({"deny": ["127.0.0.0/8"], "message": "blocked"})).await;
    let (status, _, json) = gateway.send(get("/ip/items")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(json["message"], "blocked");
}

#[tokio::test]
async fn rejects_an_ip_outside_an_allowlist_with_a_custom_status() {
    let gateway = fixture(json!({"allow": ["10.0.0.0/8"], "status": 418})).await;
    assert_eq!(
        gateway.send(get("/ip/items")).await.0,
        StatusCode::IM_A_TEAPOT
    );
}
