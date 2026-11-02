//! End-to-end tests for consumer groups and group-scoped plugins.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use octra_api_core::{
    CreateConsumer, CreateConsumerGroup, CreateKeyauthCredential, CreatePluginInstance,
};
use serde_json::{Value, json};

use crate::common::{Gateway, gateway, get, header_default, request, seed_service, spawn_mock};

fn json_request(method: &str, uri: &str, body: Value) -> Request<Body> {
    let mut request = request(method, uri);
    request
        .headers_mut()
        .insert(header::CONTENT_TYPE, header_default("application/json"));
    *request.body_mut() = Body::from(body.to_string());
    request
}

async fn add_consumer(gateway: &Gateway, username: &str, key: &str) -> String {
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

#[tokio::test]
async fn consumer_group_crud_and_membership_via_the_control_plane() {
    let gateway = gateway().await;

    let (status, _, group) = gateway
        .send_control(json_request(
            "POST",
            "/api/v1/consumer-groups",
            json!({"name": "admins"}),
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let group_id = group["id"].as_str().unwrap().to_string();

    let (status, _, consumer) = gateway
        .send_control(json_request(
            "POST",
            "/api/v1/consumers",
            json!({"username": "alice"}),
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let consumer_id = consumer["id"].as_str().unwrap().to_string();

    let (status, _, member) = gateway
        .send_control(json_request(
            "POST",
            &format!("/api/v1/consumer-groups/{group_id}/consumers"),
            json!({"consumer_id": consumer_id}),
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(member["consumer_group_id"], group_id);

    let (status, _, members) = gateway
        .send_control(get(&format!(
            "/api/v1/consumer-groups/{group_id}/consumers"
        )))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(members[0]["username"], "alice");

    let (status, _, groups) = gateway
        .send_control(get(&format!(
            "/api/v1/consumers/{consumer_id}/consumer-groups"
        )))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(groups[0]["name"], "admins");

    let (status, _, group) = gateway
        .send_control(get("/api/v1/consumer-groups/admins"))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(group["id"], group_id);

    let (status, _, _) = gateway
        .send_control(request(
            "DELETE",
            &format!("/api/v1/consumer-groups/{group_id}/consumers/{consumer_id}"),
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _, members) = gateway
        .send_control(get(&format!(
            "/api/v1/consumer-groups/{group_id}/consumers"
        )))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(members.as_array().unwrap().is_empty());

    let (status, _, _) = gateway
        .send_control(json_request(
            "PUT",
            &format!("/api/v1/consumer-groups/{group_id}"),
            json!({"name": "operators"}),
        ))
        .await;
    assert_eq!(status, StatusCode::OK);

    let (status, _, _) = gateway
        .send_control(request(
            "DELETE",
            &format!("/api/v1/consumer-groups/{group_id}"),
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn authentication_plugins_cannot_be_group_scoped() {
    let gateway = gateway().await;
    let (status, _, _) = gateway
        .send_control(json_request(
            "POST",
            "/api/v1/plugins/key-auth/instances",
            json!({"consumer_group_id": "missing"}),
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn group_scoped_plugins_override_service_scope_for_members() {
    let (addr, _calls) = spawn_mock("group").await;
    let gateway = gateway().await;
    seed_service(
        &gateway.storage,
        "grp",
        &format!("http://{addr}"),
        true,
        "/grp",
        true,
    )
    .await;
    let member = add_consumer(&gateway, "member", "member-key").await;
    let outsider = add_consumer(&gateway, "outsider", "outsider-key").await;
    assert!(!outsider.is_empty());

    let group = gateway
        .storage
        .api
        .create_consumer_group(&CreateConsumerGroup {
            name: "blocked".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    gateway
        .storage
        .api
        .add_consumer_group_member(&group.id, &member)
        .await
        .unwrap();

    for (scope_group, status_code) in [(Some(group.id.clone()), 418), (None, 503)] {
        gateway
            .storage
            .api
            .create_plugin_instance(&CreatePluginInstance {
                name: "request-termination".into(),
                service_id: Some("grp".into()),
                consumer_group_id: scope_group,
                config: Some(json!({"status_code": status_code, "message": "blocked"})),
                ..Default::default()
            })
            .await
            .unwrap();
    }
    gateway.reload().await;

    let mut member_request = get("/grp/items");
    member_request
        .headers_mut()
        .insert("apikey", header_default("member-key"));
    let (status, _, json) = gateway.send(member_request).await;
    assert_eq!(status, StatusCode::IM_A_TEAPOT);
    assert_eq!(json["message"], "blocked");

    let mut outsider_request = get("/grp/items");
    outsider_request
        .headers_mut()
        .insert("apikey", header_default("outsider-key"));
    let (status, _, _) = gateway.send(outsider_request).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}
