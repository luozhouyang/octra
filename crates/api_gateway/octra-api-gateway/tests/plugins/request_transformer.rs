//! End-to-end coverage for the `request-transformer` plugin.

use axum::body::Body;
use axum::http::{StatusCode, header};
use serde_json::json;

use crate::common::{
    Gateway, gateway, header_default, request, seed_service, service_plugin, spawn_mock,
};

async fn fixture(config: serde_json::Value) -> Gateway {
    let (addr, _calls) = spawn_mock("transform").await;
    let gateway = gateway().await;
    seed_service(
        &gateway.storage,
        "svc-request-transform",
        &format!("http://{addr}"),
        false,
        "/transform",
        true,
    )
    .await;
    service_plugin(
        &gateway.storage,
        "svc-request-transform",
        "request-transformer",
        config,
    )
    .await;
    gateway.reload().await;
    gateway
}

fn query_pairs(query: &str) -> Vec<(String, String)> {
    url::form_urlencoded::parse(query.as_bytes())
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect()
}

#[tokio::test]
async fn transforms_headers_query_method_and_uri() {
    let gateway = fixture(json!({
        "http_method": "POST",
        "remove": {"headers": ["x-drop"], "querystring": ["drop"]},
        "rename": {"headers": ["x-old:x-new"], "querystring": ["old:new"]},
        "replace": {
            "headers": ["x-replace:replaced"],
            "querystring": ["replace:replaced"],
            "uri": "/rewritten?drop=d&old=oldvalue&replace=old&append=first"
        },
        "add": {"headers": ["x-add:added"], "querystring": ["add:added"]},
        "append": {"headers": ["x-appended:appended"], "querystring": ["append:second"]}
    }))
    .await;

    let mut request = request("GET", "/transform/items");
    request
        .headers_mut()
        .insert("x-drop", header_default("gone"));
    request
        .headers_mut()
        .insert("x-old", header_default("old-value"));
    request
        .headers_mut()
        .insert("x-replace", header_default("old"));
    let (status, _, json) = gateway.send(request).await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(json["method"], "POST");
    assert_eq!(json["path"], "/rewritten");

    let pairs = query_pairs(json["query"].as_str().unwrap());
    assert_eq!(pairs.iter().filter(|(key, _)| key == "drop").count(), 0);
    assert!(pairs.contains(&("new".into(), "oldvalue".into())));
    assert!(pairs.contains(&("replace".into(), "replaced".into())));
    assert!(pairs.contains(&("add".into(), "added".into())));
    assert!(pairs.contains(&("append".into(), "first".into())));
    assert!(pairs.contains(&("append".into(), "second".into())));

    let headers = &json["headers"];
    assert!(headers.get("x-drop").is_none());
    assert_eq!(headers["x-new"], "old-value");
    assert_eq!(headers["x-replace"], "replaced");
    assert_eq!(headers["x-add"], "added");
    assert_eq!(headers["x-appended"], "appended");
}

#[tokio::test]
async fn transforms_json_bodies() {
    let gateway = fixture(json!({
        "remove": {"body": ["drop"]},
        "rename": {"body": ["old:new"]},
        "replace": {"body": ["replace:replaced"]},
        "add": {"body": ["add:added"]},
        "append": {"body": ["list:third"]}
    }))
    .await;

    let mut request = request("POST", "/transform/items");
    request
        .headers_mut()
        .insert(header::CONTENT_TYPE, header_default("application/json"));
    *request.body_mut() = Body::from(
        json!({"drop": 1, "old": "x", "replace": "old", "list": ["a", "b"]}).to_string(),
    );
    let (status, _, json) = gateway.send(request).await;
    assert_eq!(status, StatusCode::CREATED);

    let body: serde_json::Value = serde_json::from_str(json["body"].as_str().unwrap()).unwrap();
    assert!(body.get("drop").is_none());
    assert_eq!(body["new"], "x");
    assert_eq!(body["replace"], "replaced");
    assert_eq!(body["add"], "added");
    assert_eq!(body["list"], json!(["a", "b", "third"]));
}

#[tokio::test]
async fn transforms_multipart_bodies() {
    let gateway = fixture(json!({
        "remove": {"body": ["drop"]},
        "rename": {"body": ["old:new"]},
        "replace": {"body": ["replace:replaced"]},
        "add": {"body": ["add:added"]}
    }))
    .await;

    let mut request = request("POST", "/transform/items");
    request.headers_mut().insert(
        header::CONTENT_TYPE,
        header_default("multipart/form-data; boundary=X-BOUNDARY"),
    );
    *request.body_mut() = Body::from(
        "--X-BOUNDARY\r\nContent-Disposition: form-data; name=\"drop\"\r\n\r\n1\r\n\
         --X-BOUNDARY\r\nContent-Disposition: form-data; name=\"old\"\r\n\r\nx\r\n\
         --X-BOUNDARY\r\nContent-Disposition: form-data; name=\"replace\"\r\n\r\nold\r\n\
         --X-BOUNDARY--\r\n",
    );
    let (status, _, json) = gateway.send(request).await;
    assert_eq!(status, StatusCode::CREATED);

    let body = json["body"].as_str().unwrap();
    assert!(!body.contains("name=\"drop\""));
    assert!(body.contains("name=\"new\""));
    assert!(body.contains("name=\"add\""));
    assert!(body.contains("replaced"));
    assert!(body.contains("--X-BOUNDARY--"));
}

#[tokio::test]
async fn merges_form_body_into_query_for_bodyless_methods() {
    let gateway = fixture(json!({
        "http_method": "GET",
        "remove": {"querystring": ["drop"]},
        "add": {"querystring": ["added:1"]}
    }))
    .await;

    let mut request = request("POST", "/transform/items");
    request.headers_mut().insert(
        header::CONTENT_TYPE,
        header_default("application/x-www-form-urlencoded"),
    );
    *request.body_mut() = Body::from("keep=1&drop=2");
    let (status, _, json) = gateway.send(request).await;
    assert_eq!(status, StatusCode::CREATED);

    assert_eq!(json["method"], "GET");
    let pairs = query_pairs(json["query"].as_str().unwrap());
    assert!(pairs.contains(&("keep".into(), "1".into())));
    assert_eq!(pairs.iter().filter(|(key, _)| key == "drop").count(), 0);
    assert!(pairs.contains(&("added".into(), "1".into())));
}

#[tokio::test]
async fn transforms_form_urlencoded_bodies() {
    let gateway = fixture(json!({
        "remove": {"body": ["drop"]},
        "replace": {"body": ["replace:replaced"]},
        "add": {"body": ["add:added"]}
    }))
    .await;

    let mut request = request("POST", "/transform/items");
    request.headers_mut().insert(
        header::CONTENT_TYPE,
        header_default("application/x-www-form-urlencoded"),
    );
    *request.body_mut() = Body::from("drop=1&replace=old&keep=1");
    let (status, _, json) = gateway.send(request).await;
    assert_eq!(status, StatusCode::CREATED);

    let pairs = query_pairs(json["body"].as_str().unwrap());
    assert_eq!(pairs.iter().filter(|(key, _)| key == "drop").count(), 0);
    assert!(pairs.contains(&("replace".into(), "replaced".into())));
    assert!(pairs.contains(&("add".into(), "added".into())));
    assert!(pairs.contains(&("keep".into(), "1".into())));
}
