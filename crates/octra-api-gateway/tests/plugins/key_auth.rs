//! End-to-end coverage for the `key-auth` plugin.

use axum::body::Body;
use axum::http::{StatusCode, header};
use octra_api_core::{CreateConsumer, CreateKeyauthCredential};
use serde_json::json;

use crate::common::{
    Gateway, gateway, get, header_default, request, seed_service, service_plugin, spawn_mock,
};

async fn fixture() -> Gateway {
    fixture_with(json!({"key_names": ["apikey"], "hide_credentials": true})).await
}

async fn fixture_with(config: serde_json::Value) -> Gateway {
    let (addr, _calls) = spawn_mock("key").await;
    let gateway = gateway().await;
    seed_service(
        &gateway.storage,
        "svc-key",
        &format!("http://{addr}"),
        false,
        "/key",
        true,
    )
    .await;
    service_plugin(&gateway.storage, "svc-key", "key-auth", config).await;
    gateway.reload().await;
    gateway
}

async fn add_key(gateway: &Gateway, username: &str, key: &str) {
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
    gateway.reload().await;
}

#[tokio::test]
async fn rejects_missing_and_unknown_keys() {
    let gateway = fixture().await;
    add_key(&gateway, "team", "secret-key").await;

    let (status, headers, _) = gateway.send(get("/key/items")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(headers.get(header::WWW_AUTHENTICATE).unwrap(), "Key");

    let mut wrong = get("/key/items");
    wrong.headers_mut().insert("apikey", header_default("nope"));
    assert_eq!(gateway.send(wrong).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn accepts_header_key_and_hides_credentials() {
    let gateway = fixture().await;
    add_key(&gateway, "team", "secret-key").await;

    let mut request = get("/key/items");
    request
        .headers_mut()
        .insert("apikey", header_default("secret-key"));
    let (status, _, json) = gateway.send(request).await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(json["path"], "/items");
    assert_eq!(json["apikey"], "");
    assert_eq!(json["x_consumer_username"], "team");
}

#[tokio::test]
async fn accepts_query_key_and_removes_it_from_the_forwarded_query() {
    let gateway = fixture().await;
    add_key(&gateway, "team", "query-key").await;

    let (status, _, json) = gateway
        .send(get("/key/items?limit=5&apikey=query-key"))
        .await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(json["query"], "limit=5");
    assert_eq!(json["x_consumer_username"], "team");
}

#[tokio::test]
async fn accepts_a_json_body_key_and_scrubs_it() {
    let gateway = fixture_with(json!({
        "key_names": ["apikey"],
        "key_in_body": true,
        "hide_credentials": true
    }))
    .await;
    add_key(&gateway, "team", "body-key").await;

    let mut request = request("POST", "/key/items");
    request
        .headers_mut()
        .insert(header::CONTENT_TYPE, header_default("application/json"));
    *request.body_mut() = Body::from(json!({"apikey": "body-key", "keep": 1}).to_string());
    let (status, _, json) = gateway.send(request).await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(json["x_consumer_username"], "team");
    let body: serde_json::Value = serde_json::from_str(json["body"].as_str().unwrap()).unwrap();
    assert!(body.get("apikey").is_none());
    assert_eq!(body["keep"], 1);
}

#[tokio::test]
async fn accepts_a_form_body_key_and_scrubs_it() {
    let gateway = fixture_with(json!({
        "key_names": ["apikey"],
        "key_in_body": true,
        "hide_credentials": true
    }))
    .await;
    add_key(&gateway, "team", "body-key").await;

    let mut request = request("POST", "/key/items");
    request.headers_mut().insert(
        header::CONTENT_TYPE,
        header_default("application/x-www-form-urlencoded"),
    );
    *request.body_mut() = Body::from("apikey=body-key&keep=1");
    let (status, _, json) = gateway.send(request).await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(json["x_consumer_username"], "team");
    assert_eq!(json["body"], "keep=1");
}

#[tokio::test]
async fn includes_the_configured_realm() {
    let gateway = fixture_with(json!({"key_names": ["apikey"], "realm": "octra"})).await;
    let (status, headers, _) = gateway.send(get("/key/items")).await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        headers.get(header::WWW_AUTHENTICATE).unwrap(),
        "Key realm=\"octra\""
    );
}

#[tokio::test]
async fn can_disable_header_or_query_lookup() {
    let header_off = fixture_with(json!({
        "key_names": ["apikey"],
        "key_in_header": false
    }))
    .await;
    add_key(&header_off, "team", "secret-key").await;

    let mut request = get("/key/items");
    request
        .headers_mut()
        .insert("apikey", header_default("secret-key"));
    assert_eq!(header_off.send(request).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(
        header_off.send(get("/key/items?apikey=secret-key")).await.0,
        StatusCode::CREATED
    );

    let query_off = fixture_with(json!({
        "key_names": ["apikey"],
        "key_in_query": false
    }))
    .await;
    add_key(&query_off, "team", "secret-key").await;
    assert_eq!(
        query_off.send(get("/key/items?apikey=secret-key")).await.0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn forwards_credentials_when_hiding_is_disabled() {
    let gateway = fixture_with(json!({
        "key_names": ["apikey"],
        "hide_credentials": false
    }))
    .await;
    add_key(&gateway, "team", "secret-key").await;

    let mut request = get("/key/items");
    request
        .headers_mut()
        .insert("apikey", header_default("secret-key"));
    let (status, _, json) = gateway.send(request).await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(json["apikey"], "secret-key");
    assert_eq!(json["x_consumer_username"], "team");
}

#[tokio::test]
async fn header_keys_take_precedence_over_query_keys() {
    let gateway = fixture_with(json!({
        "key_names": ["apikey"],
        "hide_credentials": false
    }))
    .await;
    add_key(&gateway, "team", "header-key").await;

    let mut request = get("/key/items?apikey=wrong-key");
    request
        .headers_mut()
        .insert("apikey", header_default("header-key"));
    let (status, _, json) = gateway.send(request).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(json["x_consumer_username"], "team");

    // A wrong header key does not fall back to a valid query key.
    let mut request = get("/key/items?apikey=header-key");
    request
        .headers_mut()
        .insert("apikey", header_default("wrong-key"));
    assert_eq!(gateway.send(request).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn preflight_requests_can_bypass_authentication() {
    let skipped = fixture_with(json!({
        "key_names": ["apikey"],
        "run_on_preflight": false
    }))
    .await;
    assert_eq!(
        skipped.send(request("OPTIONS", "/key/items")).await.0,
        StatusCode::CREATED
    );

    let required = fixture().await;
    assert_eq!(
        required.send(request("OPTIONS", "/key/items")).await.0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn falls_back_to_an_anonymous_consumer() {
    let gateway = fixture_with(json!({
        "key_names": ["apikey"],
        "anonymous": "anon"
    }))
    .await;
    gateway
        .storage
        .api
        .create_consumer(&CreateConsumer {
            username: Some("anon".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    gateway.reload().await;

    let (status, _, json) = gateway.send(get("/key/items")).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(json["x_consumer_username"], "anon");

    let unknown = fixture_with(json!({
        "key_names": ["apikey"],
        "anonymous": "ghost"
    }))
    .await;
    assert_eq!(
        unknown.send(get("/key/items")).await.0,
        StatusCode::UNAUTHORIZED
    );
}
