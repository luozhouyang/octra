//! End-to-end coverage for the `acl` plugin combined with `key-auth`.

use axum::http::StatusCode;
use octra_api_core::{CreateAcl, CreateConsumer, CreateKeyauthCredential};
use serde_json::json;

use crate::common::{
    Gateway, gateway, get, header_default, seed_service, service_plugin, spawn_mock,
};

async fn fixture() -> Gateway {
    let (addr, _calls) = spawn_mock("acl").await;
    let gateway = gateway().await;
    seed_service(
        &gateway.storage,
        "svc-acl",
        &format!("http://{addr}"),
        false,
        "/acl",
        true,
    )
    .await;
    service_plugin(
        &gateway.storage,
        "svc-acl",
        "key-auth",
        json!({"key_names": ["apikey"]}),
    )
    .await;
    service_plugin(
        &gateway.storage,
        "svc-acl",
        "acl",
        json!({"allow": ["admins"]}),
    )
    .await;
    gateway.reload().await;
    gateway
}

async fn add_consumer(gateway: &Gateway, username: &str, key: &str, group: &str) {
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
            consumer_id: consumer.id.clone(),
            key: Some(key.into()),
            ..Default::default()
        })
        .await
        .unwrap();
    gateway
        .storage
        .api
        .create_acl(&CreateAcl {
            consumer_id: consumer.id,
            group: group.into(),
            ..Default::default()
        })
        .await
        .unwrap();
    gateway.reload().await;
}

fn with_key(key: &str) -> axum::extract::Request {
    let mut request = get("/acl/items");
    request.headers_mut().insert("apikey", header_default(key));
    request
}

#[tokio::test]
async fn allows_group_members_and_forwards_the_groups_header() {
    let gateway = fixture().await;
    add_consumer(&gateway, "admin", "admin-key", "admins").await;

    let (status, _, json) = gateway.send(with_key("admin-key")).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(json["x_consumer_username"], "admin");
    assert_eq!(json["x_consumer_groups"], "admins");
}

#[tokio::test]
async fn denies_non_members_and_unauthenticated_requests() {
    let gateway = fixture().await;
    add_consumer(&gateway, "admin", "admin-key", "admins").await;
    add_consumer(&gateway, "dev", "dev-key", "developers").await;

    assert_eq!(
        gateway.send(get("/acl/items")).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        gateway.send(with_key("dev-key")).await.0,
        StatusCode::FORBIDDEN
    );
}
