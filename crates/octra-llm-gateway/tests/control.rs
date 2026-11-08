//! Control plane tests: CRUD, validation status codes, reload and usage views.

mod common;

use axum::http::{Method, StatusCode};
use common::{
    COMPLETION_PATH, Harness, json_response, openai_body, openai_completion, post_json, send,
    wait_for_usage,
};
use serde_json::{Value, json};

const KEYS: &str = "/admin/llm/virtual-keys";

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

async fn call_shared(
    router: &axum::Router,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let (status, _, text) = send(router.clone(), method, path, &[], body).await;
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
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
    assert!(created["key_family"].is_string());
    assert!(created["rotated_from"].is_null());

    let (status, fetched) = call(&harness, Method::GET, &format!("{KEYS}/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(fetched.get("key_value").is_none());
    assert!(fetched.get("key_hash").is_none());
    assert_eq!(fetched["key_prefix"], &plaintext[..12]);
    assert_eq!(fetched["key_family"], created["key_family"]);

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
async fn virtual_key_rotation_copies_family_and_replaces_the_old_key() {
    let harness = Harness::new().await;
    let (status, created) = create(
        &harness,
        KEYS,
        json!({
            "name": "ci",
            "permissions": {"allowed_models": ["octra-default"]}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {created}");
    let old_id = created["id"].as_str().unwrap().to_string();
    let old_plaintext = created["key_value"].as_str().unwrap().to_string();
    let family = created["key_family"].as_str().unwrap().to_string();

    let (status, rotated) = call(
        &harness,
        Method::POST,
        &format!("{KEYS}/{old_id}/rotate"),
        Some(json!({"grace_seconds": 0})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {rotated}");
    let new_id = rotated["id"].as_str().unwrap().to_string();
    let new_plaintext = rotated["key_value"].as_str().unwrap().to_string();
    assert_ne!(old_id, new_id);
    assert_ne!(old_plaintext, new_plaintext);
    assert_eq!(rotated["key_family"], family);
    assert_eq!(rotated["rotated_from"], old_id);
    assert_eq!(rotated["replaces"], old_id);
    assert_eq!(rotated["permissions"]["allowed_models"][0], "octra-default");

    // The old key is disabled immediately with grace 0.
    let (status, _, _) = post_json(
        harness.data_router(),
        COMPLETION_PATH,
        &[("x-api-key", &old_plaintext)],
        openai_body("x", false),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // The new key authenticates (the model is missing, not the key).
    let (status, _, _) = post_json(
        harness.data_router(),
        COMPLETION_PATH,
        &[("x-api-key", &new_plaintext)],
        openai_body("x", false),
    )
    .await;
    assert_ne!(status, StatusCode::UNAUTHORIZED);

    // A disabled key cannot be rotated again.
    let (status, body) = call(
        &harness,
        Method::POST,
        &format!("{KEYS}/{old_id}/rotate"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "body: {body}");

    let (status, _) = call(
        &harness,
        Method::POST,
        &format!("{KEYS}/missing/rotate"),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn virtual_key_permissions_are_validated() {
    let harness = Harness::new().await;

    let (status, created) = create(
        &harness,
        KEYS,
        json!({
            "name": "restricted",
            "permissions": {
                "allowed_models": ["gpt-5"],
                "allowed_providers": ["openai"],
                "allowed_accounts": ["acc-1"],
                "limits": [{"window": "day", "tokens": 1000}]
            }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {created}");
    let id = created["id"].as_str().unwrap().to_string();

    let (status, fetched) = call(&harness, Method::GET, &format!("{KEYS}/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fetched["permissions"]["allowed_models"][0], "gpt-5");
    assert_eq!(fetched["permissions"]["limits"][0]["window"], "day");

    for bad in [
        json!({"limits": [{"window": "year", "tokens": 1}]}),
        json!({"limits": [{"window": "day"}]}),
        json!({"allowed_models": "gpt-5"}),
    ] {
        let (status, body) =
            create(&harness, KEYS, json!({"name": "bad", "permissions": bad})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
        assert_eq!(body["field"], "permissions");
    }

    let (status, updated) = call(
        &harness,
        Method::PUT,
        &format!("{KEYS}/{id}"),
        Some(json!({
            "permissions": {"limits": [{"window": "day", "cost_micros": 0}]}
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {updated}");
    assert_eq!(updated["field"], "permissions");

    // Unknown keys remain accepted for backward compatibility.
    let (status, body) = create(
        &harness,
        KEYS,
        json!({"name": "legacy", "permissions": {"chat": true}}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
}

#[tokio::test]
async fn reload_reports_runtime_state() {
    let harness = Harness::new().await;
    harness
        .provider("p", "openai", "https://a.test/v1", "bearer", 0)
        .await;

    let (status, reload) = call(&harness, Method::POST, "/admin/llm/reload", None).await;
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

    let (status, usage) = call(&harness, Method::GET, "/admin/llm/usage?limit=5", None).await;
    assert_eq!(status, StatusCode::OK, "body: {usage}");
    assert_eq!(usage["totals"]["requests"], 1);
    assert_eq!(usage["totals"]["input_tokens"], 11);
    assert_eq!(usage["totals"]["output_tokens"], 7);
    assert_eq!(usage["by_model"][0]["virtual_model_id"], "octra-default");
    assert_eq!(usage["recent"][0]["status_code"], 200);
    assert_eq!(usage["recent"][0]["model_id"], "m");
}

// ---------------------------------------------------------------------------
// Provider accounts

// ---------------------------------------------------------------------------
// LLM plugins

const PLUGINS: &str = "/admin/llm/plugins";

async fn plugin_id(harness: &Harness, name: &str) -> String {
    let (status, listed) = call(harness, Method::GET, PLUGINS, None).await;
    assert_eq!(status, StatusCode::OK, "body: {listed}");
    listed
        .as_array()
        .unwrap()
        .iter()
        .find(|plugin| plugin["name"] == name)
        .unwrap_or_else(|| panic!("plugin {name} not listed"))["id"]
        .as_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn llm_plugin_catalog_and_config_crud() {
    let harness = Harness::new().await;
    let id = plugin_id(&harness, "llm-model-map").await;
    let single = format!("{PLUGINS}/{id}");

    let (status, detail) = call(&harness, Method::GET, &single, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["plugin"]["name"], "llm-model-map");
    assert!(detail["configs"].as_array().unwrap().is_empty());
    assert!(
        detail["config_schema"].is_object(),
        "config_schema: {}",
        detail["config_schema"]
    );
    assert_eq!(detail["config_schema"]["type"], "object");
    assert_eq!(
        detail["config_schema"]["properties"]["map"]["type"],
        "object"
    );

    let (status, config) = create(
        &harness,
        &format!("{single}/configs"),
        json!({"scope": "global", "config": {"map": {"alias": "model"}}}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {config}");
    let config_id = config["id"].as_str().unwrap().to_string();
    assert_eq!(config["scope"], "global");
    assert_eq!(config["scope_id"], "");
    assert_eq!(config["config"]["map"]["alias"], "model");

    let (status, updated) = call(
        &harness,
        Method::PUT,
        &format!("/admin/llm/plugin-configs/{config_id}"),
        Some(json!({"enabled": false})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {updated}");
    assert_eq!(updated["enabled"], false);
    assert_eq!(updated["config"]["map"]["alias"], "model");

    let (status, reloaded) = call(&harness, Method::POST, &format!("{PLUGINS}/reload"), None).await;
    assert_eq!(status, StatusCode::OK, "body: {reloaded}");
    assert_eq!(reloaded["status"], "reloaded");

    let (status, flags) = call(
        &harness,
        Method::PUT,
        &single,
        Some(json!({"enabled": false, "priority": 5})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {flags}");
    assert_eq!(flags["plugin"]["enabled"], false);
    assert_eq!(flags["plugin"]["priority"], 5);

    let (status, _) = call(
        &harness,
        Method::DELETE,
        &format!("/admin/llm/plugin-configs/{config_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, detail) = call(&harness, Method::GET, &single, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(detail["configs"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn llm_rate_limit_rejects_provider_scope() {
    let harness = Harness::new().await;
    let id = plugin_id(&harness, "llm-rate-limit").await;
    let (status, body) = create(
        &harness,
        &format!("{PLUGINS}/{id}/configs"),
        json!({
            "scope": "provider",
            "scope_id": "any",
            "config": {"minute": 10}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    assert_eq!(body["field"], "scope");
}

#[tokio::test]
async fn request_stage_plugin_rejects_provider_scope() {
    let harness = Harness::new().await;
    let id = plugin_id(&harness, "llm-param-override").await;
    let (status, body) = create(
        &harness,
        &format!("{PLUGINS}/{id}/configs"),
        json!({
            "scope": "provider",
            "scope_id": "any",
            "config": {"set": {"temperature": 0.2}}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    assert_eq!(body["field"], "scope");
    let message = body["error"].as_str().unwrap_or_default();
    assert!(message.contains("supported scopes"), "body: {body}");
    assert!(message.contains("virtual_key, model"), "body: {body}");
}

#[tokio::test]
async fn llm_plugin_config_is_validated() {
    let harness = Harness::new().await;
    let id = plugin_id(&harness, "llm-model-map").await;
    let configs = format!("{PLUGINS}/{id}/configs");

    let (status, body) = create(&harness, &configs, json!({"scope": "global", "config": {}})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");

    let (status, body) = create(
        &harness,
        &configs,
        json!({"scope": "team", "config": {"map": {"a": "m"}}}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");

    let (status, body) = create(
        &harness,
        &configs,
        json!({"scope": "model", "scope_id": "missing", "config": {"map": {"a": "m"}}}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "body: {body}");

    let (status, body) = create(
        &harness,
        &configs,
        json!({"scope": "model", "config": {"map": {"a": "m"}}}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
}

#[tokio::test]
async fn llm_plugin_effective_chain_and_metrics() {
    let harness = Harness::new().await;
    let id = plugin_id(&harness, "llm-model-map").await;
    let (status, config) = create(
        &harness,
        &format!("{PLUGINS}/{id}/configs"),
        json!({"scope": "global", "config": {"map": {"alias": "model"}}}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {config}");

    let (status, effective) = call(
        &harness,
        Method::GET,
        &format!("{PLUGINS}/effective?model=alias"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {effective}");
    assert_eq!(effective["model"], "alias");
    let chain = effective["chain"].as_array().unwrap();
    let entry = chain
        .iter()
        .find(|entry| entry["name"] == "llm-model-map")
        .expect("model map is in the effective chain");
    assert_eq!(entry["scope"], "global");
    assert_eq!(entry["scope_id"], "");

    let (status, _) = call(&harness, Method::GET, &format!("{PLUGINS}/effective"), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, metrics) = call(&harness, Method::GET, &format!("{PLUGINS}/metrics"), None).await;
    assert_eq!(status, StatusCode::OK, "body: {metrics}");
    let plugins = metrics["plugins"].as_array().unwrap();
    let map = plugins
        .iter()
        .find(|plugin| plugin["name"] == "llm-model-map")
        .expect("model map has a metrics entry");
    assert_eq!(map["stages"]["request"]["calls"], 0);
    assert!(
        plugins
            .iter()
            .any(|plugin| plugin["name"] == "llm-pii-redaction")
    );
}

#[tokio::test]
async fn provider_scoped_plugin_config_binds_to_the_provider() {
    let harness = Harness::new().await;
    let provider_id = harness
        .provider(
            "openai-main",
            "openai",
            "https://api.openai.com/v1",
            "bearer",
            0,
        )
        .await;
    let id = plugin_id(&harness, "llm-think-tags").await;

    let (status, body) = create(
        &harness,
        &format!("{PLUGINS}/{id}/configs"),
        json!({"scope": "provider", "scope_id": provider_id, "config": {}}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    assert_eq!(body["scope"], "provider");
    assert_eq!(body["scope_id"], provider_id);
}

#[tokio::test]
async fn redaction_plugin_is_registered_and_validates_config() {
    let harness = Harness::new().await;
    let id = plugin_id(&harness, "llm-pii-redaction").await;
    let configs = format!("{PLUGINS}/{id}/configs");

    let (status, body) = create(&harness, &configs, json!({"scope": "global", "config": {}})).await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    assert_eq!(body["config"], json!({}));

    let (status, body) = create(
        &harness,
        &configs,
        json!({
            "scope": "consumer",
            "scope_id": "missing-consumer",
            "config": {"custom_patterns": [{"name": "x", "regex": "("}]}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
}

const PII_ENGINES: &str = "/admin/llm/pii-engines";

#[tokio::test]
async fn pii_engine_crud_and_status() {
    let harness = Harness::new().await;

    let (status, body) = create(
        &harness,
        PII_ENGINES,
        json!({
            "name": "pii-local",
            "mode": "external",
            "transport": "http_loopback",
            "endpoint": "http://127.0.0.1:6553",
            "config": {"timeout_ms": 150}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    let id = body["id"].as_str().unwrap().to_string();
    assert_eq!(body["status"]["readiness"], "ready");

    let (status, listed) = call(&harness, Method::GET, PII_ENGINES, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed.as_array().unwrap().len(), 1);

    let (status, updated) = call(
        &harness,
        Method::PUT,
        &format!("{PII_ENGINES}/{id}"),
        Some(json!({
            "name": "pii-local",
            "enabled": false
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {updated}");
    assert_eq!(updated["enabled"], false);

    let (status, status_body) = call(
        &harness,
        Method::GET,
        &format!("{PII_ENGINES}/{id}/status"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(status_body["status"]["readiness"], "disabled");

    let (status, _) = call(
        &harness,
        Method::DELETE,
        &format!("{PII_ENGINES}/{id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn local_pii_engine_records_are_accepted() {
    let harness = Harness::new().await;

    let (status, body) = create(
        &harness,
        PII_ENGINES,
        json!({
            "name": "custom-local",
            "mode": "local",
            "enabled": false,
            "config": {"model": "astrlink-guard", "threshold": 0.7}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    assert_eq!(body["mode"], "local");
    assert_eq!(body["transport"], "in_process");
    let id = body["id"].as_str().unwrap().to_string();

    let (status, updated) = call(
        &harness,
        Method::PUT,
        &format!("{PII_ENGINES}/{id}"),
        Some(json!({"enabled": false})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {updated}");
    assert_eq!(updated["mode"], "local");

    let (status, body) = create(
        &harness,
        PII_ENGINES,
        json!({
            "name": "local-wrong-transport",
            "mode": "local",
            "transport": "uds_jsonl"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
}

const PII_MODELS: &str = "/admin/llm/pii-models";

#[tokio::test]
async fn pii_models_list_status_and_score() {
    let harness = Harness::new().await;

    let (status, listed) = call(&harness, Method::GET, PII_MODELS, None).await;
    assert_eq!(status, StatusCode::OK);
    let models = listed.as_array().unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0]["id"], "astrlink-guard");
    assert_eq!(models[0]["installed"], false);
    assert_eq!(models[0]["enabled"], false);
    assert_eq!(models[0]["threshold"], 0.8);
    assert_eq!(models[0]["state"]["state"], "idle");

    let (status, _) = call(
        &harness,
        Method::GET,
        &format!("{PII_MODELS}/not-a-model"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, body) = create(
        &harness,
        &format!("{PII_MODELS}/astrlink-guard"),
        json!({"do": "score", "score": 0.9}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["threshold"], 0.9);
    assert_eq!(body["enabled"], false);
    assert!(body["record_id"].is_string());

    for score in [0.2, 1.0] {
        let (status, body) = create(
            &harness,
            &format!("{PII_MODELS}/astrlink-guard"),
            json!({"do": "score", "score": score}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "score {score}: {body}");
    }

    let (status, body) = create(
        &harness,
        &format!("{PII_MODELS}/astrlink-guard"),
        json!({"do": "score", "score": 0}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["threshold"], 0.8);

    let (status, body) = create(
        &harness,
        &format!("{PII_MODELS}/astrlink-guard"),
        json!({"do": "off"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["enabled"], false);
}

#[tokio::test]
async fn pii_engine_rejects_invalid_configuration() {
    let harness = Harness::new().await;

    let invalid = [
        json!({"name": "missing-endpoint", "mode": "external"}),
        json!({
            "name": "remote-endpoint",
            "mode": "external",
            "endpoint": "https://example.com"
        }),
        json!({
            "name": "bad-mode",
            "mode": "sometimes",
            "endpoint": "http://127.0.0.1:6553"
        }),
        json!({
            "name": "bad-transport",
            "mode": "external",
            "transport": "tcp",
            "endpoint": "http://127.0.0.1:6553"
        }),
        json!({
            "name": "managed-http",
            "mode": "managed",
            "transport": "http_loopback",
            "endpoint": "http://127.0.0.1:6553",
            "command": "engine"
        }),
        json!({
            "name": "managed-no-endpoint",
            "mode": "managed",
            "command": "engine"
        }),
    ];
    for body in invalid {
        let (status, response) = create(&harness, PII_ENGINES, body.clone()).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "body: {body} -> {response}"
        );
    }

    let (status, body) = create(
        &harness,
        PII_ENGINES,
        json!({
            "name": "dup",
            "mode": "external",
            "endpoint": "http://127.0.0.1:6553"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    let (status, _) = create(
        &harness,
        PII_ENGINES,
        json!({
            "name": "dup",
            "mode": "external",
            "endpoint": "http://127.0.0.1:6553"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

// ---------------------------------------------------------------------------
// AI-native account OAuth

fn oauth_provider_config(token_url: &str) -> String {
    json!({
        "wire": "openai",
        "oauth": {
            "authorize_url": "https://auth.example.test/authorize",
            "token_url": token_url,
            "client_id": "client-1",
            "scopes": ["read", "offline_access"],
            "redirect_uri": "http://localhost:1455/callback",
            "extra_authorize_params": {"audience": "api"}
        }
    })
    .to_string()
}

#[tokio::test]
async fn ai_account_oauth_lifecycle() {
    let harness = Harness::new().await;
    let router = harness
        .control_router()
        .merge(octra_llm_gateway::ai_control::router(
            octra_llm_gateway::ai_control::AiControlState::new(harness.storage.clone()),
        ));
    let tokens = common::mock_token_endpoint().await;
    let provider_id = harness
        .provider_with_config(
            "oauth-main",
            "openai",
            "https://api.example.test/v1",
            "bearer",
            0,
            &oauth_provider_config(&tokens.url),
        )
        .await;
    let account_id = harness.account(&provider_id, "acct", 0, 1, None, 0).await;
    let base = format!("/api/v1/ai/accounts/{account_id}/oauth");

    let (status, start) = call_shared(&router, Method::POST, &format!("{base}/start"), None).await;
    assert_eq!(status, StatusCode::OK, "body: {start}");
    let state_token = start["state"].as_str().unwrap().to_string();
    assert_eq!(start["redirect_uri"], "http://localhost:1455/callback");
    let authorize = start["authorize_url"].as_str().unwrap();
    assert!(authorize.starts_with("https://auth.example.test/authorize?"));
    assert!(authorize.contains("client_id=client-1"));
    assert!(authorize.contains("code_challenge_method=S256"));
    assert!(authorize.contains("audience=api"));

    let (status, completed) = call_shared(
        &router,
        Method::POST,
        &format!("{base}/complete"),
        Some(json!({"code": "code-1", "state": state_token})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {completed}");
    assert_eq!(completed["oauth_status"], "authorized");
    assert_eq!(completed["credential_kind"], "oauth");
    assert!(completed["oauth_expires_at"].is_string());

    let accounts_path = format!("/api/v1/ai/providers/{provider_id}/accounts");
    let (status, accounts) = call_shared(&router, Method::GET, &accounts_path, None).await;
    assert_eq!(status, StatusCode::OK, "body: {accounts}");
    assert_eq!(accounts[0]["oauth_status"], "authorized");
    assert_eq!(
        accounts[0]["oauth_expires_at"], completed["oauth_expires_at"],
        "account list must expose the OAuth expiry"
    );

    let (status, refreshed) =
        call_shared(&router, Method::POST, &format!("{base}/refresh"), None).await;
    assert_eq!(status, StatusCode::OK, "body: {refreshed}");
    assert_eq!(refreshed["oauth_status"], "authorized");

    let (status, _) = call_shared(&router, Method::DELETE, &base, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, cleared) =
        call_shared(&router, Method::GET, &format!("{base}/status"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cleared["oauth_status"], "none");
    assert_eq!(cleared["oauth_expires_at"], Value::Null);

    let (status, accounts) = call_shared(&router, Method::GET, &accounts_path, None).await;
    assert_eq!(status, StatusCode::OK, "body: {accounts}");
    assert_eq!(accounts[0]["oauth_status"], "none");
    assert_eq!(accounts[0]["oauth_expires_at"], Value::Null);
}

#[tokio::test]
async fn ai_account_oauth_requires_provider_configuration() {
    let harness = Harness::new().await;
    let router = harness.control_router();
    let provider_id = harness
        .provider(
            "plain-provider",
            "openai",
            "https://api.example.test/v1",
            "bearer",
            0,
        )
        .await;
    let account_id = harness
        .account(&provider_id, "plain-account", 0, 1, None, 0)
        .await;
    let (status, body) = call_shared(
        &router,
        Method::POST,
        &format!("/api/v1/ai/accounts/{account_id}/oauth/start"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    assert!(body["error"].as_str().unwrap().contains("oauth"));
}
