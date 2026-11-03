//! End-to-end coverage for the `acl` plugin combined with `key-auth`.

use axum::http::StatusCode;
use octra_api_core::{CreateAcl, CreateConsumer, CreateConsumerGroup, CreateKeyauthCredential};
use serde_json::json;

use crate::common::{
    Gateway, gateway, get, header_default, seed_service, service_plugin, spawn_mock,
};

async fn fixture() -> Gateway {
    fixture_with(json!({"allow": ["admins"]})).await
}

async fn fixture_with(config: serde_json::Value) -> Gateway {
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
    service_plugin(&gateway.storage, "svc-acl", "acl", config).await;
    gateway.reload().await;
    gateway
}

async fn add_consumer(gateway: &Gateway, username: &str, key: &str, group: &str) -> String {
    let consumer = add_consumer_without_acl(gateway, username, key).await;
    gateway
        .storage
        .api
        .create_acl(&CreateAcl {
            consumer_id: consumer.clone(),
            group: group.into(),
            ..Default::default()
        })
        .await
        .unwrap();
    gateway.reload().await;
    consumer
}

async fn add_consumer_without_acl(gateway: &Gateway, username: &str, key: &str) -> String {
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
    consumer.id
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

async fn add_to_group(gateway: &Gateway, name: &str, consumer_id: &str) {
    let group = gateway
        .storage
        .api
        .create_consumer_group(&CreateConsumerGroup {
            name: name.into(),
            ..Default::default()
        })
        .await
        .unwrap();
    gateway
        .storage
        .api
        .add_consumer_group_member(&group.id, consumer_id)
        .await
        .unwrap();
}

#[tokio::test]
async fn authorizes_consumer_group_members_when_enabled() {
    let gateway = fixture_with(json!({
        "allow": ["admins"],
        "include_consumer_groups": true
    }))
    .await;
    let consumer = add_consumer_without_acl(&gateway, "group-admin", "group-key").await;
    add_to_group(&gateway, "admins", &consumer).await;
    gateway.reload().await;

    let (status, _, json) = gateway.send(with_key("group-key")).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(json["x_consumer_groups"], "admins");
}

#[tokio::test]
async fn ignores_consumer_groups_by_default() {
    let gateway = fixture().await;
    let consumer = add_consumer_without_acl(&gateway, "group-admin", "group-key").await;
    add_to_group(&gateway, "admins", &consumer).await;
    gateway.reload().await;

    assert_eq!(
        gateway.send(with_key("group-key")).await.0,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn merges_acl_entries_and_consumer_groups_in_the_header() {
    let gateway = fixture_with(json!({
        "allow": ["admins"],
        "include_consumer_groups": true
    }))
    .await;
    let consumer = add_consumer(&gateway, "both", "both-key", "developers").await;
    add_to_group(&gateway, "admins", &consumer).await;
    gateway.reload().await;

    let (status, _, json) = gateway.send(with_key("both-key")).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(json["x_consumer_groups"], "admins, developers");
}

#[tokio::test]
async fn denies_consumer_groups_when_enabled() {
    let gateway = fixture_with(json!({
        "deny": ["blocked"],
        "include_consumer_groups": true
    }))
    .await;
    let consumer = add_consumer_without_acl(&gateway, "blocked-user", "blocked-key").await;
    add_to_group(&gateway, "blocked", &consumer).await;
    gateway.reload().await;

    assert_eq!(
        gateway.send(with_key("blocked-key")).await.0,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn deny_lists_block_listed_groups_only() {
    let gateway = fixture_with(json!({"deny": ["blocked"]})).await;
    add_consumer(&gateway, "blocked", "blocked-key", "blocked").await;
    add_consumer(&gateway, "allowed", "allowed-key", "admins").await;

    assert_eq!(
        gateway.send(with_key("blocked-key")).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        gateway.send(with_key("allowed-key")).await.0,
        StatusCode::CREATED
    );
}

#[tokio::test]
async fn can_hide_the_groups_header() {
    let gateway = fixture_with(json!({
        "allow": ["admins"],
        "hide_groups_header": true
    }))
    .await;
    add_consumer(&gateway, "admin", "admin-key", "admins").await;

    let (status, _, json) = gateway.send(with_key("admin-key")).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(json["x_consumer_groups"], "");
}
