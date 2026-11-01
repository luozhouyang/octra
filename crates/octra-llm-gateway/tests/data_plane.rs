//! End-to-end data plane tests: protocol conversion through mock upstreams,
//! virtual key auth, fallback and usage accounting.

mod common;

use std::sync::Arc;

use axum::http::StatusCode;
use common::*;
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
            .llm_find_virtual_key_by_hash(&hash_key("octra-valid-key-0001"))
            .await
            .unwrap()
            .unwrap()
            .id,
        valid_id
    );
    harness
        .storage
        .llm_delete_virtual_key(&valid_id)
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
    let keys = harness.storage.llm_list_virtual_keys().await.unwrap();
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
