//! Integration coverage for the `/api/v1/ai/*` control-plane router.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use octra_llm_gateway::ai_control::{AiControlState, router};
use octra_storage::{AiConfigRevisionRecord, Storage};
use serde_json::{Value, json};
use tower::ServiceExt;

async fn app() -> Router {
    let storage = Arc::new(Storage::connect_in_memory().await.unwrap());
    router(AiControlState::new(storage))
}

async fn request(
    app: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let builder = Request::builder().method(method).uri(uri);
    let request = match body {
        Some(body) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

async fn get(app: &Router, uri: &str) -> (StatusCode, Value) {
    request(app, "GET", uri, None).await
}

async fn post(app: &Router, uri: &str, body: Value) -> (StatusCode, Value) {
    request(app, "POST", uri, Some(body)).await
}

async fn delete(app: &Router, uri: &str) -> (StatusCode, Value) {
    request(app, "DELETE", uri, None).await
}

/// Creates one provider, model, target, policy, and attachment. The resulting
/// control plane compiles cleanly.
async fn seed(app: &Router) {
    let (status, provider) = post(
        app,
        "/api/v1/ai/providers",
        json!({
            "id": "provider-openai",
            "name": "openai",
            "protocol": "openai",
            "base_url": "https://api.example.com",
            "auth_ref": "secret://openai",
            "capabilities": ["chat"],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{provider}");
    assert_eq!(provider["id"], "provider-openai");
    assert_eq!(provider["enabled"], true);
    assert_eq!(provider["config"], json!({}));

    let (status, model) = post(
        app,
        "/api/v1/ai/models",
        json!({
            "id": "model-chat",
            "name": "chat",
            "kind": "model",
            "capabilities": ["chat"],
            "formats": ["openai"],
            "route_paths": ["/v1/chat/completions"],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{model}");
    assert_eq!(model["id"], "model-chat");
    assert_eq!(model["kind"], "model");

    let (status, target) = post(
        app,
        "/api/v1/ai/models/model-chat/targets",
        json!({
            "id": "target-chat",
            "provider_id": "provider-openai",
            "upstream_model": "gpt-4o",
            "weight": 1,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{target}");
    assert_eq!(target["model_id"], "model-chat");
    assert_eq!(target["provider_id"], "provider-openai");

    let (status, policy) = post(
        app,
        "/api/v1/ai/policies",
        json!({
            "id": "policy-allow",
            "name": "allow-chat",
            "policy_type": "allowlist",
            "config": {"models": ["chat"]},
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{policy}");
    assert_eq!(policy["policy_type"], "allowlist");

    let (status, attachment) = post(
        app,
        "/api/v1/ai/policies/policy-allow/attachments",
        json!({
            "id": "attachment-chat",
            "scope": "model",
            "scope_id": "model-chat",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{attachment}");
    assert_eq!(attachment["policy_id"], "policy-allow");
    assert_eq!(attachment["scope"], "model");
}

#[tokio::test]
async fn crud_round_trips_for_every_entity() {
    let app = app().await;
    seed(&app).await;

    let (status, providers) = get(&app, "/api/v1/ai/providers").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(providers.as_array().unwrap().len(), 1);
    assert_eq!(providers[0]["id"], "provider-openai");

    let (status, models) = get(&app, "/api/v1/ai/models").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(models.as_array().unwrap().len(), 1);
    assert_eq!(models[0]["id"], "model-chat");

    let (status, targets) = get(&app, "/api/v1/ai/models/model-chat/targets").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(targets.as_array().unwrap().len(), 1);
    assert_eq!(targets[0]["id"], "target-chat");

    let (status, policies) = get(&app, "/api/v1/ai/policies").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(policies.as_array().unwrap().len(), 1);
    assert_eq!(policies[0]["id"], "policy-allow");

    let (status, attachments) = get(&app, "/api/v1/ai/policies/policy-allow/attachments").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(attachments.as_array().unwrap().len(), 1);
    assert_eq!(attachments[0]["id"], "attachment-chat");

    let (status, provider) = get(&app, "/api/v1/ai/providers/provider-openai").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(provider["name"], "openai");

    let (status, model) = get(&app, "/api/v1/ai/models/model-chat").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(model["name"], "chat");

    let (status, target) = get(&app, "/api/v1/ai/targets/target-chat").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(target["upstream_model"], "gpt-4o");

    let (status, policy) = get(&app, "/api/v1/ai/policies/policy-allow").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(policy["name"], "allow-chat");

    let (status, attachment) = get(&app, "/api/v1/ai/attachments/attachment-chat").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(attachment["scope_id"], "model-chat");

    let (status, missing) = get(&app, "/api/v1/ai/providers/does-not-exist").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(missing["error"].is_string());
}

#[tokio::test]
async fn validate_and_apply_create_a_revision() {
    let app = app().await;
    seed(&app).await;

    let (status, report) = post(&app, "/api/v1/ai/validate", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{report}");
    assert_eq!(report["ok"], true);
    assert_eq!(report["errors"], json!([]));
    assert_eq!(report["counts"]["providers"], 1);
    assert_eq!(report["counts"]["models"], 1);
    assert_eq!(report["counts"]["targets"], 1);

    let (status, applied) = post(&app, "/api/v1/ai/apply", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{applied}");
    assert_eq!(applied["ok"], true);
    let revision_id = applied["revision_id"]
        .as_str()
        .expect("apply returns a revision id");
    assert!(revision_id.starts_with("rev_"), "{revision_id}");
    assert_eq!(applied["report"]["ok"], true);

    let (status, revisions) = get(&app, "/api/v1/ai/revisions").await;
    assert_eq!(status, StatusCode::OK);
    let revisions = revisions.as_array().unwrap();
    assert_eq!(revisions.len(), 1);
    assert_eq!(revisions[0]["id"], revision_id);
    assert_eq!(revisions[0]["status"], "applied");
    assert_eq!(revisions[0]["diff"]["models"], 1);
    assert_eq!(revisions[0]["has_snapshot"], true);
}

#[tokio::test]
async fn preview_expands_models_and_targets() {
    let app = app().await;
    seed(&app).await;

    let (status, preview) = post(&app, "/api/v1/ai/preview", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    assert_eq!(preview["report"]["ok"], true);

    let models = preview["models"].as_array().unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0]["id"], "model-chat");
    assert_eq!(models[0]["name"], "chat");
    assert_eq!(models[0]["kind"], "model");
    assert_eq!(models[0]["policy_ids"], json!(["policy-allow"]));

    let targets = models[0]["targets"].as_array().unwrap();
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0]["id"], "target-chat");
    assert_eq!(targets[0]["provider_id"], "provider-openai");
    assert_eq!(targets[0]["upstream_model"], "gpt-4o");
    assert_eq!(targets[0]["nested_model_id"], Value::Null);
    assert_eq!(targets[0]["policy_ids"], json!(["policy-allow"]));
}

#[tokio::test]
async fn delete_target_removes_it() {
    let app = app().await;
    seed(&app).await;

    let (status, body) = delete(&app, "/api/v1/ai/targets/target-chat").await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    let (status, body) = get(&app, "/api/v1/ai/targets/target-chat").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"].is_string());
}

#[tokio::test]
async fn status_reports_counts_and_last_compile() {
    let app = app().await;
    seed(&app).await;

    let (status, body) = get(&app, "/api/v1/ai/status").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.get("mode").is_none());
    assert_eq!(body["counts"]["providers"], 1);
    assert_eq!(body["counts"]["accounts"], 0);
    assert_eq!(body["counts"]["models"], 1);
    assert_eq!(body["counts"]["targets"], 1);
    assert_eq!(body["counts"]["policies"], 1);
    assert_eq!(body["counts"]["attachments"], 1);
    assert_eq!(body["revisions"], 0);
    assert!(body["last_compile"].is_null());

    let (status, _) = post(&app, "/api/v1/ai/validate", json!({})).await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = get(&app, "/api/v1/ai/status").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["last_compile"]["ok"], true);
}

#[tokio::test]
async fn rollback_restores_the_applied_snapshot() {
    let app = app().await;
    seed(&app).await;

    let (status, applied) = post(&app, "/api/v1/ai/apply", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{applied}");
    let revision_id = applied["revision_id"]
        .as_str()
        .expect("apply returns a revision id")
        .to_string();

    // Remove an entity the snapshot contains.
    let (status, body) = delete(&app, "/api/v1/ai/targets/target-chat").await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, _) = get(&app, "/api/v1/ai/targets/target-chat").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, rolled_back) = post(
        &app,
        &format!("/api/v1/ai/revisions/{revision_id}/rollback"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{rolled_back}");
    assert_eq!(rolled_back["ok"], true);
    assert_eq!(rolled_back["revision_id"], revision_id);
    assert_eq!(rolled_back["report"]["ok"], true);
    assert_eq!(rolled_back["report"]["counts"]["targets"], 1);

    let (status, target) = get(&app, "/api/v1/ai/targets/target-chat").await;
    assert_eq!(status, StatusCode::OK, "{target}");
    assert_eq!(target["provider_id"], "provider-openai");
    assert_eq!(target["upstream_model"], "gpt-4o");

    // Rollback is not a new activation, so no extra revision is recorded.
    let (status, revisions) = get(&app, "/api/v1/ai/revisions").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(revisions.as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn rollback_overwrites_mutated_entities() {
    let app = app().await;
    seed(&app).await;

    let (status, applied) = post(&app, "/api/v1/ai/apply", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{applied}");
    let revision_id = applied["revision_id"].as_str().unwrap().to_string();

    let (status, provider) = request(
        &app,
        "PUT",
        "/api/v1/ai/providers/provider-openai",
        Some(json!({
            "name": "renamed",
            "protocol": "openai",
            "base_url": "https://changed.example.com",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{provider}");
    assert_eq!(provider["name"], "renamed");

    let (status, rolled_back) = post(
        &app,
        &format!("/api/v1/ai/revisions/{revision_id}/rollback"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{rolled_back}");

    let (status, provider) = get(&app, "/api/v1/ai/providers/provider-openai").await;
    assert_eq!(status, StatusCode::OK, "{provider}");
    assert_eq!(provider["name"], "openai");
    assert_eq!(provider["base_url"], "https://api.example.com");
}

#[tokio::test]
async fn rollback_without_snapshot_returns_bad_request() {
    let storage = Arc::new(Storage::connect_in_memory().await.unwrap());
    let app = router(AiControlState::new(storage.clone()));
    seed(&app).await;

    storage
        .ai
        .insert_revision(&AiConfigRevisionRecord {
            id: "rev-empty".to_string(),
            created_at: 1,
            actor: "test".to_string(),
            status: "applied".to_string(),
            diff_json: "{}".to_string(),
            snapshot_json: "{}".to_string(),
            applied_at: Some(1),
        })
        .await
        .unwrap();

    let (status, body) = post(&app, "/api/v1/ai/revisions/rev-empty/rollback", json!({})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], "revision has no snapshot");

    let (status, revisions) = get(&app, "/api/v1/ai/revisions").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(revisions[0]["has_snapshot"], false);

    let (status, body) = post(&app, "/api/v1/ai/revisions/rev-missing/rollback", json!({})).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(body["error"].is_string());
}

#[tokio::test]
async fn apply_without_runtime_succeeds() {
    let storage = Arc::new(Storage::connect_in_memory().await.unwrap());
    let app = router(AiControlState::new(storage));
    seed(&app).await;

    let (status, applied) = post(&app, "/api/v1/ai/apply", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{applied}");
    assert_eq!(applied["ok"], true);
}
