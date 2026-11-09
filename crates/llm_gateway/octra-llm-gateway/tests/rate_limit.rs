//! End-to-end coverage for the `llm-rate-limit` plugin.

mod common;

use axum::http::StatusCode;
use common::*;
use octra_api_core::{CreateConsumer, CreateConsumerGroup};
use octra_llm_gateway::keys::hash_key;
use octra_storage::LlmVirtualKeyInput;
use serde_json::json;

fn openai_upstream_text(request: Captured) -> axum::response::Response {
    if request.body.get("stream").and_then(|v| v.as_bool()) == Some(true) {
        sse_response(openai_text_stream("Hello from OpenAI"))
    } else {
        json_response(openai_completion("Hello from OpenAI"))
    }
}

async fn harness_with_route() -> (Harness, MockUpstream) {
    let harness = Harness::new().await;
    let upstream = MockUpstream::start(&harness.config, openai_upstream_text).await;
    let provider = harness
        .provider("rate-mock", "openai", &upstream.base_url, "bearer", 0)
        .await;
    harness.route_to("octra-default", &[(provider, 0, 1)]).await;
    (harness, upstream)
}

async fn create_key(
    harness: &Harness,
    name: &str,
    plaintext: &str,
    consumer_id: Option<String>,
    family: Option<String>,
) -> String {
    let record = harness
        .storage
        .llm
        .create_virtual_key(&LlmVirtualKeyInput {
            name: name.to_string(),
            key_prefix: plaintext.chars().take(12).collect(),
            key_hash: hash_key(plaintext),
            consumer_id,
            enabled: true,
            expires_at: None,
            permissions_json: "{}".to_string(),
            key_family: family,
        })
        .await
        .unwrap();
    harness.runtime.reload().await.unwrap();
    record.id
}

async fn create_consumer(harness: &Harness, name: &str) -> String {
    harness
        .storage
        .api
        .create_consumer(&CreateConsumer {
            username: Some(name.to_string()),
            ..CreateConsumer::default()
        })
        .await
        .unwrap()
        .id
}

async fn create_group(harness: &Harness, name: &str) -> String {
    harness
        .storage
        .api
        .create_consumer_group(&CreateConsumerGroup {
            name: name.to_string(),
            ..CreateConsumerGroup::default()
        })
        .await
        .unwrap()
        .id
}

#[tokio::test]
async fn reports_limits_and_rejects_without_calling_upstream() {
    let (harness, upstream) = harness_with_route().await;
    harness
        .plugin_config(
            "llm-rate-limit",
            "global",
            None,
            json!({"minute": 1, "limit_by": "global"}),
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
    assert_eq!(first.1.get("X-RateLimit-Limit-Minute").unwrap(), "1");
    assert_eq!(first.1.get("X-RateLimit-Remaining-Minute").unwrap(), "0");
    assert_eq!(first.1.get("RateLimit-Limit").unwrap(), "1");
    assert_eq!(first.1.get("RateLimit-Remaining").unwrap(), "0");
    assert!(first.1.contains_key("RateLimit-Reset"));

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
    assert!(second.1.contains_key("Retry-After"));
    assert_eq!(second.2["error"]["code"], "rate_limited");
    assert_eq!(
        upstream.request_count(),
        1,
        "rejected call must not reach upstream"
    );
}

#[tokio::test]
async fn rotated_keys_share_the_family_counter() {
    let (harness, _upstream) = harness_with_route().await;
    let family = "family-a".to_string();
    let first = create_key(&harness, "k1", "secret-a", None, Some(family.clone())).await;
    let second = create_key(&harness, "k2", "secret-b", None, Some(family)).await;
    assert_ne!(first, second);

    harness
        .plugin_config(
            "llm-rate-limit",
            "global",
            None,
            json!({"minute": 2, "limit_by": "virtual_key"}),
        )
        .await;

    let mut statuses = Vec::new();
    for secret in ["secret-a", "secret-b", "secret-a"] {
        let response = post_json(
            harness.data_router(),
            COMPLETION_PATH,
            &[("authorization", &format!("Bearer {secret}"))],
            openai_body("octra-default", false),
        )
        .await;
        statuses.push(response.0);
    }
    assert_eq!(
        statuses,
        vec![
            StatusCode::OK,
            StatusCode::OK,
            StatusCode::TOO_MANY_REQUESTS
        ]
    );
}

#[tokio::test]
async fn consumer_group_limit_is_shared_by_its_members() {
    let (harness, _upstream) = harness_with_route().await;
    let group = create_group(&harness, "team-a").await;
    let first_consumer = create_consumer(&harness, "alice").await;
    let second_consumer = create_consumer(&harness, "bob").await;
    harness
        .storage
        .api
        .add_consumer_group_member(&group, &first_consumer)
        .await
        .unwrap();
    harness
        .storage
        .api
        .add_consumer_group_member(&group, &second_consumer)
        .await
        .unwrap();
    create_key(
        &harness,
        "alice-key",
        "alice-secret",
        Some(first_consumer),
        None,
    )
    .await;
    create_key(
        &harness,
        "bob-key",
        "bob-secret",
        Some(second_consumer),
        None,
    )
    .await;
    harness.runtime.reload().await.unwrap();

    harness
        .plugin_config(
            "llm-rate-limit",
            "global",
            None,
            json!({"rules": [
                {"name": "group-minute", "window": "minute", "limit": 2, "limit_by": "consumer_group"}
            ]}),
        )
        .await;

    let mut statuses = Vec::new();
    for secret in ["alice-secret", "bob-secret", "alice-secret"] {
        let response = post_json(
            harness.data_router(),
            COMPLETION_PATH,
            &[("authorization", &format!("Bearer {secret}"))],
            openai_body("octra-default", false),
        )
        .await;
        statuses.push(response.0);
    }
    assert_eq!(
        statuses,
        vec![
            StatusCode::OK,
            StatusCode::OK,
            StatusCode::TOO_MANY_REQUESTS
        ]
    );
}

#[tokio::test]
async fn streaming_responses_carry_headers_and_respect_the_limit() {
    let (harness, _upstream) = harness_with_route().await;
    harness
        .plugin_config(
            "llm-rate-limit",
            "global",
            None,
            json!({"minute": 1, "limit_by": "global"}),
        )
        .await;

    let first = post_json(
        harness.data_router(),
        COMPLETION_PATH,
        &[],
        openai_body("octra-default", true),
    )
    .await;
    assert_eq!(first.0, StatusCode::OK, "body: {}", first.2);
    assert_eq!(first.1.get("X-RateLimit-Limit-Minute").unwrap(), "1");
    assert!(first.1.contains_key("RateLimit-Remaining"));

    let second = post_json(
        harness.data_router(),
        COMPLETION_PATH,
        &[],
        openai_body("octra-default", true),
    )
    .await;
    assert_eq!(
        second.0,
        StatusCode::TOO_MANY_REQUESTS,
        "body: {}",
        second.2
    );
    assert!(second.1.contains_key("Retry-After"));
}

#[tokio::test]
async fn hidden_headers_are_omitted_on_success_and_rejection() {
    let (harness, _upstream) = harness_with_route().await;
    harness
        .plugin_config(
            "llm-rate-limit",
            "global",
            None,
            json!({"minute": 1, "limit_by": "global", "hide_client_headers": true}),
        )
        .await;

    let first = post_json(
        harness.data_router(),
        COMPLETION_PATH,
        &[],
        openai_body("octra-default", false),
    )
    .await;
    assert_eq!(first.0, StatusCode::OK);
    assert!(!first.1.contains_key("X-RateLimit-Limit-Minute"));
    assert!(!first.1.contains_key("RateLimit-Limit"));

    let second = post_json(
        harness.data_router(),
        COMPLETION_PATH,
        &[],
        openai_body("octra-default", false),
    )
    .await;
    assert_eq!(second.0, StatusCode::TOO_MANY_REQUESTS);
    assert!(!second.1.contains_key("Retry-After"));
    assert!(!second.1.contains_key("RateLimit-Remaining"));
}

#[tokio::test]
async fn rate_limit_runs_before_the_budget_guard() {
    let (harness, _upstream) = harness_with_route().await;
    harness
        .plugin_config(
            "llm-rate-limit",
            "global",
            None,
            json!({"minute": 10, "limit_by": "global"}),
        )
        .await;
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
    assert_eq!(first.0, StatusCode::OK);

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
    assert!(second.1.contains_key("X-RateLimit-Limit-Minute"));
}

#[test]
fn agent_example_json_files_are_valid() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for path in [
        "examples/agents/claude-code/settings.json",
        "examples/agents/opencode/opencode.json",
    ] {
        let text = std::fs::read_to_string(root.join(path)).unwrap();
        serde_json::from_str::<serde_json::Value>(&text)
            .unwrap_or_else(|error| panic!("{path}: {error}"));
    }
}
