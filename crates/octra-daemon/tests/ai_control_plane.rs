//! Daemon-level smoke tests for the AI control-plane control surface.
//!
//! The daemon mounts `ai_control::router` under the shared control app; these
//! tests exercise the same router in isolation without binding a listener.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use octra_config::OctraConfig;
use octra_llm_gateway::LlmRuntime;
use octra_llm_gateway::ai_control::{AiControlState, router};
use octra_storage::Storage;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn post(app: &Router, uri: &str, body: Value) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
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

#[tokio::test]
async fn status_reports_counts_without_a_compile() {
    let storage = Arc::new(Storage::connect_in_memory().await.unwrap());
    let state = AiControlState::new(storage);

    let response = router(state)
        .oneshot(
            Request::builder()
                .uri("/api/v1/ai/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let status: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(status.get("mode").is_none());
    assert_eq!(status["counts"]["providers"], 0);
    assert!(status["last_compile"].is_null());
}

#[tokio::test]
async fn status_reports_counts_with_an_attached_runtime() {
    let storage = Arc::new(Storage::connect_in_memory().await.unwrap());
    let runtime = Arc::new(
        LlmRuntime::bootstrap(storage.clone(), Arc::new(OctraConfig::default()))
            .await
            .unwrap(),
    );
    let state = AiControlState::new(storage).with_runtime(runtime);

    let response = router(state)
        .oneshot(
            Request::builder()
                .uri("/api/v1/ai/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let status: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(status.get("mode").is_none());
    assert_eq!(status["counts"]["models"], 0);
    assert_eq!(status["revisions"], 0);
}

#[tokio::test]
async fn refresh_reloads_the_attached_runtime() {
    let storage = Arc::new(Storage::connect_in_memory().await.unwrap());
    let runtime = Arc::new(
        LlmRuntime::bootstrap(storage.clone(), Arc::new(OctraConfig::default()))
            .await
            .unwrap(),
    );
    let state = AiControlState::new(storage).with_runtime(runtime.clone());
    let app = router(state.clone());

    let (status, body) = post(
        &app,
        "/api/v1/ai/providers",
        json!({
            "id": "provider-openai",
            "name": "openai",
            "protocol": "openai",
            "base_url": "https://api.example.com",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let (status, body) = post(
        &app,
        "/api/v1/ai/models",
        json!({
            "id": "model-chat",
            "name": "chat",
            "kind": "model",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let (status, body) = post(
        &app,
        "/api/v1/ai/models/model-chat/targets",
        json!({
            "id": "target-chat",
            "provider_id": "provider-openai",
            "upstream_model": "gpt-4o",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    assert!(runtime.snapshot().await.resolve("chat").is_none());
    state.refresh().await.unwrap();
    assert!(runtime.snapshot().await.resolve("chat").is_some());
}

#[tokio::test]
async fn refresh_without_attached_runtimes_succeeds() {
    let storage = Arc::new(Storage::connect_in_memory().await.unwrap());
    let state = AiControlState::new(storage);

    state.refresh().await.unwrap();
}
