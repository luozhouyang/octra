//! End-to-end coverage for the `basic-auth` plugin.

use axum::http::{StatusCode, header};
use base64::Engine as _;
use octra_api_core::{CreateBasicauthCredential, CreateConsumer};
use serde_json::json;

use crate::common::{
    Gateway, gateway, get, header_default, seed_service, service_plugin, spawn_mock,
};

async fn fixture() -> Gateway {
    let (addr, _calls) = spawn_mock("basic").await;
    let gateway = gateway().await;
    seed_service(
        &gateway.storage,
        "svc-basic",
        &format!("http://{addr}"),
        false,
        "/basic",
        true,
    )
    .await;
    service_plugin(&gateway.storage, "svc-basic", "basic-auth", json!({})).await;

    let consumer = gateway
        .storage
        .api
        .create_consumer(&CreateConsumer {
            username: Some("alice".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    let hash = bcrypt::hash("s3cret", 4).unwrap();
    gateway
        .storage
        .api
        .create_basicauth(&CreateBasicauthCredential {
            consumer_id: consumer.id,
            username: "alice".into(),
            password: hash,
            ..Default::default()
        })
        .await
        .unwrap();
    gateway.reload().await;
    gateway
}

fn basic(username: &str, password: &str) -> String {
    let encoded =
        base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"));
    format!("Basic {encoded}")
}

#[tokio::test]
async fn rejects_missing_and_invalid_credentials() {
    let gateway = fixture().await;

    let (status, headers, _) = gateway.send(get("/basic/items")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        headers.get(header::WWW_AUTHENTICATE).unwrap(),
        "Basic realm=\"octra\""
    );

    let mut wrong = get("/basic/items");
    wrong.headers_mut().insert(
        header::AUTHORIZATION,
        header_default(&basic("alice", "wrong")),
    );
    assert_eq!(gateway.send(wrong).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn accepts_valid_credentials_and_hides_authorization() {
    let gateway = fixture().await;

    let mut request = get("/basic/items");
    request.headers_mut().insert(
        header::AUTHORIZATION,
        header_default(&basic("alice", "s3cret")),
    );
    let (status, _, json) = gateway.send(request).await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(json["path"], "/items");
    assert_eq!(json["authorization"], "");
    assert_eq!(json["x_consumer_username"], "alice");
}
