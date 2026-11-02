//! End-to-end coverage for the `rate-limiting` plugin.

use axum::http::{StatusCode, header};
use serde_json::json;

use crate::common::{Gateway, gateway, get, seed_service, service_plugin, spawn_mock};

async fn fixture(config: serde_json::Value) -> Gateway {
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
    service_plugin(&gateway.storage, "svc-rate", "rate-limiting", config).await;
    gateway.reload().await;
    gateway
}

#[tokio::test]
async fn enforces_windows_and_reports_kong_headers() {
    let gateway = fixture(json!({"day": 1, "limit_by": "ip"})).await;

    let (status, headers, _) = gateway.send(get("/rate/items")).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(headers.get("X-RateLimit-Limit-Day").unwrap(), "1");
    assert_eq!(headers.get("X-RateLimit-Remaining-Day").unwrap(), "0");
    assert_eq!(headers.get("RateLimit-Limit").unwrap(), "1");
    assert_eq!(headers.get("RateLimit-Remaining").unwrap(), "0");
    assert!(headers.contains_key("RateLimit-Reset"));

    let (status, headers, json) = gateway.send(get("/rate/items")).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(headers.get("RateLimit-Limit").unwrap(), "1");
    assert!(headers.contains_key(header::RETRY_AFTER));
    assert_eq!(json["message"], "API rate limit exceeded");
}

#[tokio::test]
async fn supports_custom_error_and_hidden_headers() {
    let gateway = fixture(json!({
        "day": 1,
        "limit_by": "ip",
        "hide_client_headers": true,
        "error_code": 418,
        "error_message": "slow down"
    }))
    .await;

    let (status, headers, _) = gateway.send(get("/rate/items")).await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(headers.get("X-RateLimit-Limit-Day").is_none());

    let (status, headers, json) = gateway.send(get("/rate/items")).await;
    assert_eq!(status, StatusCode::IM_A_TEAPOT);
    assert!(headers.get("X-RateLimit-Limit-Day").is_none());
    assert_eq!(json["message"], "slow down");
}
