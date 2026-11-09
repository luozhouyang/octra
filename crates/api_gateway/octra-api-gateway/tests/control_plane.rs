//! End-to-end tests for consumer policy CRUD on the control plane.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use octra_api_gateway::{ApiControlState, ApiMetrics, control_router};
use serde_json::{Value, json};

use crate::common::{Gateway, gateway, get, header_default, request, send};

fn json_request(method: &str, uri: &str, body: Value) -> Request<Body> {
    let mut request = request(method, uri);
    request
        .headers_mut()
        .insert(header::CONTENT_TYPE, header_default("application/json"));
    *request.body_mut() = Body::from(body.to_string());
    request
}

async fn create_policy(gateway: &Gateway, body: Value) -> Value {
    let (status, _, policy) = gateway
        .send_control(json_request("POST", "/admin/policies", body))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{policy}");
    policy
}

#[tokio::test]
async fn policy_crud_round_trip() {
    let gateway = gateway().await;
    let created = create_policy(
        &gateway,
        json!({
            "name": "default",
            "document": {"allowed_models": ["octra-gpt"]},
        }),
    )
    .await;
    assert_eq!(created["name"], "default");
    assert_eq!(created["scope"], "global");
    assert!(created["consumer_id"].is_null());
    assert_eq!(created["document"]["allowed_models"][0], "octra-gpt");
    let id = created["id"].as_str().unwrap().to_string();

    let (status, _, fetched) = gateway
        .send_control(get(&format!("/admin/policies/{id}")))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fetched["id"], id);

    let (status, _, page) = gateway.send_control(get("/admin/policies")).await;
    assert_eq!(status, StatusCode::OK);
    let data = page["data"].as_array().unwrap();
    assert_eq!(data.len(), 1);
    assert_eq!(data[0]["id"], id);
    assert!(page["next"].is_null());
    assert!(page["offset"].is_null());

    let (status, _, updated) = gateway
        .send_control(json_request(
            "PATCH",
            &format!("/admin/policies/{id}"),
            json!({"name": "renamed", "document": {"allowed_models": ["other"]}}),
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(updated["name"], "renamed");
    assert_eq!(updated["document"]["allowed_models"][0], "other");
    assert_eq!(updated["scope"], "global");

    let (status, _, upserted) = gateway
        .send_control(json_request(
            "PUT",
            &format!("/admin/policies/{id}"),
            json!({"name": "upserted"}),
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(upserted["name"], "upserted");

    let (status, _, created_by_put) = gateway
        .send_control(json_request(
            "PUT",
            "/admin/policies/pol-put",
            json!({"name": "created-by-put", "document": {"models": []}}),
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(created_by_put["id"], "pol-put");
    assert_eq!(created_by_put["name"], "created-by-put");

    let (status, _, _) = gateway
        .send_control(request("DELETE", &format!("/admin/policies/{id}")))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _, missing) = gateway
        .send_control(get(&format!("/admin/policies/{id}")))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(missing["name"], "not found");
}

#[tokio::test]
async fn policy_validation_rejects_invalid_input() {
    let gateway = gateway().await;

    let (status, _, error) = gateway
        .send_control(json_request(
            "POST",
            "/admin/policies",
            json!({"name": "   "}),
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error["name"], "schema violation");
    assert!(error["message"].as_str().unwrap().contains("name"));

    let (status, _, error) = gateway
        .send_control(json_request(
            "POST",
            "/admin/policies",
            json!({"name": "bad-scope", "scope": "tenant"}),
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(error["message"].as_str().unwrap().contains("scope"));

    let (status, _, error) = gateway
        .send_control(json_request(
            "POST",
            "/admin/policies",
            json!({"name": "no-consumer", "scope": "consumer"}),
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(error["message"].as_str().unwrap().contains("consumer_id"));

    let (status, _, error) = gateway
        .send_control(json_request(
            "POST",
            "/admin/policies",
            json!({"name": "bad-document", "document": [1, 2, 3]}),
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(error["message"].as_str().unwrap().contains("document"));

    let policy = create_policy(&gateway, json!({"name": "valid"})).await;
    let id = policy["id"].as_str().unwrap();

    let (status, _, error) = gateway
        .send_control(json_request(
            "PATCH",
            &format!("/admin/policies/{id}"),
            json!({"name": ""}),
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(error["message"].as_str().unwrap().contains("name"));

    let (status, _, error) = gateway
        .send_control(json_request(
            "PATCH",
            &format!("/admin/policies/{id}"),
            json!({"scope": "consumer"}),
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(error["message"].as_str().unwrap().contains("consumer_id"));
}

#[tokio::test]
async fn consumer_scoped_policy_requires_and_stores_consumer_id() {
    let gateway = gateway().await;
    let (status, _, consumer) = gateway
        .send_control(json_request(
            "POST",
            "/admin/consumers",
            json!({"username": "policy-user"}),
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let consumer_id = consumer["id"].as_str().unwrap().to_string();

    let explicit = create_policy(
        &gateway,
        json!({
            "name": "explicit-consumer",
            "scope": "consumer",
            "consumer_id": consumer_id,
            "document": {"allowed_models": ["octra-gpt"]},
        }),
    )
    .await;
    assert_eq!(explicit["scope"], "consumer");
    assert_eq!(explicit["consumer_id"], consumer_id);

    let inferred = create_policy(
        &gateway,
        json!({"name": "inferred-consumer", "consumer_id": consumer_id}),
    )
    .await;
    assert_eq!(inferred["scope"], "consumer");
    assert_eq!(inferred["consumer_id"], consumer_id);

    let policy = create_policy(&gateway, json!({"name": "switch-to-consumer"})).await;
    let (status, _, updated) = gateway
        .send_control(json_request(
            "PATCH",
            &format!("/admin/policies/{}", policy["id"].as_str().unwrap()),
            json!({"scope": "consumer", "consumer_id": consumer_id}),
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(updated["scope"], "consumer");
    assert_eq!(updated["consumer_id"], consumer_id);
}

#[tokio::test]
async fn global_policy_scope_is_explicit() {
    let gateway = gateway().await;
    let policy = create_policy(
        &gateway,
        json!({
            "name": "global-default",
            "scope": "global",
            "document": {"allowed_providers": ["openai"]},
        }),
    )
    .await;
    assert_eq!(policy["scope"], "global");
    assert!(policy["consumer_id"].is_null());
    assert_eq!(policy["document"]["allowed_providers"][0], "openai");
}

#[tokio::test]
async fn policy_mutations_notify_the_reload_hook() {
    let gateway = gateway().await;
    let reloads = Arc::new(AtomicUsize::new(0));
    let hook_reloads = reloads.clone();
    let state = ApiControlState::with_plugins(
        gateway.storage.clone(),
        gateway.runtime.clone(),
        Arc::new(ApiMetrics::default()),
        gateway.plugins.clone(),
    )
    .with_reload_hook(Arc::new(move || {
        hook_reloads.fetch_add(1, Ordering::SeqCst);
    }));
    let control = control_router(state);

    let (status, _, policy) = send(
        control.clone(),
        json_request("POST", "/admin/policies", json!({"name": "hook"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(reloads.load(Ordering::SeqCst), 1);
    let id = policy["id"].as_str().unwrap().to_string();

    let (status, _, _) = send(
        control.clone(),
        json_request(
            "PUT",
            &format!("/admin/policies/{id}"),
            json!({"name": "hook-updated"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(reloads.load(Ordering::SeqCst), 2);

    let (status, _, _) = send(
        control.clone(),
        request("DELETE", &format!("/admin/policies/{id}")),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(reloads.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn switching_to_global_clears_consumer_id() {
    let gateway = gateway().await;
    let (status, _, consumer) = gateway
        .send_control(json_request(
            "POST",
            "/admin/consumers",
            json!({"username": "policy-global-user"}),
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let consumer_id = consumer["id"].as_str().unwrap().to_string();

    let policy = create_policy(
        &gateway,
        json!({
            "name": "consumer-then-global",
            "scope": "consumer",
            "consumer_id": consumer_id,
        }),
    )
    .await;
    assert_eq!(policy["consumer_id"], consumer_id);

    let (status, _, updated) = gateway
        .send_control(json_request(
            "PATCH",
            &format!("/admin/policies/{}", policy["id"].as_str().unwrap()),
            json!({"scope": "global"}),
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(updated["scope"], "global");
    assert!(updated["consumer_id"].is_null());
}

#[tokio::test]
async fn global_create_ignores_consumer_id() {
    let gateway = gateway().await;
    let (status, _, consumer) = gateway
        .send_control(json_request(
            "POST",
            "/admin/consumers",
            json!({"username": "policy-global-create-user"}),
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let consumer_id = consumer["id"].as_str().unwrap().to_string();

    let policy = create_policy(
        &gateway,
        json!({
            "name": "global-with-consumer",
            "scope": "global",
            "consumer_id": consumer_id,
        }),
    )
    .await;
    assert_eq!(policy["scope"], "global");
    assert!(policy["consumer_id"].is_null());
}
