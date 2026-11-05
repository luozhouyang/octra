//! Sandbox mode must never read or write the real agent configuration files.

use std::fs;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use http_body_util::BodyExt;
use octra_agent_core::{AgentControlState, control_router};
use octra_config::OctraConfig;
use octra_storage::Storage;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn body_json(response: Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

fn request(method: &str, uri: &str, body: Option<Value>) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if body.is_some() {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
    }
    builder
        .body(body.map_or_else(Body::empty, |value| Body::from(value.to_string())))
        .unwrap()
}

#[tokio::test]
async fn sandbox_mode_keeps_writes_under_the_sandbox_home() {
    let home = tempfile::tempdir().unwrap();
    let binaries = tempfile::tempdir().unwrap();
    let claude = binaries.path().join("claude");
    fs::write(&claude, "#!/bin/sh\n").unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&claude, fs::Permissions::from_mode(0o755)).unwrap();
    }

    let mut config = OctraConfig::default();
    config.agents.sandbox_home = Some(home.path().display().to_string());
    config.agents.sandbox_path = Some(binaries.path().display().to_string());

    let storage = Arc::new(Storage::connect_in_memory().await.unwrap());
    let app = control_router(AgentControlState::new(storage, Arc::new(config)));

    let mode = body_json(
        app.clone()
            .oneshot(request("GET", "/api/v1/agents/mode", None))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(mode["sandbox"], true);
    assert_eq!(mode["home"], home.path().display().to_string());

    let agents = body_json(
        app.clone()
            .oneshot(request("GET", "/api/v1/agents", None))
            .await
            .unwrap(),
    )
    .await;
    let claude = agents
        .as_array()
        .unwrap()
        .iter()
        .find(|agent| agent["id"] == "claude-code")
        .unwrap();
    assert_eq!(claude["detection"]["installed"], true);
    assert_eq!(claude["config"]["exists"], false);

    let applied = body_json(
        app.clone()
            .oneshot(request(
                "POST",
                "/api/v1/agents/claude-code/apply",
                Some(json!({
                    "base_url": "http://127.0.0.1:8080",
                    "virtual_model_id": "octra-default",
                    "virtual_key": "octra-abcdefghijklmnop"
                })),
            ))
            .await
            .unwrap(),
    )
    .await;

    let path = applied["path"].as_str().unwrap();
    assert!(path.starts_with(home.path().to_str().unwrap()));
    let settings = home.path().join(".claude/settings.json");
    assert!(settings.is_file());
    let content = fs::read_to_string(&settings).unwrap();
    assert!(content.contains("http://127.0.0.1:8080"));
    assert!(content.contains("octra-abcdefghijklmnop"));

    let response = app
        .oneshot(request(
            "POST",
            "/api/v1/agents/claude-code/restore",
            Some(json!({})),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!settings.exists());
}
