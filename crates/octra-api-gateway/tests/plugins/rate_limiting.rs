//! End-to-end coverage for the `rate-limiting` plugin.

use axum::http::{StatusCode, header};
use octra_api_core::{CreateConsumer, CreateKeyauthCredential};
use serde_json::json;

use crate::common::{Gateway, gateway, get, seed_service, service_plugin, spawn_mock, with_header};

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

#[tokio::test]
async fn limits_by_header_value() {
    let gateway = fixture(json!({
        "day": 1,
        "limit_by": "header",
        "header_name": "x-consumer"
    }))
    .await;

    let first = with_header(get("/rate/items"), "x-consumer", "a");
    assert_eq!(gateway.send(first).await.0, StatusCode::CREATED);

    let repeated = with_header(get("/rate/items"), "x-consumer", "a");
    assert_eq!(
        gateway.send(repeated).await.0,
        StatusCode::TOO_MANY_REQUESTS
    );

    let other = with_header(get("/rate/items"), "x-consumer", "b");
    assert_eq!(gateway.send(other).await.0, StatusCode::CREATED);
}

#[tokio::test]
async fn limits_by_path() {
    let gateway = fixture(json!({
        "day": 1,
        "limit_by": "path",
        "path": "/rate/items"
    }))
    .await;

    assert_eq!(
        gateway.send(get("/rate/items")).await.0,
        StatusCode::CREATED
    );
    assert_eq!(
        gateway.send(get("/rate/items")).await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        gateway.send(get("/rate/other")).await.0,
        StatusCode::CREATED
    );
}

#[tokio::test]
async fn reports_all_configured_windows() {
    let gateway = fixture(json!({"day": 1, "hour": 1, "limit_by": "ip"})).await;

    let (status, headers, _) = gateway.send(get("/rate/items")).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(headers.get("X-RateLimit-Limit-Day").unwrap(), "1");
    assert_eq!(headers.get("X-RateLimit-Limit-Hour").unwrap(), "1");

    let (status, headers, json) = gateway.send(get("/rate/items")).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(headers.get("X-RateLimit-Limit-Day").unwrap(), "1");
    assert_eq!(headers.get("X-RateLimit-Limit-Hour").unwrap(), "1");
    assert!(headers.contains_key(header::RETRY_AFTER));
    assert_eq!(json["message"], "API rate limit exceeded");
}

#[tokio::test]
async fn consumer_limits_share_the_client_ip_in_the_pre_auth_chain() {
    // Non-deferred rate-limiting instances run before the consumer context is
    // attached, so `limit_by: consumer` currently keys on the client IP.
    let (addr, _calls) = spawn_mock("rate-consumer").await;
    let gateway = gateway().await;
    seed_service(
        &gateway.storage,
        "svc-rate-consumer",
        &format!("http://{addr}"),
        false,
        "/rate-consumer",
        true,
    )
    .await;
    service_plugin(
        &gateway.storage,
        "svc-rate-consumer",
        "key-auth",
        json!({"key_names": ["apikey"]}),
    )
    .await;
    service_plugin(
        &gateway.storage,
        "svc-rate-consumer",
        "rate-limiting",
        json!({"day": 1, "limit_by": "consumer"}),
    )
    .await;

    for (username, key) in [("first", "first-key"), ("second", "second-key")] {
        let consumer = gateway
            .storage
            .api
            .create_consumer(&CreateConsumer {
                username: Some(username.into()),
                ..Default::default()
            })
            .await
            .unwrap();
        gateway
            .storage
            .api
            .create_credential(&CreateKeyauthCredential {
                consumer_id: consumer.id,
                key: Some(key.into()),
                ..Default::default()
            })
            .await
            .unwrap();
    }
    gateway.reload().await;

    let first = with_header(get("/rate-consumer/items"), "apikey", "first-key");
    assert_eq!(gateway.send(first).await.0, StatusCode::CREATED);

    let second = with_header(get("/rate-consumer/items"), "apikey", "second-key");
    assert_eq!(gateway.send(second).await.0, StatusCode::TOO_MANY_REQUESTS);
}
