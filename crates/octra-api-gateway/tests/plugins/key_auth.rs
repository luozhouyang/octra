//! End-to-end coverage for the `key-auth` plugin.

use axum::http::{StatusCode, header};
use octra_api_core::{CreateConsumer, CreateKeyauthCredential};
use serde_json::json;

use crate::common::{
    Gateway, gateway, get, header_default, seed_service, service_plugin, spawn_mock,
};

async fn fixture() -> Gateway {
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
    service_plugin(
        &gateway.storage,
        "svc-key",
        "key-auth",
        json!({"key_names": ["apikey"]}),
    )
    .await;
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
    assert_eq!(
        headers.get(header::WWW_AUTHENTICATE).unwrap(),
        "Key realm=\"octra\""
    );

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
