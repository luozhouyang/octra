//! End-to-end coverage for the `header-rewrite` plugin.

use axum::http::StatusCode;
use octra_api_plugin_types::Scope;
use serde_json::{Value, json};

use crate::common::{
    Gateway, gateway, get, plugin_instance, seed_simple_service, spawn_mock, with_header,
};

async fn fixture(config: Value) -> Gateway {
    let (addr, _calls) = spawn_mock("header-rewrite").await;
    let gateway = gateway().await;
    let seeded = seed_simple_service(
        &gateway.storage,
        "svc-header-rewrite",
        &format!("http://{addr}"),
        "/headers",
    )
    .await;
    plugin_instance(
        &gateway.storage,
        "header-rewrite",
        None,
        Scope::Service,
        Some(&seeded.service_id),
        &[],
        None,
        config,
    )
    .await;
    gateway.reload().await;
    gateway
}

fn value(value: &str) -> axum::http::HeaderValue {
    axum::http::HeaderValue::from_str(value).unwrap()
}

#[tokio::test]
async fn rewrites_request_and_response_headers() {
    let gateway = fixture(json!({
        "request": {
            "set": { "x-set": "one" },
            "add": { "x-add": ["a", "b"] },
            "remove": ["x-drop"]
        },
        "response": {
            "set": { "x-set-response": "one" },
            "add": { "x-add-response": ["a", "b"] },
            "remove": ["x-old"]
        }
    }))
    .await;

    let request = with_header(get("/headers/items"), "x-drop", "gone");
    let request = with_header(request, "x-plugin", "custom");
    let (status, headers, json) = gateway.send(request).await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(json["headers"]["x-set"], "one");
    // `add` appends, and the mock records the last value for duplicate headers.
    assert_eq!(json["headers"]["x-add"], "b");
    assert!(json["headers"].get("x-drop").is_none());
    assert_eq!(json["headers"]["x-plugin"], "custom");

    assert_eq!(headers.get("x-set-response").unwrap(), value("one"));
    assert_eq!(headers.get_all("x-add-response").iter().count(), 2);
    assert!(headers.get("x-old").is_none());
    assert_eq!(headers.get("x-upstream").unwrap(), value("header-rewrite"));
}

#[tokio::test]
async fn an_empty_config_leaves_headers_untouched() {
    let gateway = fixture(json!({})).await;

    let request = with_header(get("/headers/items"), "x-plugin", "custom");
    let (status, headers, json) = gateway.send(request).await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(json["headers"]["x-plugin"], "custom");
    assert!(json["headers"].get("x-set").is_none());
    assert_eq!(headers.get("x-old").unwrap(), value("old-value"));
    assert!(headers.get("x-set-response").is_none());
}
