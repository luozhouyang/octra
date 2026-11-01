//! Control plane tests: CRUD, validation status codes, reload and usage views.

mod common;

use axum::http::{Method, StatusCode};
use common::{
    COMPLETION_PATH, Harness, json_response, openai_body, openai_completion, post_json, send,
    wait_for_usage,
};
use serde_json::{Value, json};

const PROVIDERS: &str = "/api/v1/llm/providers";
const MODELS: &str = "/api/v1/llm/models";
const GROUPS: &str = "/api/v1/llm/routing-groups";
const ROUTES: &str = "/api/v1/llm/routes";
const KEYS: &str = "/api/v1/llm/virtual-keys";

async fn call(
    harness: &Harness,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let (status, _, text) = send(harness.control_router(), method, path, &[], body).await;
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

async fn create(harness: &Harness, path: &str, body: Value) -> (StatusCode, Value) {
    call(harness, Method::POST, path, Some(body)).await
}

fn provider_body(name: &str, base_url: &str) -> Value {
    json!({
        "name": name,
        "protocol": "openai",
        "base_url": base_url,
        "secret_ref": "env:OCTRA_TEST_UNUSED_SECRET",
        "config": {"wire": "openai"}
    })
}

#[tokio::test]
async fn provider_crud_round_trip() {
    let harness = Harness::new().await;

    let (status, body) = create(
        &harness,
        PROVIDERS,
        provider_body("openai-main", "https://api.openai.com/v1"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    let id = body["id"].as_str().unwrap().to_string();
    assert_eq!(body["protocol"], "openai");
    assert_eq!(body["auth_type"], "bearer");
    assert_eq!(body["config"]["wire"], "openai");
    assert!(!body.to_string().contains("sk-"));

    let (status, listed) = call(&harness, Method::GET, PROVIDERS, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed.as_array().unwrap().len(), 1);

    let (status, fetched) = call(&harness, Method::GET, &format!("{PROVIDERS}/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fetched["name"], "openai-main");

    let mut update = provider_body("openai-main", "https://api.openai.com/v1");
    update["enabled"] = json!(false);
    let (status, updated) = call(
        &harness,
        Method::PUT,
        &format!("{PROVIDERS}/{id}"),
        Some(update),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {updated}");
    assert_eq!(updated["enabled"], false);

    let (status, _) = call(&harness, Method::DELETE, &format!("{PROVIDERS}/{id}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(&harness, Method::GET, &format!("{PROVIDERS}/{id}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn provider_validation_and_conflicts() {
    let harness = Harness::new().await;

    let (status, body) = create(
        &harness,
        PROVIDERS,
        json!({"base_url": "https://a.test/v1"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["field"], "name");

    // Without a base URL the provider falls back to its protocol default.
    let (status, body) = create(&harness, PROVIDERS, json!({"name": "defaults"})).await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    assert_eq!(body["base_url"], Value::Null);

    let mut bad = provider_body("x", "https://api.openai.com/v1");
    bad["protocol"] = json!("gemini");
    let (status, body) = create(&harness, PROVIDERS, bad).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["field"], "protocol");

    let mut plaintext = provider_body("x", "https://api.openai.com/v1");
    plaintext["secret_ref"] = json!("sk-raw-secret");
    let (status, body) = create(&harness, PROVIDERS, plaintext).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["field"], "secret_ref");

    let mut bad_url = provider_body("x", "api.openai.com");
    bad_url["base_url"] = json!("api.openai.com");
    let (status, _) = create(&harness, PROVIDERS, bad_url).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, _) = create(
        &harness,
        PROVIDERS,
        provider_body("dup", "https://a.test/v1"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = create(
        &harness,
        PROVIDERS,
        provider_body("dup", "https://b.test/v1"),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "body: {body}");

    let (status, _) = call(
        &harness,
        Method::PUT,
        &format!("{PROVIDERS}/missing"),
        Some(provider_body("x", "https://a.test/v1")),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn routing_topology_crud() {
    let harness = Harness::new().await;
    let (_, provider) = create(&harness, PROVIDERS, provider_body("p", "https://a.test/v1")).await;
    let provider_id = provider["id"].as_str().unwrap().to_string();

    let (status, model) = create(
        &harness,
        MODELS,
        json!({
            "provider_id": provider_id,
            "model_id": "gpt-test",
            "display_name": "GPT Test",
            "context_window": 128000,
            "capabilities": {"vision": true},
            "pricing": {"input": 1.0}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {model}");
    assert_eq!(model["capabilities"]["vision"], true);

    let (status, body) = create(&harness, MODELS, json!({"provider_id": provider_id})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["field"], "model_id");

    let (status, group) = create(
        &harness,
        GROUPS,
        json!({"name": "g", "strategy": "priority"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {group}");
    let group_id = group["id"].as_str().unwrap().to_string();

    let (status, _) = create(
        &harness,
        GROUPS,
        json!({"name": "bad", "strategy": "random"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let target_path = format!("{GROUPS}/{group_id}/targets");
    let (status, target) = create(
        &harness,
        &target_path,
        json!({"provider_id": provider_id, "model_id": "gpt-test", "weight": 2, "priority": 1}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {target}");
    let target_id = target["id"].as_str().unwrap().to_string();

    let (status, body) = create(
        &harness,
        &target_path,
        json!({"provider_id": provider_id, "model_id": "m", "weight": 0}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["field"], "weight");

    let (status, body) = create(&harness, &target_path, json!({"model_id": "m"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["field"], "provider_id");

    let (status, updated) = call(
        &harness,
        Method::PUT,
        &format!("{target_path}/{target_id}"),
        Some(json!({"weight": 5})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {updated}");
    assert_eq!(updated["weight"], 5);
    assert_eq!(updated["model_id"], "gpt-test");

    let (status, group_view) =
        call(&harness, Method::GET, &format!("{GROUPS}/{group_id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(group_view["targets"].as_array().unwrap().len(), 1);

    let (status, route) = create(
        &harness,
        ROUTES,
        json!({"virtual_model_id": "my-model", "routing_group_id": group_id}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {route}");
    let route_id = route["id"].as_str().unwrap().to_string();

    let (status, _) = create(
        &harness,
        ROUTES,
        json!({"virtual_model_id": "my-model", "routing_group_id": group_id}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // The runtime exposes the new virtual model on the data plane.
    let (status, _, models) =
        send(harness.data_router(), Method::GET, "/v1/models", &[], None).await;
    assert_eq!(status, StatusCode::OK);
    let models: Value = serde_json::from_str(&models).unwrap();
    assert!(
        models["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|model| model["id"] == "my-model")
    );

    let (status, _) = call(
        &harness,
        Method::DELETE,
        &format!("{GROUPS}/{group_id}/targets/{target_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(
        &harness,
        Method::DELETE,
        &format!("{ROUTES}/{route_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(
        &harness,
        Method::DELETE,
        &format!("{GROUPS}/{group_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(
        &harness,
        Method::DELETE,
        &format!("{MODELS}/{}", model["id"].as_str().unwrap()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn virtual_key_is_returned_once_and_authenticates() {
    let harness = Harness::new().await;

    let (status, created) = create(
        &harness,
        KEYS,
        json!({"name": "ci", "permissions": {"chat": true}}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {created}");
    let id = created["id"].as_str().unwrap().to_string();
    let plaintext = created["key_value"].as_str().unwrap().to_string();
    assert!(plaintext.starts_with("octra-"));
    assert!(created["warning"].is_string());

    let (status, fetched) = call(&harness, Method::GET, &format!("{KEYS}/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(fetched.get("key_value").is_none());
    assert!(fetched.get("key_hash").is_none());
    assert_eq!(fetched["key_prefix"], &plaintext[..12]);

    let (status, listed) = call(&harness, Method::GET, KEYS, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!listed.to_string().contains(&plaintext));

    let mut bad = json!({"name": "bad", "expires_at": "tomorrow"});
    let (status, body) = create(&harness, KEYS, bad.clone()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["field"], "expires_at");
    bad["expires_at"] = json!("");
    let (status, _) = create(&harness, KEYS, bad).await;
    assert_eq!(status, StatusCode::CREATED);

    // With a live key present, requests need it.
    let (status, _, _) = post_json(
        harness.data_router(),
        COMPLETION_PATH,
        &[("authorization", "Bearer nope")],
        openai_body("x", false),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, updated) = call(
        &harness,
        Method::PUT,
        &format!("{KEYS}/{id}"),
        Some(json!({"enabled": false})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {updated}");
    assert_eq!(updated["enabled"], false);

    // A disabled key locks the gateway together with the other (live) key.
    let (status, _, _) = post_json(
        harness.data_router(),
        COMPLETION_PATH,
        &[("x-api-key", &plaintext)],
        openai_body("x", false),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn reload_reports_runtime_state() {
    let harness = Harness::new().await;
    let (status, body) = create(&harness, PROVIDERS, provider_body("p", "https://a.test/v1")).await;
    assert_eq!(status, StatusCode::CREATED, "body {body}");

    let (status, reload) = call(&harness, Method::POST, "/api/v1/llm/reload", None).await;
    assert_eq!(status, StatusCode::OK, "body: {reload}");
    assert_eq!(reload["status"], "reloaded");
    assert_eq!(reload["providers"], 1);
    assert!(reload["generated_at"].is_string());
}

#[tokio::test]
async fn usage_endpoint_summarizes_traffic() {
    let harness = Harness::new().await;
    let config = harness.config.clone();
    let upstream = common::MockUpstream::start(&config, |_: common::Captured| {
        json_response(openai_completion("hi"))
    })
    .await;
    let provider = harness
        .provider("mock", "openai", &upstream.base_url, "bearer", 0)
        .await;
    harness
        .route_to("octra-default", &[(provider.clone(), 0, 1)])
        .await;

    let (status, _, _) = post_json(
        harness.data_router(),
        COMPLETION_PATH,
        &[],
        openai_body("octra-default", false),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    wait_for_usage(&harness.storage, 1).await;

    let (status, usage) = call(&harness, Method::GET, "/api/v1/llm/usage?limit=5", None).await;
    assert_eq!(status, StatusCode::OK, "body: {usage}");
    assert_eq!(usage["totals"]["requests"], 1);
    assert_eq!(usage["totals"]["input_tokens"], 11);
    assert_eq!(usage["totals"]["output_tokens"], 7);
    assert_eq!(usage["by_model"][0]["virtual_model_id"], "octra-default");
    assert_eq!(usage["recent"][0]["status_code"], 200);
    assert_eq!(usage["recent"][0]["model_id"], "m");
}
