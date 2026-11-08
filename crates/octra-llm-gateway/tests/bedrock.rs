//! End-to-end Bedrock Converse path: OpenAI ingress, Converse upstream.

mod common;

use axum::http::StatusCode;
use common::*;
use serde_json::json;

fn bedrock_upstream(_request: Captured) -> axum::response::Response {
    json_response(json!({
        "output": {
            "message": {
                "role": "assistant",
                "content": [{"text": "Hello from Bedrock"}],
            }
        },
        "stopReason": "end_turn",
        "usage": {
            "inputTokens": 12,
            "outputTokens": 4,
            "totalTokens": 16,
        },
    }))
}

#[tokio::test]
async fn openai_ingress_to_bedrock_upstream() {
    let harness = Harness::new().await;
    let upstream = MockUpstream::start(&harness.config, bedrock_upstream).await;
    let provider = harness
        .provider("bedrock-mock", "bedrock", &upstream.base_url, "bearer", 0)
        .await;
    harness.route_to("octra-bedrock", &[(provider, 0, 1)]).await;

    let (status, _, body) = post_json(
        harness.data_router(),
        COMPLETION_PATH,
        &[],
        openai_body("octra-bedrock", false),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(
        body["choices"][0]["message"]["content"],
        "Hello from Bedrock"
    );
    // The client-facing model is restored from the requested virtual model.
    assert_eq!(body["model"], "octra-bedrock");
    assert_eq!(body["usage"]["prompt_tokens"], 12);
    assert_eq!(body["usage"]["completion_tokens"], 4);

    let request = &upstream.requests()[0];
    assert_eq!(request.path, "/v1/model/m/converse");
    assert_eq!(request.header("authorization"), Some("Bearer test-key"));
    // The model travels in the URL, never in the Converse body.
    assert!(request.body.get("model").is_none());
    assert_eq!(request.body["messages"][0]["content"][0]["text"], "hello");
}

#[tokio::test]
async fn openai_streaming_ingress_to_bedrock_upstream() {
    let harness = Harness::new().await;
    let upstream = MockUpstream::start(&harness.config, |_request| {
        event_stream_response(bedrock_stream_frames())
    })
    .await;
    let provider = harness
        .provider("bedrock-mock", "bedrock", &upstream.base_url, "bearer", 0)
        .await;
    harness.route_to("octra-bedrock", &[(provider, 0, 1)]).await;

    let (status, headers, text) = send(
        harness.data_router(),
        axum::http::Method::POST,
        COMPLETION_PATH,
        &[],
        Some(openai_body("octra-bedrock", true)),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "text/event-stream");
    assert!(text.contains("Hello from Bedrock"), "stream: {text}");
    // The gateway fills the empty Bedrock model with the client-facing model.
    assert!(
        text.contains("\"model\":\"octra-bedrock\""),
        "stream: {text}"
    );
    assert!(text.trim_end().ends_with("data: [DONE]"));
    assert_eq!(upstream.requests()[0].path, "/v1/model/m/converse-stream");
}

fn event_stream_response(bytes: Vec<u8>) -> axum::response::Response {
    axum::response::Response::builder()
        .status(StatusCode::OK)
        .header(
            axum::http::header::CONTENT_TYPE,
            "application/vnd.amazon.eventstream",
        )
        .body(axum::body::Body::from(bytes))
        .unwrap()
}

fn bedrock_stream_frames() -> Vec<u8> {
    let mut out = Vec::new();
    out.extend(bedrock_frame("messageStart", r#"{"role":"assistant"}"#));
    out.extend(bedrock_frame(
        "contentBlockDelta",
        r#"{"contentBlockIndex":0,"delta":{"text":"Hello from Bedrock"}}"#,
    ));
    out.extend(bedrock_frame(
        "contentBlockStop",
        r#"{"contentBlockIndex":0}"#,
    ));
    out.extend(bedrock_frame("messageStop", r#"{"stopReason":"end_turn"}"#));
    out.extend(bedrock_frame(
        "metadata",
        r#"{"usage":{"inputTokens":12,"outputTokens":4,"totalTokens":16}}"#,
    ));
    out
}

fn bedrock_frame(event: &str, payload: &str) -> Vec<u8> {
    let mut headers = Vec::new();
    headers.push(":event-type".len() as u8);
    headers.extend_from_slice(b":event-type");
    headers.push(7);
    headers.extend_from_slice(&(event.len() as u16).to_be_bytes());
    headers.extend_from_slice(event.as_bytes());

    let total = 12 + headers.len() + payload.len() + 4;
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&(total as u32).to_be_bytes());
    out.extend_from_slice(&(headers.len() as u32).to_be_bytes());
    let prelude = crc32(&out);
    out.extend_from_slice(&prelude.to_be_bytes());
    out.extend_from_slice(&headers);
    out.extend_from_slice(payload.as_bytes());
    let message = crc32(&out);
    out.extend_from_slice(&message.to_be_bytes());
    out
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}
