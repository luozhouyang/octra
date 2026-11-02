//! End-to-end coverage for the `cors` plugin.

use axum::http::{StatusCode, header};
use serde_json::json;

use crate::common::{
    Gateway, gateway, get, header_default, request, seed_service, service_plugin, spawn_mock,
};

const ORIGIN: &str = "https://app.test";

async fn fixture() -> Gateway {
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
    service_plugin(
        &gateway.storage,
        "svc-cors",
        "cors",
        json!({
            "allow_origins": [ORIGIN],
            "allow_methods": ["GET", "POST"],
            "allow_headers": ["x-token"],
            "max_age": 60
        }),
    )
    .await;
    gateway.reload().await;
    gateway
}

fn preflight() -> axum::extract::Request {
    let mut request = request("OPTIONS", "/cors/items");
    request
        .headers_mut()
        .insert(header::ORIGIN, header_default(ORIGIN));
    request
        .headers_mut()
        .insert("access-control-request-method", header_default("GET"));
    request
}

#[tokio::test]
async fn answers_preflight_with_the_configured_policy() {
    let gateway = fixture().await;
    let (status, headers, _) = gateway.send(preflight()).await;

    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(headers.get("access-control-allow-origin").unwrap(), ORIGIN);
    assert_eq!(
        headers.get("access-control-allow-methods").unwrap(),
        "GET, POST"
    );
    assert_eq!(
        headers.get("access-control-allow-headers").unwrap(),
        "x-token"
    );
    assert_eq!(headers.get("access-control-max-age").unwrap(), "60");
}

#[tokio::test]
async fn adds_headers_to_actual_cross_origin_responses() {
    let gateway = fixture().await;
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
            .contains("origin")
    );
}
