//! End-to-end data plane tests: protocol conversion through mock upstreams,
//! virtual key auth, fallback and usage accounting.

mod common;

use std::sync::Arc;

use axum::http::StatusCode;
use common::*;
use octra_api_core::{CreateConsumer, CreatePolicy, UpdatePolicy};
use octra_llm_gateway::keys::hash_key;
use octra_storage::{LlmUsageInput, LlmVirtualKeyInput};
use serde_json::json;

fn openai_upstream_text(request: Captured) -> axum::response::Response {
    if request.body.get("stream").and_then(|v| v.as_bool()) == Some(true) {
        sse_response(openai_text_stream("Hello from OpenAI"))
    } else {
        json_response(openai_completion("Hello from OpenAI"))
    }
}

fn openai_upstream_tool(request: Captured) -> axum::response::Response {
    if request.body.get("stream").and_then(|v| v.as_bool()) == Some(true) {
        sse_response(openai_tool_stream())
    } else {
        json_response(openai_tool_completion())
    }
}

fn anthropic_upstream_text(request: Captured) -> axum::response::Response {
    if request.body.get("stream").and_then(|v| v.as_bool()) == Some(true) {
        sse_response(anthropic_text_stream("Hello from Anthropic"))
    } else {
        json_response(anthropic_message("Hello from Anthropic"))
    }
}

fn anthropic_upstream_tool(request: Captured) -> axum::response::Response {
    if request.body.get("stream").and_then(|v| v.as_bool()) == Some(true) {
        sse_response(anthropic_tool_stream())
    } else {
        json_response(anthropic_tool_message())
    }
}

fn failing_upstream(_request: Captured) -> axum::response::Response {
    json_status(
        StatusCode::INTERNAL_SERVER_ERROR,
        json!({"error": {"message": "upstream exploded"}}),
    )
}

async fn openai_harness() -> (Harness, MockUpstream) {
    let harness = Harness::new().await;
    let upstream = MockUpstream::start(&harness.config, openai_upstream_text).await;
    let provider = harness
        .provider("openai-mock", "openai", &upstream.base_url, "bearer", 0)
        .await;
    harness.route_to("octra-default", &[(provider, 0, 1)]).await;
    (harness, upstream)
}

async fn anthropic_harness() -> (Harness, MockUpstream) {
    let harness = Harness::new().await;
    let upstream = MockUpstream::start(&harness.config, anthropic_upstream_text).await;
    let provider = harness
        .provider(
            "anthropic-mock",
            "anthropic",
            &upstream.base_url,
            "anthropic",
            0,
        )
        .await;
    harness.route_to("octra-default", &[(provider, 0, 1)]).await;
    (harness, upstream)
}

#[tokio::test]
async fn openai_to_openai_non_streaming() {
    let (harness, upstream) = openai_harness().await;
    let (status, headers, body) = post_json(
        harness.data_router(),
        COMPLETION_PATH,
        &[],
        openai_body("octra-default", false),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert!(headers.get("x-request-id").is_some());
    assert_eq!(body["object"], "chat.completion");
    assert_eq!(
        body["choices"][0]["message"]["content"],
        "Hello from OpenAI"
    );
    assert_eq!(body["usage"]["prompt_tokens"], 11);

    let request = &upstream.requests()[0];
    assert_eq!(request.path, COMPLETION_PATH);
    assert_eq!(request.header("authorization"), Some("Bearer test-key"));
    assert_eq!(request.body["model"], "m");
    assert_eq!(request.body["messages"][0]["content"], "hello");

    let usage = wait_for_usage(&harness.storage, 1).await;
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0].status_code, Some(200));
    assert_eq!(usage[0].input_tokens, 11);
    assert_eq!(usage[0].output_tokens, 7);
    assert_eq!(usage[0].protocol.as_deref(), Some("openai"));
    assert_eq!(usage[0].model_id.as_deref(), Some("m"));
}

#[tokio::test]
async fn anthropic_to_anthropic_non_streaming() {
    let (harness, upstream) = anthropic_harness().await;
    let (status, _, body) = post_json(
        harness.data_router(),
        MESSAGES_PATH,
        &[],
        anthropic_body("octra-default", false),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["type"], "message");
    assert_eq!(body["content"][0]["text"], "Hello from Anthropic");
    assert_eq!(body["usage"]["input_tokens"], 13);

    let request = &upstream.requests()[0];
    assert_eq!(request.path, MESSAGES_PATH);
    assert_eq!(request.header("x-api-key"), Some("test-key"));
    assert_eq!(request.header("anthropic-version"), Some("2023-06-01"));
    assert_eq!(request.body["max_tokens"], 64);

    let usage = wait_for_usage(&harness.storage, 1).await;
    assert_eq!(usage[0].protocol.as_deref(), Some("anthropic"));
    assert_eq!(usage[0].output_tokens, 9);
}

#[tokio::test]
async fn openai_ingress_to_anthropic_upstream() {
    let harness = Harness::new().await;
    let upstream = MockUpstream::start(&harness.config, anthropic_upstream_text).await;
    let provider = harness
        .provider(
            "anthropic-mock",
            "anthropic",
            &upstream.base_url,
            "anthropic",
            0,
        )
        .await;
    harness.route_to("octra-claude", &[(provider, 0, 1)]).await;

    let (status, _, body) = post_json(
        harness.data_router(),
        COMPLETION_PATH,
        &[],
        openai_body("octra-claude", false),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["choices"][0]["message"]["content"],
        "Hello from Anthropic"
    );
    assert_eq!(body["usage"]["completion_tokens"], 9);

    let request = &upstream.requests()[0];
    assert_eq!(request.path, MESSAGES_PATH);
    assert_eq!(request.body["messages"][0]["content"][0]["text"], "hello");
    // Anthropic requires max_tokens; a default is supplied.
    assert!(request.body["max_tokens"].as_u64().unwrap_or(0) > 0);
}

#[tokio::test]
async fn anthropic_ingress_to_openai_upstream() {
    let harness = Harness::new().await;
    let upstream = MockUpstream::start(&harness.config, openai_upstream_text).await;
    let provider = harness
        .provider("openai-mock", "openai", &upstream.base_url, "bearer", 0)
        .await;
    harness.route_to("octra-openai", &[(provider, 0, 1)]).await;

    let (status, _, body) = post_json(
        harness.data_router(),
        MESSAGES_PATH,
        &[],
        anthropic_body("octra-openai", false),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["type"], "message");
    assert_eq!(body["content"][0]["text"], "Hello from OpenAI");
    assert_eq!(body["stop_reason"], "end_turn");
    assert_eq!(body["usage"]["input_tokens"], 11);

    let request = &upstream.requests()[0];
    assert_eq!(request.path, COMPLETION_PATH);
    assert_eq!(request.body["messages"][0]["content"], "hello");
}

#[tokio::test]
async fn openai_streaming_to_openai() {
    let (harness, _upstream) = openai_harness().await;
    let (status, headers, text) = send(
        harness.data_router(),
        axum::http::Method::POST,
        COMPLETION_PATH,
        &[],
        Some(openai_body("octra-default", true)),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "text/event-stream");
    assert!(text.contains("chat.completion.chunk"));
    assert!(text.contains("\"content\":\"Hello from OpenAI\""));
    assert!(text.contains("\"finish_reason\":\"stop\""));
    // Usage frames are only sent when the client asked for them.
    assert!(!text.contains("\"usage\""));
    assert!(text.trim_end().ends_with("data: [DONE]"));
}

#[tokio::test]
async fn openai_streaming_usage_and_token_accounting() {
    let (harness, upstream) = openai_harness().await;
    let mut body = openai_body("octra-default", true);
    body["stream_options"] = json!({"include_usage": true});
    let (status, _, text) = send(
        harness.data_router(),
        axum::http::Method::POST,
        COMPLETION_PATH,
        &[],
        Some(body),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let full = text.replace('\n', "<NL>");
    if !text.contains("\"usage\":{\"completion_tokens\":4,\"prompt_tokens\":3,\"total_tokens\":7}")
    {
        panic!("stream: {full}");
    }
    assert!(text.trim_end().ends_with("data: [DONE]"));
    // The gateway asks the upstream for usage.
    let request = &upstream.requests()[0];
    assert_eq!(request.body["stream_options"]["include_usage"], true);

    let usage = wait_for_usage(&harness.storage, 1).await;
    assert_eq!(usage.len(), 1, "usage rows: {usage:?}");
    assert!(
        usage
            .iter()
            .any(|row| row.input_tokens == 3 && row.output_tokens == 4),
        "usage rows: {usage:?}"
    );
}

#[tokio::test]
async fn anthropic_streaming_to_anthropic_with_tool_call() {
    let harness = Harness::new().await;
    let upstream = MockUpstream::start(&harness.config, anthropic_upstream_tool).await;
    let provider = harness
        .provider(
            "anthropic-mock",
            "anthropic",
            &upstream.base_url,
            "anthropic",
            0,
        )
        .await;
    harness.route_to("octra-claude", &[(provider, 0, 1)]).await;

    let (status, _, text) = send(
        harness.data_router(),
        axum::http::Method::POST,
        MESSAGES_PATH,
        &[],
        Some(anthropic_body("octra-claude", true)),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert!(text.contains("event: message_start"));
    assert!(text.contains("\"type\":\"tool_use\""));
    assert!(text.contains("\"id\":\"toolu_s1\""));
    assert!(text.contains("\"type\":\"input_json_delta\""));
    assert!(text.contains("\"stop_reason\":\"tool_use\""));
    assert!(text.contains("event: message_stop"));
}

#[tokio::test]
async fn openai_streaming_to_anthropic_with_tool_call() {
    let harness = Harness::new().await;
    let upstream = MockUpstream::start(&harness.config, openai_upstream_tool).await;
    let provider = harness
        .provider("openai-mock", "openai", &upstream.base_url, "bearer", 0)
        .await;
    harness.route_to("octra-mixed", &[(provider, 0, 1)]).await;

    let (status, _, text) = send(
        harness.data_router(),
        axum::http::Method::POST,
        MESSAGES_PATH,
        &[],
        Some(anthropic_body("octra-mixed", true)),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert!(text.contains("event: message_start"));
    assert!(text.contains("\"type\":\"tool_use\""));
    assert!(text.contains("\"id\":\"call_s1\""));
    assert!(text.contains("\"type\":\"input_json_delta\""));
    assert!(text.contains("\"stop_reason\":\"tool_use\""));
}

#[tokio::test]
async fn anthropic_streaming_to_openai_with_tool_call() {
    let harness = Harness::new().await;
    let upstream = MockUpstream::start(&harness.config, anthropic_upstream_tool).await;
    let provider = harness
        .provider(
            "anthropic-mock",
            "anthropic",
            &upstream.base_url,
            "anthropic",
            0,
        )
        .await;
    harness.route_to("octra-mixed", &[(provider, 0, 1)]).await;

    let (status, _, text) = send(
        harness.data_router(),
        axum::http::Method::POST,
        COMPLETION_PATH,
        &[],
        Some(openai_body("octra-mixed", true)),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert!(text.contains("\"id\":\"toolu_s1\""));
    assert!(text.contains("\"finish_reason\":\"tool_calls\""));
    assert!(text.trim_end().ends_with("data: [DONE]"));
}

#[tokio::test]
async fn non_streaming_tool_calls_cross_protocols() {
    let harness = Harness::new().await;
    let openai_upstream = MockUpstream::start(&harness.config, openai_upstream_tool).await;
    let anthropic_upstream = MockUpstream::start(&harness.config, anthropic_upstream_tool).await;
    let openai_provider = harness
        .provider(
            "openai-mock",
            "openai",
            &openai_upstream.base_url,
            "bearer",
            0,
        )
        .await;
    let anthropic_provider = harness
        .provider(
            "anthropic-mock",
            "anthropic",
            &anthropic_upstream.base_url,
            "anthropic",
            0,
        )
        .await;
    harness
        .route_to("octra-openai", &[(openai_provider, 0, 1)])
        .await;
    harness
        .route_to("octra-claude", &[(anthropic_provider, 0, 1)])
        .await;

    let (status, _, body) = post_json(
        harness.data_router(),
        MESSAGES_PATH,
        &[],
        anthropic_body("octra-openai", false),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["stop_reason"], "tool_use");
    assert_eq!(body["content"][0]["type"], "tool_use");
    assert_eq!(body["content"][0]["name"], "get_weather");
    assert_eq!(body["content"][0]["input"]["city"], "Paris");

    let (status, _, body) = post_json(
        harness.data_router(),
        COMPLETION_PATH,
        &[],
        openai_body("octra-claude", false),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(
        body["choices"][0]["message"]["tool_calls"][0]["id"],
        "toolu_t1"
    );
    assert_eq!(
        body["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
        "{\"city\":\"Paris\"}"
    );
}

#[tokio::test]
async fn routing_falls_back_to_the_next_priority_target() {
    let harness = Harness::new().await;
    let broken = MockUpstream::start(&harness.config, failing_upstream).await;
    let healthy = MockUpstream::start(&harness.config, openai_upstream_text).await;
    let broken_id = harness
        .provider("broken", "openai", &broken.base_url, "bearer", 0)
        .await;
    let healthy_id = harness
        .provider("healthy", "openai", &healthy.base_url, "bearer", 1)
        .await;
    // Lower priority value is tried first.
    harness
        .route_to("octra-default", &[(broken_id, 0, 1), (healthy_id, 1, 1)])
        .await;

    let (status, _, body) = post_json(
        harness.data_router(),
        COMPLETION_PATH,
        &[],
        openai_body("octra-default", false),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["choices"][0]["message"]["content"],
        "Hello from OpenAI"
    );
    assert_eq!(broken.request_count(), 1);
    assert_eq!(healthy.request_count(), 1);
}

#[tokio::test]
async fn missing_secret_is_a_502_and_falls_through() {
    let harness = Harness::new().await;
    let healthy = MockUpstream::start(&harness.config, openai_upstream_text).await;
    // A local server with a bearer secret that cannot be resolved.
    let broken_id = harness
        .provider("broken", "openai", "http://127.0.0.1:9/v1", "bearer", 0)
        .await;
    let healthy_id = harness
        .provider("healthy", "openai", &healthy.base_url, "bearer", 1)
        .await;
    harness
        .route_to("octra-default", &[(broken_id, 0, 1), (healthy_id, 1, 1)])
        .await;
    let (status, _, _) = post_json(
        harness.data_router(),
        COMPLETION_PATH,
        &[],
        openai_body("octra-default", false),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // With only the broken provider, the error surfaces in ingress shape.
    // The harness transport injects a test key, so build a runtime without it.
    let storage = Arc::new(octra_storage::Storage::connect_in_memory().await.unwrap());
    let config = Arc::new(octra_config::OctraConfig::default());
    let runtime = Arc::new(
        octra_llm_gateway::LlmRuntime::bootstrap(storage.clone(), config.clone())
            .await
            .unwrap(),
    );
    let harness = Harness {
        storage,
        runtime,
        config,
        plugins: Arc::new(octra_llm_gateway::LlmPluginRuntime::empty()),
    };
    let broken_id = harness
        .provider(
            "broken",
            "openai",
            "https://example.invalid/v1",
            "bearer",
            0,
        )
        .await;
    harness
        .route_to("octra-default", &[(broken_id, 0, 1)])
        .await;
    let app = octra_llm_gateway::data_plane_router(octra_llm_gateway::LlmDataState::new(
        harness.runtime.clone(),
        harness.storage.clone(),
        harness.config.clone(),
    ));
    let (status, _, body) = post_json(
        app,
        COMPLETION_PATH,
        &[],
        openai_body("octra-default", false),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "body {body}");
    assert_eq!(body["error"]["code"], "secret_unavailable");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("OCTRA_TEST_UNUSED_SECRET"),
        "body: {body}"
    );
}

#[tokio::test]
async fn upstream_errors_keep_ingress_shape() {
    let harness = Harness::new().await;
    let upstream = MockUpstream::start(&harness.config, failing_upstream).await;
    let provider = harness
        .provider("broken", "anthropic", &upstream.base_url, "anthropic", 0)
        .await;
    harness.route_to("octra-claude", &[(provider, 0, 1)]).await;

    let (status, _, body) = post_json(
        harness.data_router(),
        MESSAGES_PATH,
        &[],
        anthropic_body("octra-claude", false),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "api_error");
    let usage = wait_for_usage(&harness.storage, 1).await;
    assert_eq!(usage[0].status_code, Some(502));
    assert!(usage[0].error.is_some());
}

#[tokio::test]
async fn unknown_model_is_404_in_ingress_shape() {
    let harness = Harness::new().await;
    let (status, _, body) = post_json(
        harness.data_router(),
        COMPLETION_PATH,
        &[],
        openai_body("nope", false),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"]["message"].as_str().unwrap().contains("nope"));

    let (status, _, body) = post_json(
        harness.data_router(),
        MESSAGES_PATH,
        &[],
        anthropic_body("nope", false),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["type"], "not_found_error");
}

#[tokio::test]
async fn virtual_key_authentication() {
    use octra_llm_gateway::keys::hash_key;
    let (harness, _upstream) = openai_harness().await;
    let now = chrono::Utc::now();

    // No live keys: anonymous access.
    let (status, _, _) = post_json(
        harness.data_router(),
        COMPLETION_PATH,
        &[],
        openai_body("octra-default", false),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let valid_id = harness
        .key("valid-key", "octra-valid-key-0001", true, None)
        .await;
    let (status, _, body) = post_json(
        harness.data_router(),
        COMPLETION_PATH,
        &[],
        openai_body("octra-default", false),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["type"], "authentication_error");

    let (status, _, body) = post_json(
        harness.data_router(),
        COMPLETION_PATH,
        &[("authorization", "Bearer octra-wrong-key")],
        openai_body("octra-default", false),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "body: {body}");

    let (status, _, body) = post_json(
        harness.data_router(),
        COMPLETION_PATH,
        &[("authorization", "Bearer octra-valid-key-0001")],
        openai_body("octra-default", false),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");

    // The same key works through x-api-key and unlocks Anthropic endpoints.
    let (status, _, _) = post_json(
        harness.data_router(),
        MESSAGES_PATH,
        &[("x-api-key", "octra-valid-key-0001")],
        anthropic_body("octra-default", true),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // An expired key alone does not lock the gateway, presenting it fails.
    let expired_id = harness
        .key(
            "expired",
            "octra-expired-key-0001",
            true,
            Some((now - chrono::Duration::hours(1)).to_rfc3339()),
        )
        .await;
    assert!(!expired_id.is_empty());
    assert_eq!(
        harness
            .storage
            .llm
            .find_virtual_key_by_hash(&hash_key("octra-valid-key-0001"))
            .await
            .unwrap()
            .unwrap()
            .id,
        valid_id
    );
    harness
        .storage
        .llm
        .delete_virtual_key(&valid_id)
        .await
        .unwrap();
    harness.runtime.reload().await.unwrap();
    let (status, _, _) = post_json(
        harness.data_router(),
        COMPLETION_PATH,
        &[],
        openai_body("octra-default", false),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // No live key remains, so presenting any key (even an expired one) falls
    // back to anonymous local access.
    let (status, _, body) = post_json(
        harness.data_router(),
        COMPLETION_PATH,
        &[("authorization", "Bearer octra-expired-key-0001")],
        openai_body("octra-default", false),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body {body}");

    // The deleted key remains listed with its last use recorded.
    let keys = harness.storage.llm.list_virtual_keys().await.unwrap();
    assert!(!keys.iter().any(|key| key.id == valid_id));
}

#[tokio::test]
async fn models_listing_and_count_tokens() {
    let (harness, _upstream) = openai_harness().await;
    let (status, _, body) = send(
        harness.data_router(),
        axum::http::Method::GET,
        "/v1/models",
        &[],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let body: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(body["object"], "list");
    assert!(
        body["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|model| model["id"] == "octra-default")
    );

    let (status, _, body) = send(
        harness.data_router(),
        axum::http::Method::GET,
        "/v1/models",
        &[("anthropic-version", "2023-06-01")],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let body: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(
        body["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|model| model["type"] == "model")
    );

    let (status, _, body) = post_json(
        harness.data_router(),
        "/v1/messages/count_tokens",
        &[],
        anthropic_body("octra-default", false),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["input_tokens"].as_u64().unwrap() > 0);
}

#[tokio::test]
async fn oversized_bodies_are_rejected() {
    let mut config = octra_config::OctraConfig::default();
    config.gateway.max_body_bytes = 64;
    let storage = Arc::new(octra_storage::Storage::connect_in_memory().await.unwrap());
    let runtime = Arc::new(
        octra_llm_gateway::LlmRuntime::bootstrap(storage.clone(), Arc::new(config.clone()))
            .await
            .unwrap(),
    );
    let app = octra_llm_gateway::data_plane_router(octra_llm_gateway::LlmDataState::new(
        runtime,
        storage,
        Arc::new(config),
    ));
    let long = "x".repeat(500);
    let (status, _, _) = send(
        app,
        axum::http::Method::POST,
        COMPLETION_PATH,
        &[],
        Some(json!({"model": "m", "messages": [{"role": "user", "content": long}]})),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn consumer_policies_restrict_models_and_providers() {
    let (harness, _upstream) = openai_harness().await;
    let consumer = harness
        .storage
        .api
        .create_consumer(&CreateConsumer {
            username: Some("policy-team".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    harness
        .storage
        .llm
        .create_virtual_key(&LlmVirtualKeyInput {
            name: "policy-key".into(),
            key_prefix: "octra-policy".into(),
            key_hash: hash_key("octra-policy-key-0001"),
            consumer_id: Some(consumer.id.clone()),
            enabled: true,
            expires_at: None,
            permissions_json: "{}".into(),
        })
        .await
        .unwrap();
    harness.runtime.reload().await.unwrap();
    let auth = [("authorization", "Bearer octra-policy-key-0001")];

    // Without a policy the consumer keeps access.
    let (status, _, body) = post_json(
        harness.data_router(),
        COMPLETION_PATH,
        &auth,
        openai_body("octra-default", false),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");

    // A policy that names other models defaults the request to deny.
    let policy = harness
        .storage
        .api
        .create_policy(&CreatePolicy {
            name: "restrict-model".into(),
            consumer_id: Some(consumer.id.clone()),
            document: Some(json!({"allowed_models": ["other-model"]})),
            ..Default::default()
        })
        .await
        .unwrap();
    harness.runtime.reload().await.unwrap();

    let (status, _, body) = post_json(
        harness.data_router(),
        COMPLETION_PATH,
        &auth,
        openai_body("octra-default", false),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "body: {body}");
    assert_eq!(body["error"]["type"], "permission_error");

    // The model listing reflects the same restriction.
    let (status, _, body) = send(
        harness.data_router(),
        axum::http::Method::GET,
        "/v1/models",
        &auth,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let body: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(body["data"].as_array().unwrap().is_empty());

    // Allow the model but name a provider the route cannot reach.
    harness
        .storage
        .api
        .update_policy(
            &policy.id,
            &UpdatePolicy {
                document: Some(json!({
                    "allowed_models": ["octra-default"],
                    "allowed_providers": ["missing-provider"]
                })),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    harness.runtime.reload().await.unwrap();

    let (status, _, body) = post_json(
        harness.data_router(),
        COMPLETION_PATH,
        &auth,
        openai_body("octra-default", false),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "body: {body}");

    // Provider names are accepted as well as ids.
    harness
        .storage
        .api
        .update_policy(
            &policy.id,
            &UpdatePolicy {
                document: Some(json!({
                    "allowed_models": ["octra-default"],
                    "allowed_providers": ["openai-mock"]
                })),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    harness.runtime.reload().await.unwrap();

    let (status, _, body) = post_json(
        harness.data_router(),
        COMPLETION_PATH,
        &auth,
        openai_body("octra-default", false),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
}

fn responses_upstream(_request: Captured) -> axum::response::Response {
    json_response(json!({
        "id": "resp_1",
        "object": "response",
        "model": "m",
        "status": "completed",
        "output": [{
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "Hello from Responses"}]
        }],
        "usage": {"input_tokens": 3, "output_tokens": 4}
    }))
}

fn gemini_upstream(_request: Captured) -> axum::response::Response {
    json_response(json!({
        "candidates": [{
            "content": {"role": "model", "parts": [{"text": "Hello from Gemini"}]},
            "finishReason": "STOP"
        }],
        "usageMetadata": {"promptTokenCount": 2, "candidatesTokenCount": 3}
    }))
}

#[tokio::test]
async fn responses_ingress_round_trip() {
    let harness = Harness::new().await;
    let upstream = MockUpstream::start(&harness.config, responses_upstream).await;
    let provider = harness
        .provider(
            "responses-mock",
            "openai_responses",
            &upstream.base_url,
            "bearer",
            0,
        )
        .await;
    harness.route_to("octra-default", &[(provider, 0, 1)]).await;

    let (status, _, body) = post_json(
        harness.data_router(),
        "/v1/responses",
        &[],
        json!({"model": "octra-default", "input": "hi"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["object"], "response");
    assert_eq!(body["output_text"], "Hello from Responses");
    assert_eq!(body["usage"]["input_tokens"], 3);
    assert_eq!(upstream.requests()[0].path, "/v1/responses");
}

#[tokio::test]
async fn gemini_ingress_to_openai_upstream() {
    let (harness, upstream) = openai_harness().await;
    let (status, _, body) = post_json(
        harness.data_router(),
        "/v1beta/models/octra-default:generateContent",
        &[],
        json!({"contents": [{"role": "user", "parts": [{"text": "hi"}]}]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(
        body["candidates"][0]["content"]["parts"][0]["text"],
        "Hello from OpenAI"
    );
    assert_eq!(body["usageMetadata"]["promptTokenCount"], 11);
    assert_eq!(upstream.request_count(), 1);
}

#[tokio::test]
async fn openai_ingress_to_gemini_upstream() {
    let harness = Harness::new().await;
    let upstream = MockUpstream::start(&harness.config, gemini_upstream).await;
    let provider = harness
        .provider("gemini-mock", "gemini", &upstream.base_url, "header", 0)
        .await;
    harness.route_to("octra-default", &[(provider, 0, 1)]).await;

    let (status, _, body) = post_json(
        harness.data_router(),
        COMPLETION_PATH,
        &[],
        openai_body("octra-default", false),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(
        body["choices"][0]["message"]["content"],
        "Hello from Gemini"
    );
    let captured = &upstream.requests()[0];
    assert!(
        captured.path.contains(":generateContent"),
        "{}",
        captured.path
    );
    assert!(
        captured.body.get("model").is_none(),
        "gemini body: {}",
        captured.body
    );
}

#[tokio::test]
async fn responses_streaming_ingress() {
    let harness = Harness::new().await;
    let upstream = MockUpstream::start(&harness.config, openai_upstream_text).await;
    let provider = harness
        .provider("openai-mock", "openai", &upstream.base_url, "bearer", 0)
        .await;
    harness.route_to("octra-default", &[(provider, 0, 1)]).await;

    let (status, _, body) = send(
        harness.data_router(),
        axum::http::Method::POST,
        "/v1/responses",
        &[],
        Some(json!({"model": "octra-default", "input": "hi", "stream": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert!(body.contains("response.output_text.delta"), "body: {body}");
    assert!(body.contains("response.completed"), "body: {body}");
}

// ---------------------------------------------------------------------------
// Provider accounts

use std::sync::atomic::{AtomicUsize, Ordering};

static RATE_LIMIT_ONCE: AtomicUsize = AtomicUsize::new(0);

fn rate_limited_once(_request: Captured) -> axum::response::Response {
    if RATE_LIMIT_ONCE.fetch_add(1, Ordering::SeqCst) == 0 {
        json_status(
            StatusCode::TOO_MANY_REQUESTS,
            json!({"error": {"message": "rate limited"}}),
        )
    } else {
        json_response(openai_completion("second account"))
    }
}

#[tokio::test]
async fn rate_limited_account_falls_back_to_the_next() {
    RATE_LIMIT_ONCE.store(0, Ordering::SeqCst);
    let harness = Harness::new().await;
    let upstream = MockUpstream::start(&harness.config, rate_limited_once).await;
    let provider = harness
        .provider("openai-mock", "openai", &upstream.base_url, "bearer", 0)
        .await;
    let first = harness.account(&provider, "a-first", 0, 1, None, 0).await;
    let second = harness.account(&provider, "b-second", 0, 1, None, 0).await;
    harness.route_to("octra-default", &[(provider, 0, 1)]).await;

    let (status, _, body) = post_json(
        harness.data_router(),
        COMPLETION_PATH,
        &[],
        openai_body("octra-default", false),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["choices"][0]["message"]["content"], "second account");
    assert_eq!(upstream.request_count(), 2);

    let usage = wait_for_usage(&harness.storage, 1).await;
    assert_eq!(usage[0].account_id.as_deref(), Some(second.as_str()));
    assert_ne!(usage[0].account_id.as_deref(), Some(first.as_str()));

    // The rate-limited account is taken out of rotation for a while.
    let mut cooled = false;
    for _ in 0..50 {
        let account = harness
            .storage
            .llm
            .get_account(&first)
            .await
            .unwrap()
            .unwrap();
        if account.cooldown_until.is_some() {
            cooled = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(cooled, "rate-limited account should be cooling down");
}

#[tokio::test]
async fn exhausted_account_is_skipped() {
    let harness = Harness::new().await;
    let upstream = MockUpstream::start(&harness.config, openai_upstream_text).await;
    let provider = harness
        .provider("openai-mock", "openai", &upstream.base_url, "bearer", 0)
        .await;
    harness
        .account(&provider, "a-exhausted", 0, 1, Some(1), 1)
        .await;
    let healthy = harness.account(&provider, "b-healthy", 0, 1, None, 0).await;
    harness.route_to("octra-default", &[(provider, 0, 1)]).await;

    let (status, _, body) = post_json(
        harness.data_router(),
        COMPLETION_PATH,
        &[],
        openai_body("octra-default", false),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(upstream.request_count(), 1);
    let usage = wait_for_usage(&harness.storage, 1).await;
    assert_eq!(usage[0].account_id.as_deref(), Some(healthy.as_str()));
}

#[tokio::test]
async fn session_header_pins_requests_to_one_account() {
    let harness = Harness::new().await;
    let upstream = MockUpstream::start(&harness.config, openai_upstream_text).await;
    let provider = harness
        .provider("openai-mock", "openai", &upstream.base_url, "bearer", 0)
        .await;
    harness.account(&provider, "a", 0, 1, None, 0).await;
    harness.account(&provider, "b", 0, 1, None, 0).await;
    harness.route_to("octra-default", &[(provider, 0, 1)]).await;

    for _ in 0..2 {
        let (status, _, body) = send(
            harness.data_router(),
            axum::http::Method::POST,
            COMPLETION_PATH,
            &[("x-octra-session", "sticky-session")],
            Some(openai_body("octra-default", false)),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "body: {body}");
    }

    let usage = wait_for_usage(&harness.storage, 2).await;
    let first = usage[1].account_id.clone();
    assert!(first.is_some());
    assert_eq!(usage[0].account_id, first);
}

#[tokio::test]
async fn usage_cost_comes_from_model_pricing() {
    let harness = Harness::new().await;
    let upstream = MockUpstream::start(&harness.config, openai_upstream_text).await;
    let provider = harness
        .provider("openai-mock", "openai", &upstream.base_url, "bearer", 0)
        .await;
    harness.account(&provider, "a", 0, 1, None, 0).await;
    harness
        .model_priced(
            &provider,
            "m",
            r#"{"currency":"USD","input_per_million":3.0,"output_per_million":15.0}"#,
        )
        .await;
    harness.route_to("octra-default", &[(provider, 0, 1)]).await;

    let (status, _, body) = post_json(
        harness.data_router(),
        COMPLETION_PATH,
        &[],
        openai_body("octra-default", false),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");

    let usage = wait_for_usage(&harness.storage, 1).await;
    // prompt_tokens 11 * 3 + completion_tokens 7 * 15 = 138 micro-dollars.
    assert_eq!(usage[0].cost_micros, Some(138));
    assert_eq!(usage[0].cost_currency.as_deref(), Some("USD"));
    assert_eq!(usage[0].finish_reason.as_deref(), Some("stop"));
}

// ---------------------------------------------------------------------------
// LLM plugins

#[tokio::test]
async fn model_map_routes_to_the_mapped_model() {
    let harness = Harness::new().await;
    let upstream = MockUpstream::start(&harness.config, openai_upstream_text).await;
    let provider = harness
        .provider("openai-mock", "openai", &upstream.base_url, "bearer", 0)
        .await;
    harness.route_to("aliased", &[(provider, 0, 1)]).await;
    harness
        .plugin_config(
            "llm-model-map",
            "global",
            None,
            json!({"map": {"octra-default": "aliased"}}),
        )
        .await;

    let (status, _, body) = post_json(
        harness.data_router(),
        COMPLETION_PATH,
        &[],
        openai_body("octra-default", false),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["model"], "octra-default");
    let usage = wait_for_usage(&harness.storage, 1).await;
    assert_eq!(usage[0].virtual_model_id, "octra-default");
}

#[tokio::test]
async fn model_map_cannot_bypass_model_policy() {
    let harness = Harness::new().await;
    let upstream = MockUpstream::start(&harness.config, openai_upstream_text).await;
    let provider = harness
        .provider("openai-mock", "openai", &upstream.base_url, "bearer", 0)
        .await;
    harness.route_to("octra-default", &[(provider, 0, 1)]).await;

    let consumer = harness
        .storage
        .api
        .create_consumer(&CreateConsumer {
            username: Some("mapped-team".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    harness
        .storage
        .llm
        .create_virtual_key(&LlmVirtualKeyInput {
            name: "mapped-key".into(),
            key_prefix: "octra-mapped".into(),
            key_hash: hash_key("octra-mapped-key-0001"),
            consumer_id: Some(consumer.id.clone()),
            enabled: true,
            expires_at: None,
            permissions_json: "{}".into(),
        })
        .await
        .unwrap();
    harness
        .storage
        .api
        .create_policy(&CreatePolicy {
            name: "allow-octra-default".into(),
            consumer_id: Some(consumer.id),
            document: Some(json!({"allowed_models": ["octra-default"]})),
            ..Default::default()
        })
        .await
        .unwrap();
    harness.runtime.reload().await.unwrap();
    harness
        .plugin_config(
            "llm-model-map",
            "global",
            None,
            json!({"map": {"octra-default": "not-allowed"}}),
        )
        .await;

    let (status, _, body) = post_json(
        harness.data_router(),
        COMPLETION_PATH,
        &[("authorization", "Bearer octra-mapped-key-0001")],
        openai_body("octra-default", false),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "body: {body}");
    assert_eq!(body["error"]["code"], "model_forbidden");
    assert_eq!(upstream.request_count(), 0);
}

#[tokio::test]
async fn param_override_and_system_prompt_reach_the_upstream() {
    let (harness, upstream) = openai_harness().await;
    harness
        .plugin_config(
            "llm-system-prompt",
            "global",
            None,
            json!({"prompt": "be brief"}),
        )
        .await;
    harness
        .plugin_config(
            "llm-param-override",
            "global",
            None,
            json!({"set": {"temperature": 0.2, "max_tokens": 32}}),
        )
        .await;

    let (status, _, body) = post_json(
        harness.data_router(),
        COMPLETION_PATH,
        &[],
        openai_body("octra-default", false),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");

    let request = &upstream.requests()[0];
    assert_eq!(request.body["temperature"], 0.2);
    assert_eq!(request.body["max_tokens"], 32);
    let messages = request.body["messages"].as_array().unwrap();
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[0]["content"], "be brief");
}

fn think_tag_stream(_request: Captured) -> axum::response::Response {
    sse_response(openai_text_stream("<think>secret reasoning</think>hello"))
}

#[tokio::test]
async fn think_tags_strips_reasoning_from_streams() {
    let harness = Harness::new().await;
    let upstream = MockUpstream::start(&harness.config, think_tag_stream).await;
    let provider = harness
        .provider("openai-mock", "openai", &upstream.base_url, "bearer", 0)
        .await;
    harness.route_to("octra-default", &[(provider, 0, 1)]).await;
    harness
        .plugin_config("llm-think-tags", "global", None, json!({}))
        .await;

    let (status, _, body) = send(
        harness.data_router(),
        axum::http::Method::POST,
        COMPLETION_PATH,
        &[],
        Some(openai_body("octra-default", true)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert!(!body.contains("secret reasoning"), "body: {body}");
    assert!(body.contains("hello"), "body: {body}");
}

// ---------------------------------------------------------------------------
// PII redaction

fn prompt_text(request: &Captured) -> String {
    request.body["messages"]
        .as_array()
        .and_then(|messages| {
            messages
                .iter()
                .rev()
                .find(|message| message["role"] == "user")
        })
        .and_then(|message| message["content"].as_str())
        .unwrap_or_default()
        .to_string()
}

fn echo_upstream(request: Captured) -> axum::response::Response {
    json_response(openai_completion(&prompt_text(&request)))
}

fn echo_stream(request: Captured) -> axum::response::Response {
    let text = prompt_text(&request);
    let mut out = String::new();
    out.push_str(&openai_chunk(json!({"role": "assistant"}), None));
    for chunk in text.as_bytes().chunks(5) {
        let chunk = String::from_utf8_lossy(chunk);
        out.push_str(&openai_chunk(json!({"content": chunk}), None));
    }
    out.push_str(&openai_chunk(json!({}), Some("stop")));
    out.push_str("data: [DONE]\n\n");
    sse_response(out)
}

async fn redaction_harness(
    handler: fn(Captured) -> axum::response::Response,
) -> (Harness, MockUpstream) {
    let harness = Harness::new().await;
    let upstream = MockUpstream::start(&harness.config, handler).await;
    let provider = harness
        .provider("openai-mock", "openai", &upstream.base_url, "bearer", 0)
        .await;
    harness.route_to("octra-default", &[(provider, 0, 1)]).await;
    (harness, upstream)
}

#[tokio::test]
async fn pii_redaction_masks_upstream_and_restores_client() {
    let (harness, upstream) = redaction_harness(echo_upstream).await;
    harness
        .plugin_config("llm-pii-redaction", "global", None, json!({}))
        .await;

    let (status, _, body) = post_json(
        harness.data_router(),
        COMPLETION_PATH,
        &[],
        json!({
            "model": "octra-default",
            "messages": [{"role": "user", "content": "email me at a@b.com today"}]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(
        body["choices"][0]["message"]["content"],
        "email me at a@b.com today"
    );

    let sent = prompt_text(&upstream.requests()[0]);
    assert!(!sent.contains("a@b.com"), "upstream saw: {sent}");
    assert!(sent.contains("[[octra:"), "upstream saw: {sent}");
}

#[tokio::test]
async fn pii_redaction_restores_split_stream_placeholders() {
    let (harness, upstream) = redaction_harness(echo_stream).await;
    harness
        .plugin_config("llm-pii-redaction", "global", None, json!({}))
        .await;

    let (status, _, body) = send(
        harness.data_router(),
        axum::http::Method::POST,
        COMPLETION_PATH,
        &[],
        Some(json!({
            "model": "octra-default",
            "messages": [{"role": "user", "content": "email a@b.com now"}],
            "stream": true
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert!(body.contains("a@b.com"), "body: {body}");
    assert!(!body.contains("[[octra:"), "body: {body}");

    let sent = prompt_text(&upstream.requests()[0]);
    assert!(!sent.contains("a@b.com"), "upstream saw: {sent}");
}

#[tokio::test]
async fn irreversible_redaction_keeps_placeholders_for_the_client() {
    let (harness, _upstream) = redaction_harness(echo_upstream).await;
    harness
        .plugin_config(
            "llm-pii-redaction",
            "global",
            None,
            json!({"mode": "irreversible"}),
        )
        .await;

    let (status, _, body) = post_json(
        harness.data_router(),
        COMPLETION_PATH,
        &[],
        json!({
            "model": "octra-default",
            "messages": [{"role": "user", "content": "email a@b.com"}]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let content = body["choices"][0]["message"]["content"].as_str().unwrap();
    assert!(content.contains("[[octra:"), "body: {body}");
    assert!(!content.contains("a@b.com"), "body: {body}");
}

#[tokio::test]
async fn redaction_state_is_isolated_between_concurrent_requests() {
    let (harness, _upstream) = redaction_harness(echo_upstream).await;
    harness
        .plugin_config("llm-pii-redaction", "global", None, json!({}))
        .await;

    let first = post_json(
        harness.data_router(),
        COMPLETION_PATH,
        &[],
        json!({
            "model": "octra-default",
            "messages": [{"role": "user", "content": "first@one.com"}]
        }),
    );
    let second = post_json(
        harness.data_router(),
        COMPLETION_PATH,
        &[],
        json!({
            "model": "octra-default",
            "messages": [{"role": "user", "content": "second@two.com"}]
        }),
    );
    let (first, second) = tokio::join!(first, second);
    assert_eq!(first.0, StatusCode::OK, "body: {}", first.2);
    assert_eq!(second.0, StatusCode::OK, "body: {}", second.2);
    assert_eq!(first.2["choices"][0]["message"]["content"], "first@one.com");
    assert_eq!(
        second.2["choices"][0]["message"]["content"],
        "second@two.com"
    );
}

#[tokio::test]
async fn intent_header_routes_to_a_capable_target() {
    let harness = Harness::new().await;
    let upstream = MockUpstream::start(&harness.config, openai_upstream_text).await;
    let provider = harness
        .provider("intent-mock", "openai", &upstream.base_url, "bearer", 0)
        .await;
    harness
        .model_with(
            &provider,
            "plain-model",
            Some(128_000),
            r#"{"vision": false}"#,
            "{}",
        )
        .await;
    harness
        .model_with(
            &provider,
            "vision-model",
            Some(128_000),
            r#"{"vision": true}"#,
            "{}",
        )
        .await;
    harness
        .route_to_strategy(
            "octra-vision",
            "intent",
            &[
                (provider.clone(), "plain-model".to_string(), 0, 1),
                (provider.clone(), "vision-model".to_string(), 0, 1),
            ],
        )
        .await;
    harness
        .plugin_config("llm-intent-hint", "global", None, json!({}))
        .await;

    let (status, _, text) = send(
        harness.data_router(),
        axum::http::Method::POST,
        COMPLETION_PATH,
        &[("x-octra-intent", "vision")],
        Some(openai_body("octra-vision", false)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {text}");
    let captured = upstream.requests();
    let request = captured.last().expect("upstream request");
    assert_eq!(request.body["model"], "vision-model");
}

#[tokio::test]
async fn pacing_spreads_requests_across_accounts() {
    let harness = Harness::new().await;
    let upstream = MockUpstream::start(&harness.config, openai_upstream_text).await;
    let provider = harness
        .provider("pace-mock", "openai", &upstream.base_url, "bearer", 0)
        .await;
    harness
        .route_to("octra-default", &[(provider.clone(), 0, 1)])
        .await;
    harness
        .account_with_config(
            &provider,
            "pace-a",
            0,
            1,
            r#"{"requests_per_minute": 1, "burst": 1}"#,
        )
        .await;
    harness
        .account_with_config(
            &provider,
            "pace-b",
            0,
            1,
            r#"{"requests_per_minute": 1, "burst": 1}"#,
        )
        .await;

    for _ in 0..2 {
        let (status, _, text) = send(
            harness.data_router(),
            axum::http::Method::POST,
            COMPLETION_PATH,
            &[],
            Some(openai_body("octra-default", false)),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "body: {text}");
    }

    let usage = wait_for_usage(&harness.storage, 2).await;
    let names: std::collections::BTreeSet<_> = usage
        .iter()
        .filter_map(|row| row.account_name.clone())
        .collect();
    assert_eq!(
        names,
        ["pace-a".to_string(), "pace-b".to_string()]
            .into_iter()
            .collect()
    );
}

#[tokio::test]
async fn fully_paced_account_returns_429() {
    let harness = Harness::new().await;
    let upstream = MockUpstream::start(&harness.config, openai_upstream_text).await;
    let provider = harness
        .provider("paced-mock", "openai", &upstream.base_url, "bearer", 0)
        .await;
    harness
        .route_to("octra-default", &[(provider.clone(), 0, 1)])
        .await;
    harness
        .account_with_config(
            &provider,
            "only-account",
            0,
            1,
            r#"{"requests_per_minute": 1, "burst": 1}"#,
        )
        .await;

    let first = post_json(
        harness.data_router(),
        COMPLETION_PATH,
        &[],
        openai_body("octra-default", false),
    )
    .await;
    assert_eq!(first.0, StatusCode::OK, "body: {}", first.2);

    let second = post_json(
        harness.data_router(),
        COMPLETION_PATH,
        &[],
        openai_body("octra-default", false),
    )
    .await;
    assert_eq!(
        second.0,
        StatusCode::TOO_MANY_REQUESTS,
        "body: {}",
        second.2
    );
    assert_eq!(second.2["error"]["code"], "account_paced");
}

#[tokio::test]
async fn budget_guard_rejects_after_the_request_limit() {
    let harness = Harness::new().await;
    let upstream = MockUpstream::start(&harness.config, openai_upstream_text).await;
    let provider = harness
        .provider("budget-mock", "openai", &upstream.base_url, "bearer", 0)
        .await;
    harness.route_to("octra-default", &[(provider, 0, 1)]).await;
    harness
        .plugin_config(
            "llm-budget-guard",
            "global",
            None,
            json!({"requests": 1, "period": "day"}),
        )
        .await;

    let first = post_json(
        harness.data_router(),
        COMPLETION_PATH,
        &[],
        openai_body("octra-default", false),
    )
    .await;
    assert_eq!(first.0, StatusCode::OK, "body: {}", first.2);

    let second = post_json(
        harness.data_router(),
        COMPLETION_PATH,
        &[],
        openai_body("octra-default", false),
    )
    .await;
    assert_eq!(
        second.0,
        StatusCode::TOO_MANY_REQUESTS,
        "body: {}",
        second.2
    );
    assert_eq!(second.2["error"]["code"], "budget_exceeded");
}

#[tokio::test]
async fn budget_survives_a_plugin_reload() {
    let harness = Harness::new().await;
    let upstream = MockUpstream::start(&harness.config, openai_upstream_text).await;
    let provider = harness
        .provider("budget-reload", "openai", &upstream.base_url, "bearer", 0)
        .await;
    harness.route_to("octra-default", &[(provider, 0, 1)]).await;
    harness
        .plugin_config(
            "llm-budget-guard",
            "global",
            None,
            json!({"requests": 1, "period": "day"}),
        )
        .await;

    let first = post_json(
        harness.data_router(),
        COMPLETION_PATH,
        &[],
        openai_body("octra-default", false),
    )
    .await;
    assert_eq!(first.0, StatusCode::OK, "body: {}", first.2);

    harness.plugins.reload().await.unwrap();

    let second = post_json(
        harness.data_router(),
        COMPLETION_PATH,
        &[],
        openai_body("octra-default", false),
    )
    .await;
    assert_eq!(
        second.0,
        StatusCode::TOO_MANY_REQUESTS,
        "body: {}",
        second.2
    );
    assert_eq!(second.2["error"]["code"], "budget_exceeded");
}

#[tokio::test]
async fn plugin_state_round_trips_through_storage() {
    let harness = Harness::new().await;
    let key = "budget:round-trip";
    let value = json!({"ends_at_ms": 10, "requests": 3, "tokens": 7});
    harness
        .storage
        .llm
        .upsert_plugin_state(key, &value.to_string())
        .await
        .unwrap();
    let rows = harness.storage.llm.load_plugin_state().await.unwrap();
    let row = rows
        .iter()
        .find(|(name, _)| name == key)
        .expect("persisted plug state");
    assert_eq!(row.1, value);
}

#[tokio::test]
async fn audit_plugin_keeps_streaming_output_intact() {
    let harness = Harness::new().await;
    let upstream = MockUpstream::start(&harness.config, openai_upstream_text).await;
    let provider = harness
        .provider("audit-mock", "openai", &upstream.base_url, "bearer", 0)
        .await;
    harness.route_to("octra-default", &[(provider, 0, 1)]).await;
    harness
        .plugin_config("llm-request-audit", "global", None, json!({}))
        .await;

    let (status, _, text) = send(
        harness.data_router(),
        axum::http::Method::POST,
        COMPLETION_PATH,
        &[],
        Some(openai_body("octra-default", true)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(text.contains("Hello from OpenAI"), "body: {text}");
}

// ---------------------------------------------------------------------------
// Virtual key governance

const KEY_HEADER: &str = "authorization";
const KEY_VALUE: &str = "Bearer octra-governance-key-0001";

#[tokio::test]
async fn virtual_key_model_allow_list_blocks_other_models() {
    let (harness, upstream) = openai_harness().await;
    harness
        .key_with_permissions(
            "restricted",
            "octra-governance-key-0001",
            json!({"allowed_models": ["other-model"]}),
        )
        .await;

    let (status, _, body) = post_json(
        harness.data_router(),
        COMPLETION_PATH,
        &[(KEY_HEADER, KEY_VALUE)],
        openai_body("octra-default", false),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "body: {body}");
    assert_eq!(body["error"]["code"], "model_forbidden");
    assert_eq!(upstream.request_count(), 0);

    harness
        .storage
        .llm
        .update_virtual_key(
            &harness.storage.llm.list_virtual_keys().await.unwrap()[0].id,
            &LlmVirtualKeyInput {
                name: "restricted".into(),
                key_prefix: "octra-govern".into(),
                key_hash: hash_key("octra-governance-key-0001"),
                consumer_id: None,
                enabled: true,
                expires_at: None,
                permissions_json: json!({"allowed_models": ["octra-default"]}).to_string(),
            },
        )
        .await
        .unwrap();
    harness.runtime.reload().await.unwrap();

    let (status, _, body) = post_json(
        harness.data_router(),
        COMPLETION_PATH,
        &[(KEY_HEADER, KEY_VALUE)],
        openai_body("octra-default", false),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(upstream.request_count(), 1);
}

#[tokio::test]
async fn virtual_key_alias_cannot_escape_allow_list() {
    let harness = Harness::new().await;
    let upstream = MockUpstream::start(&harness.config, openai_upstream_text).await;
    let provider = harness
        .provider("alias-mock", "openai", &upstream.base_url, "bearer", 0)
        .await;
    harness.route_to("not-allowed", &[(provider, 0, 1)]).await;
    harness
        .plugin_config(
            "llm-model-map",
            "global",
            None,
            json!({"map": {"octra-default": "not-allowed"}}),
        )
        .await;
    harness
        .key_with_permissions(
            "alias",
            "octra-governance-key-0001",
            json!({"allowed_models": ["octra-default"]}),
        )
        .await;

    let (status, _, body) = post_json(
        harness.data_router(),
        COMPLETION_PATH,
        &[(KEY_HEADER, KEY_VALUE)],
        openai_body("octra-default", false),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "body: {body}");
    assert_eq!(body["error"]["code"], "model_forbidden");
    assert_eq!(upstream.request_count(), 0);
}

#[tokio::test]
async fn virtual_key_provider_allow_list_routes_to_allowed_provider() {
    let harness = Harness::new().await;
    let upstream_a = MockUpstream::start(&harness.config, openai_upstream_text).await;
    let upstream_b = MockUpstream::start(&harness.config, openai_upstream_text).await;
    let provider_a = harness
        .provider("provider-a", "openai", &upstream_a.base_url, "bearer", 0)
        .await;
    let provider_b = harness
        .provider("provider-b", "openai", &upstream_b.base_url, "bearer", 0)
        .await;
    harness
        .route_to(
            "octra-default",
            &[(provider_a, 0, 1), (provider_b.clone(), 1, 1)],
        )
        .await;
    harness
        .key_with_permissions(
            "providers",
            "octra-governance-key-0001",
            json!({"allowed_providers": [provider_b]}),
        )
        .await;

    let (status, _, body) = post_json(
        harness.data_router(),
        COMPLETION_PATH,
        &[(KEY_HEADER, KEY_VALUE)],
        openai_body("octra-default", false),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(upstream_a.request_count(), 0);
    assert_eq!(upstream_b.request_count(), 1);
}

#[tokio::test]
async fn virtual_key_provider_allow_list_rejects_disallowed_provider() {
    let (harness, upstream) = openai_harness().await;
    harness
        .key_with_permissions(
            "providers",
            "octra-governance-key-0001",
            json!({"allowed_providers": ["missing-provider"]}),
        )
        .await;

    let (status, _, body) = post_json(
        harness.data_router(),
        COMPLETION_PATH,
        &[(KEY_HEADER, KEY_VALUE)],
        openai_body("octra-default", false),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "body: {body}");
    assert_eq!(body["error"]["code"], "provider_forbidden");
    assert_eq!(upstream.request_count(), 0);
}

#[tokio::test]
async fn virtual_key_account_allow_list_selects_allowed_account() {
    let harness = Harness::new().await;
    let upstream = MockUpstream::start(&harness.config, openai_upstream_text).await;
    let provider = harness
        .provider("account-mock", "openai", &upstream.base_url, "bearer", 0)
        .await;
    harness
        .route_to("octra-default", &[(provider.clone(), 0, 1)])
        .await;
    harness
        .account_with_config(&provider, "account-a", 0, 1, "{}")
        .await;
    let account_b = harness
        .account_with_config(&provider, "account-b", 1, 1, "{}")
        .await;
    harness
        .key_with_permissions(
            "accounts",
            "octra-governance-key-0001",
            json!({"allowed_accounts": [account_b]}),
        )
        .await;

    let (status, _, body) = post_json(
        harness.data_router(),
        COMPLETION_PATH,
        &[(KEY_HEADER, KEY_VALUE)],
        openai_body("octra-default", false),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let usage = wait_for_usage(&harness.storage, 1).await;
    assert_eq!(
        usage[0].account_name.as_deref(),
        Some("account-b"),
        "usage: {usage:?}"
    );
}

#[tokio::test]
async fn virtual_key_account_allow_list_rejects_unknown_account() {
    let (harness, upstream) = openai_harness().await;
    harness
        .key_with_permissions(
            "accounts",
            "octra-governance-key-0001",
            json!({"allowed_accounts": ["missing-account"]}),
        )
        .await;

    let (status, _, body) = post_json(
        harness.data_router(),
        COMPLETION_PATH,
        &[(KEY_HEADER, KEY_VALUE)],
        openai_body("octra-default", false),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "body: {body}");
    assert_eq!(body["error"]["code"], "account_forbidden");
    assert_eq!(upstream.request_count(), 0);
}

#[tokio::test]
async fn virtual_key_token_limit_rejects_before_upstream() {
    let (harness, upstream) = openai_harness().await;
    let key_id = harness
        .key_with_permissions(
            "limits",
            "octra-governance-key-0001",
            json!({"limits": [{"window": "day", "tokens": 10}]}),
        )
        .await;
    harness
        .storage
        .llm
        .insert_usage(&LlmUsageInput {
            request_id: "seed".into(),
            virtual_model_id: "octra-default".into(),
            api_key_id: Some(key_id),
            input_tokens: 10,
            ..Default::default()
        })
        .await
        .unwrap();

    let (status, _, body) = post_json(
        harness.data_router(),
        COMPLETION_PATH,
        &[(KEY_HEADER, KEY_VALUE)],
        openai_body("octra-default", false),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "body: {body}");
    assert_eq!(body["error"]["code"], "virtual_key_limit_exceeded");
    assert_eq!(upstream.request_count(), 0);
}

#[tokio::test]
async fn virtual_key_cost_limit_rejects_before_upstream() {
    let (harness, upstream) = openai_harness().await;
    let key_id = harness
        .key_with_permissions(
            "limits",
            "octra-governance-key-0001",
            json!({"limits": [{"window": "day", "cost_micros": 100}]}),
        )
        .await;
    harness
        .storage
        .llm
        .insert_usage(&LlmUsageInput {
            request_id: "seed".into(),
            virtual_model_id: "octra-default".into(),
            api_key_id: Some(key_id),
            cost_micros: Some(100),
            ..Default::default()
        })
        .await
        .unwrap();

    let (status, _, body) = post_json(
        harness.data_router(),
        COMPLETION_PATH,
        &[(KEY_HEADER, KEY_VALUE)],
        openai_body("octra-default", false),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "body: {body}");
    assert_eq!(body["error"]["code"], "virtual_key_limit_exceeded");
    assert_eq!(upstream.request_count(), 0);
}

#[tokio::test]
async fn virtual_key_limit_allows_usage_below_the_limit() {
    let (harness, upstream) = openai_harness().await;
    let key_id = harness
        .key_with_permissions(
            "limits",
            "octra-governance-key-0001",
            json!({"limits": [{"window": "day", "tokens": 10}]}),
        )
        .await;
    harness
        .storage
        .llm
        .insert_usage(&LlmUsageInput {
            request_id: "seed".into(),
            virtual_model_id: "octra-default".into(),
            api_key_id: Some(key_id),
            input_tokens: 9,
            ..Default::default()
        })
        .await
        .unwrap();

    let (status, _, body) = post_json(
        harness.data_router(),
        COMPLETION_PATH,
        &[(KEY_HEADER, KEY_VALUE)],
        openai_body("octra-default", false),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(upstream.request_count(), 1);
}

#[tokio::test]
async fn virtual_key_permissions_combine_with_consumer_policy() {
    let (harness, upstream) = openai_harness().await;
    let consumer = harness
        .storage
        .api
        .create_consumer(&CreateConsumer {
            username: Some("governed-team".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    harness
        .storage
        .api
        .create_policy(&CreatePolicy {
            name: "allow-octra-default".into(),
            consumer_id: Some(consumer.id.clone()),
            document: Some(json!({"allowed_models": ["octra-default"]})),
            ..Default::default()
        })
        .await
        .unwrap();
    harness
        .storage
        .llm
        .create_virtual_key(&LlmVirtualKeyInput {
            name: "consumer-key".into(),
            key_prefix: "octra-govern".into(),
            key_hash: hash_key("octra-governance-key-0001"),
            consumer_id: Some(consumer.id),
            enabled: true,
            expires_at: None,
            permissions_json: json!({"allowed_models": ["other-model"]}).to_string(),
        })
        .await
        .unwrap();
    harness.runtime.reload().await.unwrap();

    let (status, _, body) = post_json(
        harness.data_router(),
        COMPLETION_PATH,
        &[(KEY_HEADER, KEY_VALUE)],
        openai_body("octra-default", false),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "body: {body}");
    assert_eq!(body["error"]["code"], "model_forbidden");
    assert_eq!(upstream.request_count(), 0);
}

#[tokio::test]
async fn invalid_virtual_key_permissions_fail_closed() {
    let (harness, upstream) = openai_harness().await;
    harness
        .storage
        .llm
        .create_virtual_key(&LlmVirtualKeyInput {
            name: "broken".into(),
            key_prefix: "octra-govern".into(),
            key_hash: hash_key("octra-governance-key-0001"),
            consumer_id: None,
            enabled: true,
            expires_at: None,
            permissions_json: "{not json".into(),
        })
        .await
        .unwrap();
    harness.runtime.reload().await.unwrap();

    let (status, _, body) = post_json(
        harness.data_router(),
        COMPLETION_PATH,
        &[(KEY_HEADER, KEY_VALUE)],
        openai_body("octra-default", false),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "body: {body}");
    assert_eq!(upstream.request_count(), 0);
}

#[tokio::test]
async fn models_listing_respects_virtual_key_permissions() {
    let (harness, _upstream) = openai_harness().await;
    harness
        .key_with_permissions(
            "visible",
            "octra-governance-key-0001",
            json!({"allowed_models": ["octra-default"]}),
        )
        .await;

    let (status, _, text) = send(
        harness.data_router(),
        axum::http::Method::GET,
        "/v1/models",
        &[(KEY_HEADER, KEY_VALUE)],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {text}");
    let body: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert!(
        body["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|model| model["id"] == "octra-default")
    );

    harness
        .key_with_permissions(
            "hidden",
            "octra-governance-key-0002",
            json!({"allowed_models": ["other-model"]}),
        )
        .await;
    let (status, _, text) = send(
        harness.data_router(),
        axum::http::Method::GET,
        "/v1/models",
        &[(KEY_HEADER, "Bearer octra-governance-key-0002")],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {text}");
    let body: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert!(
        body["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|model| model["id"] != "octra-default")
    );
}
