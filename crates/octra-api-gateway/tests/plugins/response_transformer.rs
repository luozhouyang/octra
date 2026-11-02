//! End-to-end coverage for the `response-transformer` plugin.

use axum::http::StatusCode;
use serde_json::json;

use crate::common::{Gateway, gateway, get, seed_service, service_plugin, spawn_mock};

async fn fixture(config: serde_json::Value) -> Gateway {
    let (addr, _calls) = spawn_mock("response-transform").await;
    let gateway = gateway().await;
    seed_service(
        &gateway.storage,
        "svc-response-transform",
        &format!("http://{addr}"),
        false,
        "/response-transform",
        true,
    )
    .await;
    service_plugin(
        &gateway.storage,
        "svc-response-transform",
        "response-transformer",
        config,
    )
    .await;
    gateway.reload().await;
    gateway
}

#[tokio::test]
async fn transforms_response_headers_and_json_bodies() {
    let gateway = fixture(json!({
        "remove": {"headers": ["x-remove"], "json": ["connection"]},
        "rename": {"headers": ["x-old:x-new"], "json": ["server:upstream"]},
        "replace": {"headers": ["x-replace:replaced"], "json": ["path:/patched"]},
        "add": {
            "headers": ["x-add:added"],
            "json": ["added:1", "flag:true"],
            "json_types": ["number", "boolean"]
        },
        "append": {"headers": ["x-append:appended"], "json": ["query:extra"]}
    }))
    .await;

    let (status, headers, json) = gateway.send(get("/response-transform/items?limit=5")).await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(json["upstream"], "response-transform");
    assert!(json.get("server").is_none());
    assert!(json.get("connection").is_none());
    assert_eq!(json["path"], "/patched");
    assert_eq!(json["added"], 1);
    assert_eq!(json["flag"], true);
    assert_eq!(json["query"], json!(["limit=5", "extra"]));

    assert_eq!(headers.get("x-new").unwrap(), "old-value");
    assert!(headers.get("x-remove").is_none());
    assert!(headers.get("x-old").is_none());
    assert_eq!(headers.get("x-replace").unwrap(), "replaced");
    assert_eq!(headers.get("x-add").unwrap(), "added");
    assert_eq!(headers.get("x-append").unwrap(), "appended");
}
