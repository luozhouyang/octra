//! Data plane behaviour against a mock Streamable HTTP MCP upstream.

mod support;

use std::time::Duration;

use axum::body::Body;
use axum::http::StatusCode;
use futures_util::StreamExt;
use octra_config::OctraConfig;
use octra_data_plane::sse::{SseDecoder, SseEvent};
use octra_mcp_core::domain::default_server_mode;
use octra_mcp_core::{NewMcpServer, Transport};
use serde_json::json;
use support::{
    Gateway, Upstream, UpstreamMode, create_consumer_group, create_key, create_keyed_consumer,
    sign_test_token, start_api_mock, start_discovery, start_introspection, start_jwks,
    start_upstream, test_jwks,
};

struct SseStream {
    stream: axum::body::BodyDataStream,
    decoder: SseDecoder,
    pending: std::collections::VecDeque<SseEvent>,
}

impl SseStream {
    async fn next_event(&mut self, timeout: Duration) -> Option<SseEvent> {
        if let Some(event) = self.pending.pop_front() {
            return Some(event);
        }
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            match tokio::time::timeout(remaining, self.stream.next()).await {
                Ok(Some(Ok(chunk))) => {
                    self.pending.extend(self.decoder.push(&chunk));
                    if let Some(event) = self.pending.pop_front() {
                        return Some(event);
                    }
                }
                _ => return None,
            }
        }
    }

    async fn expect_event(&mut self, timeout: Duration) -> SseEvent {
        self.next_event(timeout)
            .await
            .expect("expected an SSE event")
    }

    /// Returns true when the body ends before any further event arrives.
    async fn body_ended(&mut self, timeout: Duration) -> bool {
        if !self.pending.is_empty() {
            return false;
        }
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return false;
            }
            match tokio::time::timeout(remaining, self.stream.next()).await {
                Ok(Some(Ok(chunk))) => {
                    self.pending.extend(self.decoder.push(&chunk));
                    if !self.pending.is_empty() {
                        return false;
                    }
                }
                Ok(Some(Err(_))) | Ok(None) => return true,
                Err(_) => return false,
            }
        }
    }
}

async fn open_sse(gateway: &Gateway, path: &str, session: Option<&str>) -> SseStream {
    let mut builder = axum::http::Request::builder().method("GET").uri(path);
    if let Some(session) = session {
        builder = builder.header("mcp-session-id", session);
    }
    let response =
        tower::ServiceExt::oneshot(gateway.data_router(), builder.body(Body::empty()).unwrap())
            .await
            .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    SseStream {
        stream: response.into_body().into_data_stream(),
        decoder: SseDecoder::new(),
        pending: std::collections::VecDeque::new(),
    }
}

async fn wait_for_active_get(upstream: &Upstream, minimum: u64, timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if upstream.state.active_get_streams() >= minimum {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!(
        "upstream GET streams did not reach {minimum}: {}",
        upstream.state.active_get_streams()
    );
}

async fn wait_for_idle_get(upstream: &Upstream, timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if upstream.state.active_get_streams() == 0 {
            // Give a connection still completing its handshake time to settle.
            tokio::time::sleep(Duration::from_millis(100)).await;
            if upstream.state.active_get_streams() == 0 {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!(
        "upstream GET streams did not become idle: {}",
        upstream.state.active_get_streams()
    );
}

async fn wait_for_tool_lists(upstream: &Upstream, minimum: u64, timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if upstream.state.tool_lists() >= minimum {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!(
        "upstream tools/list calls did not reach {minimum}: {}",
        upstream.state.tool_lists()
    );
}

fn tool_names(body: &serde_json::Value) -> Vec<String> {
    body["result"]["tools"]
        .as_array()
        .expect("tools/list result")
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn initialize_lists_and_calls_over_json_upstream() {
    let upstream = start_upstream(UpstreamMode::Json).await;
    let gateway = Gateway::new().await;
    let server_id = gateway.add_http_server("remote", &upstream).await;

    let session = gateway.open_session(None).await;

    // Notifications are accepted with 202 and no body.
    let (status, _, body) = gateway
        .post_mcp_with_content_type(
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            None,
            None,
            "application/json",
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body, serde_json::Value::Null);

    let (status, body) = gateway
        .rpc(&session, json!(2), "tools/list", json!({}), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(tool_names(&body), vec!["remote__boom", "remote__echo"]);
    assert_eq!(
        body["result"]["tools"][1]["description"],
        "Echoes the message argument"
    );

    let (status, body) = gateway
        .rpc(
            &session,
            json!(3),
            "tools/call",
            json!({"name": "remote__echo", "arguments": {"message": "hello"}}),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["result"]["content"][0]["text"], "hello");
    assert_eq!(
        *upstream.state.calls.lock().unwrap(),
        vec!["echo".to_string()]
    );

    // Upstream JSON-RPC errors are forwarded with their code intact.
    let (status, body) = gateway
        .rpc(
            &session,
            json!(4),
            "tools/call",
            json!({"name": "remote__boom"}),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["error"]["code"], -32000);

    // Ping answers with an empty result.
    let (status, body) = gateway
        .rpc(&session, json!(5), "ping", json!({}), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["result"], json!({}));

    // Invocations are persisted, one per call.
    let invocations = gateway
        .storage
        .mcp
        .list_invocations(10, Some(&server_id))
        .await
        .unwrap();
    assert_eq!(invocations.len(), 2);
    assert_eq!(invocations[0].status, "error");
    assert_eq!(invocations[1].status, "ok");
    assert!(invocations[1].duration_ms >= 0);
    assert!(invocations[1].finished_at.is_some());
    assert!(!invocations[1].request_id.is_empty());
}

#[tokio::test]
async fn sse_upstream_responses_are_decoded() {
    let upstream = start_upstream(UpstreamMode::Sse).await;
    let gateway = Gateway::new().await;
    gateway.add_http_server("stream", &upstream).await;

    let session = gateway.open_session(None).await;
    let (status, body) = gateway
        .rpc(&session, json!(2), "tools/list", json!({}), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(tool_names(&body), vec!["stream__boom", "stream__echo"]);

    let (status, body) = gateway
        .rpc(
            &session,
            json!(3),
            "tools/call",
            json!({"name": "stream__echo", "arguments": {"message": "sse"}}),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["result"]["content"][0]["text"], "sse");
}

#[tokio::test]
async fn unknown_tools_and_methods_are_reported() {
    let upstream = start_upstream(UpstreamMode::Json).await;
    let gateway = Gateway::new().await;
    gateway.add_http_server("remote", &upstream).await;
    let session = gateway.open_session(None).await;

    let (status, body) = gateway
        .rpc(
            &session,
            json!(2),
            "tools/call",
            json!({"name": "remote__missing"}),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["error"]["code"], -32002);

    let (_, body) = gateway
        .rpc(
            &session,
            json!(3),
            "tools/call",
            json!({"name": "unqualified"}),
            None,
        )
        .await;
    assert_eq!(body["error"]["code"], -32002);

    let (_, body) = gateway
        .rpc(
            &session,
            json!(4),
            "tools/call",
            json!({"name": "nobody__echo"}),
            None,
        )
        .await;
    assert_eq!(body["error"]["code"], -32002);

    let (_, body) = gateway
        .rpc(&session, json!(5), "resources/list", json!({}), None)
        .await;
    assert_eq!(body["error"]["code"], -32601);

    // Parse errors and unknown sessions use HTTP status codes too.
    let (status, _, body) = gateway
        .post_mcp(json!({"not": "json-rpc"}), None, None)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], -32600);

    let (status, _, _) = gateway
        .post_mcp(
            json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}),
            None,
            Some("no-such-session"),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, body) = gateway
        .rpc("no-such-session", json!(1), "tools/list", json!({}), None)
        .await;
    assert_eq!(body["error"]["code"], -32001);
}

#[tokio::test]
async fn disabled_servers_and_tools_are_hidden() {
    let upstream = start_upstream(UpstreamMode::Json).await;
    let other = start_upstream(UpstreamMode::Json).await;
    let gateway = Gateway::new().await;
    let first = gateway.add_http_server("first", &upstream).await;
    gateway.add_http_server("second", &other).await;
    let session = gateway.open_session(None).await;

    // Disable one tool of the first server through the control plane.
    let (_, tools) = gateway
        .control(
            "GET",
            &format!("/api/v1/mcp/tools?server_id={first}"),
            json!(null),
        )
        .await;
    let echo_id = tools
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "echo")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let (status, body) = gateway
        .control(
            "PUT",
            &format!("/api/v1/mcp/tools/{echo_id}"),
            json!({"enabled": false}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["enabled"], false);

    let (_, body) = gateway
        .rpc(&session, json!(2), "tools/list", json!({}), None)
        .await;
    let names = tool_names(&body);
    assert!(!names.contains(&"first__echo".to_string()));
    assert!(names.contains(&"second__echo".to_string()));

    // The disabled server disappears entirely.
    let (status, _) = gateway
        .control(
            "PUT",
            &format!("/api/v1/mcp/servers/{first}"),
            json!({"name": "first", "transport": "http", "url": upstream.url(), "enabled": false}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (_, body) = gateway
        .rpc(&session, json!(3), "tools/list", json!({}), None)
        .await;
    assert!(
        !tool_names(&body)
            .iter()
            .any(|name| name.starts_with("first__"))
    );

    let (_, body) = gateway
        .rpc(
            &session,
            json!(4),
            "tools/call",
            json!({"name": "first__boom"}),
            None,
        )
        .await;
    assert_eq!(body["error"]["code"], -32002);
}

#[tokio::test]
async fn delete_terminates_the_session_and_get_streams_sse() {
    let upstream = start_upstream(UpstreamMode::Json).await;
    let gateway = Gateway::new().await;
    gateway.add_http_server("remote", &upstream).await;
    let session = gateway.open_session(None).await;

    // GET with a session yields an SSE stream.
    let request = axum::http::Request::builder()
        .method("GET")
        .uri("/mcp")
        .header("mcp-session-id", &session)
        .body(axum::body::Body::empty())
        .unwrap();
    let response = tower::ServiceExt::oneshot(gateway.data_router(), request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    drop(response);

    // DELETE terminates; the same session then fails validation.
    let request = axum::http::Request::builder()
        .method("DELETE")
        .uri("/mcp")
        .header("mcp-session-id", &session)
        .body(axum::body::Body::empty())
        .unwrap();
    let response = tower::ServiceExt::oneshot(gateway.data_router(), request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let (_, body) = gateway
        .rpc(&session, json!(2), "tools/list", json!({}), None)
        .await;
    assert_eq!(body["error"]["code"], -32001);
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("session not found")
    );
}

#[tokio::test]
async fn expired_sessions_must_reinitialize() {
    let mut config = OctraConfig::default();
    config.mcp.session_ttl_secs = 1;
    let upstream = start_upstream(UpstreamMode::Json).await;
    let gateway = Gateway::new_with_config(config).await;
    gateway.add_http_server("first", &upstream).await;
    let session = gateway.open_session(None).await;

    tokio::time::sleep(std::time::Duration::from_millis(1_200)).await;
    let (status, body) = gateway
        .rpc(&session, json!(1), "ping", json!({}), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], -32001);
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("session not found")
    );
}

#[tokio::test]
async fn sessions_are_bound_to_their_scoped_endpoint() {
    let upstream = start_upstream(UpstreamMode::Json).await;
    let other = start_upstream(UpstreamMode::Json).await;
    let gateway = Gateway::new().await;
    gateway.add_http_server("first", &upstream).await;
    gateway.add_http_server("second", &other).await;
    let session = gateway.open_session_at("/mcp/first", None).await;

    let (status, body) = gateway
        .rpc_at("/mcp/second", &session, json!(1), "ping", json!({}), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], -32001);

    let (status, body) = gateway
        .rpc_at("/mcp/first", &session, json!(2), "ping", json!({}), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn rejects_unsupported_protocol_version_headers() {
    let upstream = start_upstream(UpstreamMode::Json).await;
    let gateway = Gateway::new().await;
    gateway.add_http_server("first", &upstream).await;
    let session = gateway.open_session(None).await;

    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "ping"});
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("content-type", "application/json")
        .header("mcp-session-id", &session)
        .header("mcp-protocol-version", "1900-01-01")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = tower::ServiceExt::oneshot(gateway.data_router(), request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = http_body_util::BodyExt::collect(response.into_body())
        .await
        .unwrap()
        .to_bytes();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["error"]["code"], -32600);
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unsupported MCP-Protocol-Version")
    );
}

#[tokio::test]
async fn key_auth_gates_the_endpoint_and_resolves_consumers() {
    let upstream = start_upstream(UpstreamMode::Json).await;
    let gateway = Gateway::new().await;
    gateway.add_http_server("remote", &upstream).await;

    let key = create_key(&gateway, "agent").await;
    assert!(
        key.starts_with("octra_"),
        "keys are prefixed for recognisability"
    );

    // Without a key the gateway now refuses.
    let (status, _, _) = gateway
        .post_mcp(
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18"}}),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, _, _) = gateway
        .post_mcp(
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18"}}),
            Some("octra_wrong"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let session = gateway.open_session(Some(&key)).await;
    let (status, body) = gateway
        .rpc(
            &session,
            json!(2),
            "tools/call",
            json!({"name": "remote__echo", "arguments": {"message": "keyed"}}),
            Some(&key),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["result"]["content"][0]["text"], "keyed");

    // The plaintext never reaches the database; only a 12-char prefix and the
    // SHA-256 hex digest are stored, and the digest resolves the key.
    let stored = gateway.storage.mcp.list_api_keys().await.unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].key_prefix, &key[..12]);
    assert!(
        gateway
            .storage
            .mcp
            .find_active_api_key(&octra_mcp_gateway::auth::hash_key(&key))
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        gateway
            .storage
            .mcp
            .find_active_api_key(&key)
            .await
            .unwrap()
            .is_none(),
        "the raw key must never be a stored hash"
    );
}

#[tokio::test]
async fn per_tool_acl_restricts_calls_to_allowed_consumers() {
    let upstream = start_upstream(UpstreamMode::Json).await;
    let gateway = Gateway::new().await;
    let server_id = gateway.add_http_server("remote", &upstream).await;

    // Two consumers: agent-a is allowed echo, agent-b only exists.
    let (_, key_a) = gateway
        .control(
            "POST",
            "/api/v1/mcp/keys",
            json!({"name": "agent-a", "consumer_name": "agent-a"}),
        )
        .await;
    let key_a = key_a["key_value"].as_str().unwrap().to_string();
    let consumer_a = key_a_consumer(&gateway, "agent-a").await;
    let (_, key_b) = gateway
        .control(
            "POST",
            "/api/v1/mcp/keys",
            json!({"name": "agent-b", "consumer_name": "agent-b"}),
        )
        .await;
    let key_b = key_b["key_value"].as_str().unwrap().to_string();

    let (_, tools) = gateway
        .control(
            "GET",
            &format!("/api/v1/mcp/tools?server_id={server_id}"),
            json!(null),
        )
        .await;
    let echo_id = tools
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "echo")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    // Any ACL row on `echo` switches it from open to rule-based.
    let (status, acl) = gateway
        .control(
            "POST",
            "/api/v1/mcp/acls",
            json!({"tool_id": echo_id, "consumer_id": consumer_a, "allowed": true}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{acl}");

    let session_a = gateway.open_session(Some(&key_a)).await;
    let (_, body) = gateway
        .rpc(
            &session_a,
            json!(2),
            "tools/call",
            json!({"name": "remote__echo", "arguments": {"message": "a"}}),
            Some(&key_a),
        )
        .await;
    assert_eq!(body["result"]["content"][0]["text"], "a");

    // tools/list hides echo from the disallowed consumer but keeps boom
    // (no ACL rows, so it follows the open default).
    let session_b = gateway.open_session(Some(&key_b)).await;
    let (_, body) = gateway
        .rpc(&session_b, json!(3), "tools/list", json!({}), Some(&key_b))
        .await;
    let names = tool_names(&body);
    assert!(!names.contains(&"remote__echo".to_string()));
    assert!(names.contains(&"remote__boom".to_string()));

    let (_, body) = gateway
        .rpc(
            &session_b,
            json!(4),
            "tools/call",
            json!({"name": "remote__echo"}),
            Some(&key_b),
        )
        .await;
    assert_eq!(body["error"]["code"], -32005);

    // Denials are audited.
    let invocations = gateway.storage.mcp.list_invocations(5, None).await.unwrap();
    let denied = invocations.iter().find(|row| row.status == "denied");
    assert!(
        denied.is_some(),
        "acl denial must be recorded: {invocations:?}"
    );
    assert!(denied.unwrap().consumer_id.is_some());
}

async fn key_a_consumer(gateway: &Gateway, name: &str) -> String {
    let (_, keys) = gateway
        .control("GET", "/api/v1/mcp/keys", json!(null))
        .await;
    keys.as_array()
        .unwrap()
        .iter()
        .find(|key| key["name"] == name)
        .unwrap()["consumer_id"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn find_tool_id(gateway: &Gateway, server_id: &str, name: &str) -> String {
    let (_, tools) = gateway
        .control(
            "GET",
            &format!("/api/v1/mcp/tools?server_id={server_id}"),
            json!(null),
        )
        .await;
    tools
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == name)
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn set_acl(gateway: &Gateway, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
    gateway.control("POST", "/api/v1/mcp/acls", body).await
}

#[tokio::test]
async fn scoped_acl_control_plane_round_trips() {
    let upstream = start_upstream(UpstreamMode::Json).await;
    let gateway = Gateway::new().await;
    let server_id = gateway.add_http_server("remote", &upstream).await;
    let (_, consumer_id) = create_keyed_consumer(&gateway, "agent-a").await;
    let group_id = create_consumer_group(&gateway, "team", &[&consumer_id]).await;
    let echo_id = find_tool_id(&gateway, &server_id, "echo").await;

    // A rule cannot target a consumer and a group at the same time.
    let (status, body) = set_acl(
        &gateway,
        json!({
            "tool_id": echo_id,
            "consumer_id": consumer_id,
            "consumer_group_id": group_id,
            "allowed": true,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // Group ids are validated before the rule is written.
    let (status, body) = set_acl(
        &gateway,
        json!({
            "tool_id": echo_id,
            "consumer_group_id": "missing-group",
            "allowed": true,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unknown consumer_group_id")
    );

    let (status, group_acl) = set_acl(
        &gateway,
        json!({
            "tool_id": echo_id,
            "consumer_group_id": group_id,
            "allowed": true,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{group_acl}");
    assert!(group_acl["consumer_id"].is_null());
    assert_eq!(
        group_acl["consumer_group_id"].as_str(),
        Some(group_id.as_str())
    );

    // Omitting both ids creates the tool default.
    let (status, default_acl) =
        set_acl(&gateway, json!({"tool_id": echo_id, "allowed": false})).await;
    assert_eq!(status, StatusCode::CREATED, "{default_acl}");
    assert!(default_acl["consumer_id"].is_null());
    assert!(default_acl["consumer_group_id"].is_null());
    assert_eq!(default_acl["allowed"], false);

    let (status, listed) = gateway
        .control(
            "GET",
            &format!("/api/v1/mcp/acls?tool_id={echo_id}"),
            json!(null),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    let listed = listed.as_array().unwrap();
    assert_eq!(listed.len(), 2);
    assert!(listed.iter().any(|row| {
        row["consumer_group_id"].as_str() == Some(group_id.as_str())
            && row["consumer_id"].is_null()
            && row["allowed"] == true
    }));
    assert!(listed.iter().any(|row| {
        row["consumer_id"].is_null()
            && row["consumer_group_id"].is_null()
            && row["allowed"] == false
    }));

    let (status, single) = gateway
        .control(
            "GET",
            &format!("/api/v1/mcp/acls/{}", group_acl["id"].as_str().unwrap()),
            json!(null),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{single}");
    assert_eq!(
        single["consumer_group_id"].as_str(),
        Some(group_id.as_str())
    );

    // Re-posting the default updates the existing row instead of failing.
    let (status, updated_default) =
        set_acl(&gateway, json!({"tool_id": echo_id, "allowed": true})).await;
    assert_eq!(status, StatusCode::CREATED, "{updated_default}");
    assert_eq!(updated_default["id"], default_acl["id"]);
    assert_eq!(updated_default["allowed"], true);
    assert_eq!(
        gateway
            .control(
                "GET",
                &format!("/api/v1/mcp/acls?tool_id={echo_id}"),
                json!(null),
            )
            .await
            .1
            .as_array()
            .unwrap()
            .len(),
        2
    );

    // Updates keep the scope and only change `allowed`.
    let (status, updated) = gateway
        .control(
            "PUT",
            &format!("/api/v1/mcp/acls/{}", group_acl["id"].as_str().unwrap()),
            json!({"allowed": false}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["allowed"], false);
    assert_eq!(
        updated["consumer_group_id"].as_str(),
        Some(group_id.as_str())
    );
    assert!(updated["consumer_id"].is_null());
}

#[tokio::test]
async fn acl_default_deny_applies_to_anonymous_and_unscoped_consumers() {
    let upstream = start_upstream(UpstreamMode::Json).await;
    let gateway = Gateway::new().await;
    let server_id = gateway.add_http_server("remote", &upstream).await;

    // Without ACL rows the open-local default allows the call.
    let session = gateway.open_session(None).await;
    let (_, body) = gateway
        .rpc(
            &session,
            json!(2),
            "tools/call",
            json!({"name": "remote__echo", "arguments": {"message": "open"}}),
            None,
        )
        .await;
    assert_eq!(body["result"]["content"][0]["text"], "open");

    let echo_id = find_tool_id(&gateway, &server_id, "echo").await;
    let (status, acl) = set_acl(&gateway, json!({"tool_id": echo_id, "allowed": false})).await;
    assert_eq!(status, StatusCode::CREATED, "{acl}");

    // Anonymous callers only match the default rule.
    let session = gateway.open_session(None).await;
    let (_, body) = gateway
        .rpc(&session, json!(3), "tools/list", json!({}), None)
        .await;
    let names = tool_names(&body);
    assert!(!names.contains(&"remote__echo".to_string()));
    assert!(names.contains(&"remote__boom".to_string()));
    let (_, body) = gateway
        .rpc(
            &session,
            json!(4),
            "tools/call",
            json!({"name": "remote__echo"}),
            None,
        )
        .await;
    assert_eq!(body["error"]["code"], -32005);

    // A keyed consumer without a consumer or group rule is denied as well.
    let (key, _) = create_keyed_consumer(&gateway, "agent-a").await;
    let session = gateway.open_session(Some(&key)).await;
    let (_, body) = gateway
        .rpc(&session, json!(5), "tools/list", json!({}), Some(&key))
        .await;
    assert!(!tool_names(&body).contains(&"remote__echo".to_string()));
    let (_, body) = gateway
        .rpc(
            &session,
            json!(6),
            "tools/call",
            json!({"name": "remote__echo"}),
            Some(&key),
        )
        .await;
    assert_eq!(body["error"]["code"], -32005);
}

#[tokio::test]
async fn consumer_and_group_acls_follow_precedence() {
    let upstream = start_upstream(UpstreamMode::Json).await;
    let gateway = Gateway::new().await;
    let server_id = gateway.add_http_server("remote", &upstream).await;
    let (key_a, consumer_a) = create_keyed_consumer(&gateway, "agent-a").await;
    let (key_b, consumer_b) = create_keyed_consumer(&gateway, "agent-b").await;
    let allow_group = create_consumer_group(&gateway, "allow-team", &[&consumer_a]).await;
    let deny_group = create_consumer_group(&gateway, "deny-team", &[&consumer_a]).await;
    let echo_id = find_tool_id(&gateway, &server_id, "echo").await;

    // Start from a default deny for everyone.
    let (status, _) = set_acl(&gateway, json!({"tool_id": echo_id, "allowed": false})).await;
    assert_eq!(status, StatusCode::CREATED);

    let session_a = gateway.open_session(Some(&key_a)).await;
    let (_, body) = gateway
        .rpc(
            &session_a,
            json!(2),
            "tools/call",
            json!({"name": "remote__echo"}),
            Some(&key_a),
        )
        .await;
    assert_eq!(body["error"]["code"], -32005);

    // A matching group allow grants over the default deny.
    let (status, _) = set_acl(
        &gateway,
        json!({"tool_id": echo_id, "consumer_group_id": allow_group, "allowed": true}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (_, body) = gateway
        .rpc(
            &session_a,
            json!(3),
            "tools/call",
            json!({"name": "remote__echo", "arguments": {"message": "group"}}),
            Some(&key_a),
        )
        .await;
    assert_eq!(body["result"]["content"][0]["text"], "group");
    let (_, body) = gateway
        .rpc(&session_a, json!(4), "tools/list", json!({}), Some(&key_a))
        .await;
    assert!(tool_names(&body).contains(&"remote__echo".to_string()));

    // Any matching group deny overrides the group allow.
    let (status, _) = set_acl(
        &gateway,
        json!({"tool_id": echo_id, "consumer_group_id": deny_group, "allowed": false}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (_, body) = gateway
        .rpc(
            &session_a,
            json!(5),
            "tools/call",
            json!({"name": "remote__echo"}),
            Some(&key_a),
        )
        .await;
    assert_eq!(body["error"]["code"], -32005);

    // An explicit consumer allow wins over the group deny.
    let (status, _) = set_acl(
        &gateway,
        json!({"tool_id": echo_id, "consumer_id": consumer_a, "allowed": true}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (_, body) = gateway
        .rpc(
            &session_a,
            json!(6),
            "tools/call",
            json!({"name": "remote__echo", "arguments": {"message": "consumer"}}),
            Some(&key_a),
        )
        .await;
    assert_eq!(body["result"]["content"][0]["text"], "consumer");

    // An explicit consumer deny wins over the group allow as well.
    let (status, _) = set_acl(
        &gateway,
        json!({"tool_id": echo_id, "consumer_id": consumer_a, "allowed": false}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (_, body) = gateway
        .rpc(
            &session_a,
            json!(7),
            "tools/call",
            json!({"name": "remote__echo"}),
            Some(&key_a),
        )
        .await;
    assert_eq!(body["error"]["code"], -32005);

    // A consumer row is enough to allow a call while the default still denies.
    let (status, _) = set_acl(
        &gateway,
        json!({"tool_id": echo_id, "consumer_id": consumer_b, "allowed": true}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let session_b = gateway.open_session(Some(&key_b)).await;
    let (_, body) = gateway
        .rpc(
            &session_b,
            json!(8),
            "tools/call",
            json!({"name": "remote__echo", "arguments": {"message": "b"}}),
            Some(&key_b),
        )
        .await;
    assert_eq!(body["result"]["content"][0]["text"], "b");
}

async fn set_server_acl(
    gateway: &Gateway,
    server_id: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    gateway
        .control(
            "POST",
            &format!("/api/v1/mcp/servers/{server_id}/acls"),
            body,
        )
        .await
}

#[tokio::test]
async fn server_mode_and_source_validation_is_kong_shaped() {
    let upstream = start_upstream(UpstreamMode::Json).await;
    let gateway = Gateway::new().await;
    let source_id = gateway.add_http_server("source", &upstream).await;

    // Existing/transport servers default to passthrough-listener.
    let (status, source) = gateway
        .control(
            "GET",
            &format!("/api/v1/mcp/servers/{source_id}"),
            json!(null),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{source}");
    assert_eq!(source["type"], "passthrough-listener");
    assert_eq!(source["sources"], json!([]));

    // A listener aggregates named sources; ids are accepted too and both
    // normalize to the canonical server name.
    let (status, listener) = gateway
        .control(
            "POST",
            "/api/v1/mcp/servers",
            json!({
                "name": "aggregate",
                "url": "http://127.0.0.1:9/mcp",
                "mode": "listener",
                "sources": ["source", source_id],
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{listener}");
    assert_eq!(listener["type"], "listener");
    assert_eq!(listener["sources"], json!(["source"]));
    assert_eq!(listener["tool_count"], 0);

    // Only listeners may declare sources.
    let (status, body) = gateway
        .control(
            "POST",
            "/api/v1/mcp/servers",
            json!({
                "name": "plain",
                "url": "http://127.0.0.1:9/mcp",
                "type": "upstream-server",
                "sources": ["source"],
            }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("sources can only be set")
    );

    // Unknown sources are rejected before the row is written.
    let (status, body) = gateway
        .control(
            "POST",
            "/api/v1/mcp/servers",
            json!({
                "name": "ghost-listener",
                "url": "http://127.0.0.1:9/mcp",
                "type": "listener",
                "sources": ["missing"],
            }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unknown MCP source server")
    );

    // A listener cannot source another listener.
    let listener_id = listener["id"].as_str().unwrap();
    let (status, body) = gateway
        .control(
            "POST",
            "/api/v1/mcp/servers",
            json!({
                "name": "nested-listener",
                "url": "http://127.0.0.1:9/mcp",
                "type": "listener",
                "sources": [listener_id],
            }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("expected 'upstream-server'")
    );

    // Conversion modes require an OpenAPI document; other modes reject one.
    let (status, body) = gateway
        .control(
            "POST",
            "/api/v1/mcp/servers",
            json!({
                "name": "converted",
                "url": "http://127.0.0.1:9/mcp",
                "type": "conversion-only",
            }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("requires config.openapi")
    );

    let (status, body) = gateway
        .control(
            "POST",
            "/api/v1/mcp/servers",
            json!({
                "name": "plain-with-openapi",
                "url": "http://127.0.0.1:9/mcp",
                "config": {"openapi": {"document": "{}"}},
            }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("only supported for conversion")
    );

    let (status, body) = gateway
        .control(
            "POST",
            "/api/v1/mcp/servers",
            json!({"name": "bogus", "url": "http://127.0.0.1:9/mcp", "type": "proxy"}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("invalid MCP server mode")
    );

    // Updating an unrelated field keeps the stored type and sources.
    let (status, updated) = gateway
        .control(
            "PUT",
            &format!("/api/v1/mcp/servers/{listener_id}"),
            json!({"name": "aggregate", "url": "http://127.0.0.1:9/mcp"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["type"], "listener");
    assert_eq!(updated["sources"], json!(["source"]));

    // An explicit source list on a non-listener update is still rejected.
    let (status, body) = gateway
        .control(
            "PUT",
            &format!("/api/v1/mcp/servers/{source_id}"),
            json!({"name": "source", "url": upstream.url(), "sources": [listener_id]}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("sources can only be set")
    );
}

#[tokio::test]
async fn server_acl_control_plane_round_trips() {
    let upstream = start_upstream(UpstreamMode::Json).await;
    let gateway = Gateway::new().await;
    let server_id = gateway.add_http_server("remote", &upstream).await;
    let (_, consumer_id) = create_keyed_consumer(&gateway, "agent-a").await;
    let group_id = create_consumer_group(&gateway, "team", &[&consumer_id]).await;

    // Unknown servers cannot own ACLs.
    let (status, body) = set_server_acl(&gateway, "missing", json!({"allowed": false})).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, _) = gateway
        .control("GET", "/api/v1/mcp/servers/missing/acls", json!(null))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Scope validation mirrors the tool ACL rules.
    let (status, body) = set_server_acl(
        &gateway,
        &server_id,
        json!({
            "consumer_id": consumer_id,
            "consumer_group_id": group_id,
            "allowed": true,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    let (status, body) = set_server_acl(
        &gateway,
        &server_id,
        json!({"consumer_id": "missing", "allowed": true}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unknown consumer_id")
    );

    let (status, body) = set_server_acl(
        &gateway,
        &server_id,
        json!({"consumer_group_id": "missing", "allowed": true}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unknown consumer_group_id")
    );

    let (status, group_acl) = set_server_acl(
        &gateway,
        &server_id,
        json!({"consumer_group_id": group_id, "allowed": true}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{group_acl}");
    assert_eq!(group_acl["server_id"], server_id);
    assert!(group_acl["consumer_id"].is_null());

    // Omitting both ids writes the server default.
    let (status, default_acl) =
        set_server_acl(&gateway, &server_id, json!({"allowed": false})).await;
    assert_eq!(status, StatusCode::CREATED, "{default_acl}");
    assert!(default_acl["consumer_id"].is_null());
    assert!(default_acl["consumer_group_id"].is_null());
    assert_eq!(default_acl["allowed"], false);

    // Re-posting the same scope updates in place.
    let (status, updated_default) =
        set_server_acl(&gateway, &server_id, json!({"allowed": true})).await;
    assert_eq!(status, StatusCode::CREATED, "{updated_default}");
    assert_eq!(updated_default["id"], default_acl["id"]);
    assert_eq!(updated_default["allowed"], true);

    let (status, listed) = gateway
        .control(
            "GET",
            &format!("/api/v1/mcp/servers/{server_id}/acls"),
            json!(null),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert_eq!(listed.as_array().unwrap().len(), 2);

    let acl_id = group_acl["id"].as_str().unwrap();
    let (status, single) = gateway
        .control(
            "GET",
            &format!("/api/v1/mcp/server-acls/{acl_id}"),
            json!(null),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{single}");

    let (status, updated) = gateway
        .control(
            "PUT",
            &format!("/api/v1/mcp/server-acls/{acl_id}"),
            json!({"allowed": false}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["allowed"], false);
    assert_eq!(updated["consumer_group_id"], group_id);

    assert_eq!(
        gateway
            .control_delete(&format!("/api/v1/mcp/server-acls/{acl_id}"))
            .await,
        StatusCode::NO_CONTENT
    );
    let (status, _) = gateway
        .control(
            "GET",
            &format!("/api/v1/mcp/server-acls/{acl_id}"),
            json!(null),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        gateway
            .control_delete(&format!("/api/v1/mcp/server-acls/{acl_id}"))
            .await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn no_listener_keeps_the_global_catalog() {
    let first = start_upstream(UpstreamMode::Json).await;
    let second = start_upstream(UpstreamMode::Json).await;
    let gateway = Gateway::new().await;
    gateway.add_http_server("first", &first).await;
    gateway.add_http_server("second", &second).await;

    let session = gateway.open_session(None).await;
    let (_, body) = gateway
        .rpc(&session, json!(2), "tools/list", json!({}), None)
        .await;
    assert_eq!(
        tool_names(&body),
        vec!["first__boom", "first__echo", "second__boom", "second__echo",]
    );
}

#[tokio::test]
async fn listener_with_sources_only_exposes_source_tools() {
    let first = start_upstream(UpstreamMode::Json).await;
    let second = start_upstream(UpstreamMode::Json).await;
    let gateway = Gateway::new().await;
    gateway.add_http_server("first", &first).await;
    gateway.add_http_server("second", &second).await;
    gateway.add_listener("front", &["first"]).await;

    let session = gateway.open_session(None).await;
    let (_, body) = gateway
        .rpc(&session, json!(2), "tools/list", json!({}), None)
        .await;
    assert_eq!(tool_names(&body), vec!["first__boom", "first__echo"]);

    // The source tool still routes.
    let (_, body) = gateway
        .rpc(
            &session,
            json!(3),
            "tools/call",
            json!({"name": "first__echo", "arguments": {"message": "via-listener"}}),
            None,
        )
        .await;
    assert_eq!(body["result"]["content"][0]["text"], "via-listener");
    assert_eq!(*first.state.calls.lock().unwrap(), vec!["echo".to_string()]);

    // Tools outside the listener's sources are not addressable.
    let (_, body) = gateway
        .rpc(
            &session,
            json!(4),
            "tools/call",
            json!({"name": "second__echo"}),
            None,
        )
        .await;
    assert_eq!(body["error"]["code"], -32002);
    assert!(second.state.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn listener_with_empty_sources_aggregates_all_upstreams() {
    let first = start_upstream(UpstreamMode::Json).await;
    let second = start_upstream(UpstreamMode::Json).await;
    let gateway = Gateway::new().await;
    gateway.add_http_server("first", &first).await;
    gateway.add_http_server("second", &second).await;
    gateway.add_listener("front", &[]).await;

    let session = gateway.open_session(None).await;
    let (_, body) = gateway
        .rpc(&session, json!(2), "tools/list", json!({}), None)
        .await;
    assert_eq!(
        tool_names(&body),
        vec!["first__boom", "first__echo", "second__boom", "second__echo",]
    );

    let (_, body) = gateway
        .rpc(
            &session,
            json!(3),
            "tools/call",
            json!({"name": "second__echo", "arguments": {"message": "all"}}),
            None,
        )
        .await;
    assert_eq!(body["result"]["content"][0]["text"], "all");
}

#[tokio::test]
async fn server_default_acl_governs_tools_without_tool_rows() {
    let upstream = start_upstream(UpstreamMode::Json).await;
    let gateway = Gateway::new().await;
    let server_id = gateway.add_http_server("remote", &upstream).await;

    // Server default deny closes every tool that has no rows of its own.
    let (status, acl) = set_server_acl(&gateway, &server_id, json!({"allowed": false})).await;
    assert_eq!(status, StatusCode::CREATED, "{acl}");

    let session = gateway.open_session(None).await;
    let (_, body) = gateway
        .rpc(&session, json!(2), "tools/list", json!({}), None)
        .await;
    assert!(tool_names(&body).is_empty(), "{body}");
    let (_, body) = gateway
        .rpc(
            &session,
            json!(3),
            "tools/call",
            json!({"name": "remote__echo"}),
            None,
        )
        .await;
    assert_eq!(body["error"]["code"], -32005);

    // A per-tool allow overrides the server default for that tool only.
    let echo_id = find_tool_id(&gateway, &server_id, "echo").await;
    let (status, _) = set_acl(&gateway, json!({"tool_id": echo_id, "allowed": true})).await;
    assert_eq!(status, StatusCode::CREATED);
    let (_, body) = gateway
        .rpc(
            &session,
            json!(4),
            "tools/call",
            json!({"name": "remote__echo", "arguments": {"message": "override"}}),
            None,
        )
        .await;
    assert_eq!(body["result"]["content"][0]["text"], "override");
    let (_, body) = gateway
        .rpc(&session, json!(5), "tools/list", json!({}), None)
        .await;
    assert_eq!(tool_names(&body), vec!["remote__echo"]);

    // Flipping the server default to allow reopens boom (still no tool rows)
    // while echo keeps following its own overriding rows.
    let (status, _) = set_server_acl(&gateway, &server_id, json!({"allowed": true})).await;
    assert_eq!(status, StatusCode::CREATED);
    let (_, body) = gateway
        .rpc(
            &session,
            json!(6),
            "tools/call",
            json!({"name": "remote__boom"}),
            None,
        )
        .await;
    assert_eq!(body["error"]["code"], -32000, "boom reaches the upstream");
}

#[tokio::test]
async fn tool_acl_deny_overrides_an_allow_server_default() {
    let upstream = start_upstream(UpstreamMode::Json).await;
    let gateway = Gateway::new().await;
    let server_id = gateway.add_http_server("remote", &upstream).await;

    let (status, _) = set_server_acl(&gateway, &server_id, json!({"allowed": true})).await;
    assert_eq!(status, StatusCode::CREATED);
    let echo_id = find_tool_id(&gateway, &server_id, "echo").await;
    let (status, _) = set_acl(&gateway, json!({"tool_id": echo_id, "allowed": false})).await;
    assert_eq!(status, StatusCode::CREATED);

    let session = gateway.open_session(None).await;
    let (_, body) = gateway
        .rpc(&session, json!(2), "tools/list", json!({}), None)
        .await;
    assert_eq!(tool_names(&body), vec!["remote__boom"]);

    let (_, body) = gateway
        .rpc(
            &session,
            json!(3),
            "tools/call",
            json!({"name": "remote__echo"}),
            None,
        )
        .await;
    assert_eq!(body["error"]["code"], -32005);

    // A tool without its own rows still follows the allowing server default.
    let (_, body) = gateway
        .rpc(
            &session,
            json!(4),
            "tools/call",
            json!({"name": "remote__boom"}),
            None,
        )
        .await;
    assert_eq!(body["error"]["code"], -32000, "boom reaches the upstream");
}

#[tokio::test]
async fn server_acl_consumer_and_group_precedence_is_preserved() {
    let upstream = start_upstream(UpstreamMode::Json).await;
    let gateway = Gateway::new().await;
    let server_id = gateway.add_http_server("remote", &upstream).await;
    let (key_a, consumer_a) = create_keyed_consumer(&gateway, "agent-a").await;
    let allow_group = create_consumer_group(&gateway, "allow-team", &[&consumer_a]).await;
    let deny_group = create_consumer_group(&gateway, "deny-team", &[&consumer_a]).await;

    // Start from a server default deny.
    let (status, _) = set_server_acl(&gateway, &server_id, json!({"allowed": false})).await;
    assert_eq!(status, StatusCode::CREATED);
    let session_a = gateway.open_session(Some(&key_a)).await;
    let (_, body) = gateway
        .rpc(
            &session_a,
            json!(2),
            "tools/call",
            json!({"name": "remote__echo"}),
            Some(&key_a),
        )
        .await;
    assert_eq!(body["error"]["code"], -32005);

    // A matching group allow grants over the default deny.
    let (status, _) = set_server_acl(
        &gateway,
        &server_id,
        json!({"consumer_group_id": allow_group, "allowed": true}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (_, body) = gateway
        .rpc(
            &session_a,
            json!(3),
            "tools/call",
            json!({"name": "remote__echo", "arguments": {"message": "group"}}),
            Some(&key_a),
        )
        .await;
    assert_eq!(body["result"]["content"][0]["text"], "group");

    // Any matching group deny overrides the group allow.
    let (status, _) = set_server_acl(
        &gateway,
        &server_id,
        json!({"consumer_group_id": deny_group, "allowed": false}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (_, body) = gateway
        .rpc(
            &session_a,
            json!(4),
            "tools/call",
            json!({"name": "remote__echo"}),
            Some(&key_a),
        )
        .await;
    assert_eq!(body["error"]["code"], -32005);

    // An explicit consumer allow wins over the group deny.
    let (status, _) = set_server_acl(
        &gateway,
        &server_id,
        json!({"consumer_id": consumer_a, "allowed": true}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (_, body) = gateway
        .rpc(
            &session_a,
            json!(5),
            "tools/call",
            json!({"name": "remote__echo", "arguments": {"message": "consumer"}}),
            Some(&key_a),
        )
        .await;
    assert_eq!(body["result"]["content"][0]["text"], "consumer");
}

#[tokio::test]
async fn refresh_rediscovers_tools_and_unknown_servers_fail() {
    let upstream = start_upstream(UpstreamMode::Json).await;
    let gateway = Gateway::new().await;
    let server_id = gateway.add_http_server("remote", &upstream).await;

    let (status, stats) = gateway
        .control(
            "POST",
            &format!("/api/v1/mcp/servers/{server_id}/refresh"),
            json!(null),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{stats}");
    assert_eq!(stats["tools"], 2);
    assert_eq!(stats["protocol_version"], "2025-06-18");

    let (status, _) = gateway
        .control("POST", "/api/v1/mcp/servers/missing/refresh", json!(null))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn per_server_routes_scope_catalogs_calls_and_sessions() {
    let first = start_upstream(UpstreamMode::Json).await;
    let second = start_upstream(UpstreamMode::Json).await;
    let gateway = Gateway::new().await;
    let first_id = gateway.add_http_server("first", &first).await;
    let second_id = gateway.add_http_server("second", &second).await;

    // Name and id both resolve to the same scoped endpoint.
    let session = gateway.open_session_at("/mcp/second", None).await;
    let stored = gateway
        .storage
        .mcp
        .get_session(&session)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        stored.server_id, second_id,
        "scoped sessions anchor to the selected server"
    );

    let (status, body) = gateway
        .rpc_at(
            "/mcp/second",
            &session,
            json!(2),
            "tools/list",
            json!({}),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(tool_names(&body), vec!["second__boom", "second__echo"]);

    let id_path = format!("/mcp/{first_id}");
    let id_session = gateway.open_session_at(&id_path, None).await;
    let (_, body) = gateway
        .rpc_at(
            &id_path,
            &id_session,
            json!(3),
            "tools/list",
            json!({}),
            None,
        )
        .await;
    assert_eq!(tool_names(&body), vec!["first__boom", "first__echo"]);

    // Calls are scoped too: a valid source tool routes, a foreign one is unknown.
    let (_, body) = gateway
        .rpc_at(
            &id_path,
            &id_session,
            json!(4),
            "tools/call",
            json!({"name": "first__echo", "arguments": {"message": "scoped"}}),
            None,
        )
        .await;
    assert_eq!(body["result"]["content"][0]["text"], "scoped");
    let (_, body) = gateway
        .rpc_at(
            &id_path,
            &id_session,
            json!(5),
            "tools/call",
            json!({"name": "second__echo"}),
            None,
        )
        .await;
    assert_eq!(body["error"]["code"], -32002);

    // Unknown routes are 404 for both POST and GET.
    let (status, _, body) = gateway.initialize_at("/mcp/ghost", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"]["code"], -32002);
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unknown MCP route")
    );
    let request = axum::http::Request::builder()
        .method("GET")
        .uri("/mcp/ghost")
        .body(axum::body::Body::empty())
        .unwrap();
    let response = tower::ServiceExt::oneshot(gateway.data_router(), request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    // The global endpoint keeps aggregating every enabled server.
    let global = gateway.open_session(None).await;
    let (_, body) = gateway
        .rpc(&global, json!(6), "tools/list", json!({}), None)
        .await;
    assert_eq!(
        tool_names(&body),
        vec!["first__boom", "first__echo", "second__boom", "second__echo",]
    );
}

#[tokio::test]
async fn listener_route_exposes_only_its_sources() {
    let first = start_upstream(UpstreamMode::Json).await;
    let second = start_upstream(UpstreamMode::Json).await;
    let gateway = Gateway::new().await;
    gateway.add_http_server("first", &first).await;
    gateway.add_http_server("second", &second).await;
    gateway.add_listener("front", &["first"]).await;

    let session = gateway.open_session_at("/mcp/front", None).await;
    let (status, body) = gateway
        .rpc_at(
            "/mcp/front",
            &session,
            json!(2),
            "tools/list",
            json!({}),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(tool_names(&body), vec!["first__boom", "first__echo"]);

    let (_, body) = gateway
        .rpc_at(
            "/mcp/front",
            &session,
            json!(3),
            "tools/call",
            json!({"name": "first__echo", "arguments": {"message": "listener"}}),
            None,
        )
        .await;
    assert_eq!(body["result"]["content"][0]["text"], "listener");

    let (_, body) = gateway
        .rpc_at(
            "/mcp/front",
            &session,
            json!(4),
            "tools/call",
            json!({"name": "second__echo"}),
            None,
        )
        .await;
    assert_eq!(body["error"]["code"], -32002);
    assert!(second.state.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn custom_route_paths_resolve_and_are_validated() {
    let upstream = start_upstream(UpstreamMode::Json).await;
    let gateway = Gateway::new().await;

    let (status, created) = gateway
        .control(
            "POST",
            "/api/v1/mcp/servers",
            json!({
                "name": "custom",
                "transport": "http",
                "url": upstream.url(),
                "route_path": "/mcp/team/custom",
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["route_path"], "/mcp/team/custom");
    let custom_id = created["id"].as_str().unwrap().to_string();

    let session = gateway.open_session_at("/mcp/team/custom", None).await;
    let stored = gateway
        .storage
        .mcp
        .get_session(&session)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.server_id, custom_id);
    let (_, body) = gateway
        .rpc_at(
            "/mcp/team/custom",
            &session,
            json!(2),
            "tools/list",
            json!({}),
            None,
        )
        .await;
    assert_eq!(tool_names(&body), vec!["custom__boom", "custom__echo"]);

    // Responses expose the name-derived default even when the column is null.
    let default_id = gateway.add_http_server("default", &upstream).await;
    let (status, default) = gateway
        .control(
            "GET",
            &format!("/api/v1/mcp/servers/{default_id}"),
            json!(null),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{default}");
    assert_eq!(default["route_path"], "/mcp/default");

    // Route path syntax and uniqueness are validated with field-specific errors.
    let (status, body) = gateway
        .control(
            "POST",
            "/api/v1/mcp/servers",
            json!({
                "name": "bad-path",
                "transport": "http",
                "url": upstream.url(),
                "route_path": "mcp/no-slash",
            }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("route_path")
    );

    for path in ["/mcp", "/mcp/", "/mcp/bad?query=1"] {
        let (status, body) = gateway
            .control(
                "POST",
                "/api/v1/mcp/servers",
                json!({
                    "name": "bad-path",
                    "transport": "http",
                    "url": upstream.url(),
                    "route_path": path,
                }),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}: {body}");
    }

    let (status, body) = gateway
        .control(
            "POST",
            "/api/v1/mcp/servers",
            json!({
                "name": "rival",
                "transport": "http",
                "url": upstream.url(),
                "route_path": "/mcp/team/custom",
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("already used")
    );

    // Updates accept a new path and keep it in responses.
    let (status, updated) = gateway
        .control(
            "PUT",
            &format!("/api/v1/mcp/servers/{custom_id}"),
            json!({
                "name": "custom",
                "transport": "http",
                "url": upstream.url(),
                "route_path": "/mcp/team/renamed",
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["route_path"], "/mcp/team/renamed");
    let session = gateway.open_session_at("/mcp/team/renamed", None).await;
    let (_, body) = gateway
        .rpc_at(
            "/mcp/team/renamed",
            &session,
            json!(3),
            "tools/list",
            json!({}),
            None,
        )
        .await;
    assert_eq!(tool_names(&body), vec!["custom__boom", "custom__echo"]);
}

#[tokio::test]
async fn cel_allow_when_matches_consumer_groups_and_denies_outsiders() {
    let upstream = start_upstream(UpstreamMode::Json).await;
    let gateway = Gateway::new().await;
    let (member_key, member_id) = create_keyed_consumer(&gateway, "team-member").await;
    let (outsider_key, _) = create_keyed_consumer(&gateway, "outsider").await;
    create_consumer_group(&gateway, "team", &[&member_id]).await;

    let (status, server) = gateway
        .control(
            "POST",
            "/api/v1/mcp/servers",
            json!({
                "name": "guarded",
                "transport": "http",
                "url": upstream.url(),
                "acl_allow_when": ["consumer_group.names.contains(\"team\")"],
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{server}");
    assert_eq!(
        server["acl_allow_when"],
        json!(["consumer_group.names.contains(\"team\")"])
    );

    let member_session = gateway.open_session(Some(&member_key)).await;
    let (_, body) = gateway
        .rpc(
            &member_session,
            json!(2),
            "tools/list",
            json!({}),
            Some(&member_key),
        )
        .await;
    assert_eq!(tool_names(&body), vec!["guarded__boom", "guarded__echo"]);
    let (_, body) = gateway
        .rpc(
            &member_session,
            json!(3),
            "tools/call",
            json!({"name": "guarded__echo", "arguments": {"message": "group"}}),
            Some(&member_key),
        )
        .await;
    assert_eq!(body["result"]["content"][0]["text"], "group");

    // A consumer outside the group does not satisfy the allow expression.
    let outsider_session = gateway.open_session(Some(&outsider_key)).await;
    let (_, body) = gateway
        .rpc(
            &outsider_session,
            json!(4),
            "tools/list",
            json!({}),
            Some(&outsider_key),
        )
        .await;
    assert!(tool_names(&body).is_empty(), "{body}");
    let (_, body) = gateway
        .rpc(
            &outsider_session,
            json!(5),
            "tools/call",
            json!({"name": "guarded__echo"}),
            Some(&outsider_key),
        )
        .await;
    assert_eq!(body["error"]["code"], -32005);
}

#[tokio::test]
async fn cel_deny_when_wins_over_allow_when() {
    let upstream = start_upstream(UpstreamMode::Json).await;
    let gateway = Gateway::new().await;
    let (blocked_key, blocked_id) = create_keyed_consumer(&gateway, "blocked").await;
    let (allowed_key, _) = create_keyed_consumer(&gateway, "allowed").await;

    let (status, server) = gateway
        .control(
            "POST",
            "/api/v1/mcp/servers",
            json!({
                "name": "guarded",
                "transport": "http",
                "url": upstream.url(),
                "acl_allow_when": ["true"],
                "acl_deny_when": [format!("consumer.id == \"{blocked_id}\"")],
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{server}");

    let blocked_session = gateway.open_session(Some(&blocked_key)).await;
    let (_, body) = gateway
        .rpc(
            &blocked_session,
            json!(2),
            "tools/list",
            json!({}),
            Some(&blocked_key),
        )
        .await;
    assert!(tool_names(&body).is_empty(), "{body}");
    let (_, body) = gateway
        .rpc(
            &blocked_session,
            json!(3),
            "tools/call",
            json!({"name": "guarded__echo"}),
            Some(&blocked_key),
        )
        .await;
    assert_eq!(body["error"]["code"], -32005);

    // The same allow expression grants every consumer the deny does not match.
    let allowed_session = gateway.open_session(Some(&allowed_key)).await;
    let (_, body) = gateway
        .rpc(
            &allowed_session,
            json!(4),
            "tools/list",
            json!({}),
            Some(&allowed_key),
        )
        .await;
    assert_eq!(tool_names(&body), vec!["guarded__boom", "guarded__echo"]);
    let (_, body) = gateway
        .rpc(
            &allowed_session,
            json!(5),
            "tools/call",
            json!({"name": "guarded__echo", "arguments": {"message": "allowed"}}),
            Some(&allowed_key),
        )
        .await;
    assert_eq!(body["result"]["content"][0]["text"], "allowed");
}

#[tokio::test]
async fn per_tool_acl_rows_override_server_cel() {
    let upstream = start_upstream(UpstreamMode::Json).await;
    let gateway = Gateway::new().await;
    let server_id = gateway.add_http_server("remote", &upstream).await;

    // Server CEL denies every tool without rows of its own.
    let (status, updated) = gateway
        .control(
            "PUT",
            &format!("/api/v1/mcp/servers/{server_id}"),
            json!({
                "name": "remote",
                "transport": "http",
                "url": upstream.url(),
                "acl_deny_when": ["true"],
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");

    let echo_id = find_tool_id(&gateway, &server_id, "echo").await;
    let (status, acl) = set_acl(&gateway, json!({"tool_id": echo_id, "allowed": true})).await;
    assert_eq!(status, StatusCode::CREATED, "{acl}");

    let session = gateway.open_session(None).await;
    let (_, body) = gateway
        .rpc(&session, json!(2), "tools/list", json!({}), None)
        .await;
    assert_eq!(tool_names(&body), vec!["remote__echo"]);

    let (_, body) = gateway
        .rpc(
            &session,
            json!(3),
            "tools/call",
            json!({"name": "remote__echo", "arguments": {"message": "override"}}),
            None,
        )
        .await;
    assert_eq!(body["result"]["content"][0]["text"], "override");

    let (_, body) = gateway
        .rpc(
            &session,
            json!(4),
            "tools/call",
            json!({"name": "remote__boom"}),
            None,
        )
        .await;
    assert_eq!(body["error"]["code"], -32005);
}

#[tokio::test]
async fn invalid_cel_expressions_are_rejected_by_the_control_api() {
    let upstream = start_upstream(UpstreamMode::Json).await;
    let gateway = Gateway::new().await;

    let cases = [
        (
            json!({"name": "non-bool", "url": upstream.url(), "acl_allow_when": ["1 + 1"]}),
            "must evaluate to a boolean",
        ),
        (
            json!({"name": "syntax", "url": upstream.url(), "acl_deny_when": ["this is not cel"]}),
            "acl_deny_when[0]",
        ),
        (
            json!({"name": "empty", "url": upstream.url(), "acl_allow_when": [""]}),
            "must not be empty",
        ),
        (
            json!({"name": "too-long", "url": upstream.url(), "acl_allow_when": ["a".repeat(1025)]}),
            "at most 1024",
        ),
    ];
    for (body, expected) in cases {
        let (status, response) = gateway.control("POST", "/api/v1/mcp/servers", body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{response}");
        assert!(
            response["error"]["message"]
                .as_str()
                .unwrap()
                .contains(expected),
            "expected '{expected}' in {response}"
        );
    }

    // Updates run the same validation before persisting.
    let server_id = gateway.add_http_server("remote", &upstream).await;
    let (status, body) = gateway
        .control(
            "PUT",
            &format!("/api/v1/mcp/servers/{server_id}"),
            json!({
                "name": "remote",
                "transport": "http",
                "url": upstream.url(),
                "acl_allow_when": ["request.method"],
            }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("acl_allow_when[0]")
    );
}

#[tokio::test]
async fn cel_evaluation_failures_fail_closed() {
    let upstream = start_upstream(UpstreamMode::Json).await;
    let gateway = Gateway::new().await;
    let server_id = gateway.add_http_server("remote", &upstream).await;

    // Bypass control-plane validation to simulate a corrupted/legacy row.
    let update = NewMcpServer {
        name: "remote".into(),
        transport: Transport::Http,
        mode: default_server_mode(),
        sources: Vec::new(),
        route_path: None,
        acl_allow_when: vec!["unknown_variable".into()],
        acl_deny_when: Vec::new(),
        auth_strategy_id: None,
        command: None,
        url: Some(upstream.url()),
        enabled: true,
        config: json!({}),
        headers: serde_json::Map::new(),
    };
    gateway
        .storage
        .mcp
        .update_server(&server_id, &update)
        .await
        .unwrap()
        .unwrap();

    let session = gateway.open_session(None).await;
    let (status, body) = gateway
        .rpc(&session, json!(2), "tools/list", json!({}), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        tool_names(&body).is_empty(),
        "evaluation failures must hide tools: {body}"
    );

    let (_, body) = gateway
        .rpc(
            &session,
            json!(3),
            "tools/call",
            json!({"name": "remote__echo"}),
            None,
        )
        .await;
    assert_eq!(body["error"]["code"], -32005);
    assert!(upstream.state.calls.lock().unwrap().is_empty());
}

// ----- OIDC auth strategies ---------------------------------------------

const OIDC_ISSUER: &str = "https://issuer.example.test";
const OIDC_AUDIENCE: &str = "octra-mcp";

fn oidc_config(jwks_uri: &str, audience: Option<&str>) -> serde_json::Value {
    let mut config = json!({
        "issuer": OIDC_ISSUER,
        "jwks_uri": jwks_uri,
        "required_scopes": ["tools:call"],
        "algorithms": ["RS256"],
        "jwks_cache_secs": 300,
    });
    if let Some(audience) = audience {
        config["audience"] = json!(audience);
    }
    config
}

fn discovery_oidc_config(discovery_url: &str, audience: Option<&str>) -> serde_json::Value {
    let mut config = json!({
        "issuer": OIDC_ISSUER,
        "discovery_url": discovery_url,
        "required_scopes": ["tools:call"],
        "algorithms": ["RS256"],
        "jwks_cache_secs": 300,
    });
    if let Some(audience) = audience {
        config["audience"] = json!(audience);
    }
    config
}

fn introspection_oidc_config(
    endpoint: &str,
    client_secret_ref: &str,
    audience: Option<&str>,
) -> serde_json::Value {
    let mut config = json!({
        "issuer": OIDC_ISSUER,
        "token_validation": "introspection",
        "required_scopes": ["tools:call"],
        "introspection": {
            "endpoint": endpoint,
            "client_id": "octra-test-client",
            "client_secret_ref": client_secret_ref,
            "cache_secs": 60,
        },
    });
    if let Some(audience) = audience {
        config["audience"] = json!(audience);
    }
    config
}

fn oidc_client_secret_ref(name: &str) -> String {
    let path = std::env::temp_dir().join(format!(
        "octra-phase43-{name}-{}.secret",
        std::process::id()
    ));
    std::fs::write(&path, "test-client-secret").unwrap();
    format!("file:{}", path.display())
}

fn introspection_claims(expires_at: u64, audience: Option<&str>, scope: &str) -> serde_json::Value {
    let mut claims = json!({
        "active": true,
        "sub": "opaque-user",
        "preferred_username": "Opaque User",
        "email": "opaque.user@example.test",
        "groups": ["team"],
        "scope": scope,
        "exp": expires_at,
    });
    if let Some(audience) = audience {
        claims["aud"] = json!(audience);
    }
    claims
}

async fn post_strategy_config(
    gateway: &Gateway,
    name: &str,
    config: serde_json::Value,
    is_default: bool,
    enabled: bool,
) -> (StatusCode, serde_json::Value) {
    gateway
        .control(
            "POST",
            "/api/v1/mcp/auth-strategies",
            json!({
                "name": name,
                "kind": "oidc",
                "config": config,
                "enabled": enabled,
                "is_default": is_default,
            }),
        )
        .await
}

async fn post_oidc_strategy(
    gateway: &Gateway,
    name: &str,
    jwks_uri: &str,
    audience: Option<&str>,
    is_default: bool,
    enabled: bool,
) -> (StatusCode, serde_json::Value) {
    post_strategy_config(
        gateway,
        name,
        oidc_config(jwks_uri, audience),
        is_default,
        enabled,
    )
    .await
}

fn epoch_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[allow(clippy::too_many_arguments)]
fn mint_token(
    kid: &str,
    issuer: &str,
    audience: Option<&str>,
    sub: &str,
    groups: serde_json::Value,
    scope: &str,
    expires_at: u64,
    not_before: u64,
) -> String {
    let mut claims = json!({
        "iss": issuer,
        "sub": sub,
        "preferred_username": "OIDC User",
        "email": "oidc.user@example.test",
        "groups": groups,
        "scope": scope,
        "exp": expires_at,
        "nbf": not_before,
    });
    if let Some(audience) = audience {
        claims["aud"] = json!(audience);
    }
    sign_test_token(kid, claims)
}

fn valid_token() -> String {
    let now = epoch_now();
    mint_token(
        "test-kid",
        OIDC_ISSUER,
        Some(OIDC_AUDIENCE),
        "oidc-user",
        json!(["team"]),
        "tools:call tools:list",
        now + 3600,
        now - 10,
    )
}

#[tokio::test]
async fn auth_strategy_control_plane_crud_validates_and_keeps_one_default() {
    let gateway = Gateway::new().await;
    let jwks = start_jwks(test_jwks("test-kid")).await;

    let (status, list) = gateway
        .control("GET", "/api/v1/mcp/auth-strategies", json!(null))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list, json!([]));

    let invalid = [
        (
            json!({
                "name": "bad-url",
                "kind": "oidc",
                "config": {"issuer": OIDC_ISSUER, "jwks_uri": "not-a-url"},
            }),
            "absolute http(s) URL",
        ),
        (
            json!({
                "name": "bad-alg",
                "kind": "oidc",
                "config": {
                    "issuer": OIDC_ISSUER,
                    "jwks_uri": jwks.url(),
                    "algorithms": ["HS256"],
                },
            }),
            "unsupported OIDC algorithm",
        ),
        (
            json!({
                "name": "bad-claim",
                "kind": "oidc",
                "config": {
                    "issuer": OIDC_ISSUER,
                    "jwks_uri": jwks.url(),
                    "consumer_claim": " ",
                },
            }),
            "consumer_claim",
        ),
        (
            json!({
                "name": "jwt-without-source",
                "kind": "oidc",
                "config": {"issuer": OIDC_ISSUER, "token_validation": "jwt"},
            }),
            "requires jwks_uri or discovery_url",
        ),
        (
            json!({
                "name": "introspection-without-config",
                "kind": "oidc",
                "config": {"issuer": OIDC_ISSUER, "token_validation": "introspection"},
            }),
            "requires introspection settings",
        ),
        (
            json!({
                "name": "auto-without-source",
                "kind": "oidc",
                "config": {"issuer": OIDC_ISSUER, "token_validation": "auto"},
            }),
            "requires jwks_uri, discovery_url, or introspection",
        ),
        (
            json!({
                "name": "bad-discovery-url",
                "kind": "oidc",
                "config": {
                    "issuer": OIDC_ISSUER,
                    "discovery_url": "relative",
                    "token_validation": "jwt",
                },
            }),
            "absolute http(s) URL",
        ),
        (
            json!({
                "name": "bad-introspection-cache",
                "kind": "oidc",
                "config": {
                    "issuer": OIDC_ISSUER,
                    "token_validation": "introspection",
                    "introspection": {
                        "endpoint": "http://127.0.0.1:9/introspect",
                        "client_id": "octra",
                        "client_secret_ref": "env:OIDC_SECRET",
                        "cache_secs": 0,
                    },
                },
            }),
            "cache_secs",
        ),
        (
            json!({
                "name": "bad-kind",
                "kind": "jwt",
                "config": oidc_config(&jwks.url(), None),
            }),
            "invalid auth strategy kind",
        ),
        (
            json!({"name": "", "kind": "oidc", "config": oidc_config(&jwks.url(), None)}),
            "name must not be empty",
        ),
    ];
    for (body, expected) in invalid {
        let (status, response) = gateway
            .control("POST", "/api/v1/mcp/auth-strategies", body)
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{response}");
        assert!(
            response["error"]["message"]
                .as_str()
                .unwrap()
                .contains(expected),
            "expected '{expected}' in {response}"
        );
    }

    let (status, first) =
        post_oidc_strategy(&gateway, "first", &jwks.url(), None, true, true).await;
    assert_eq!(status, StatusCode::CREATED, "{first}");
    assert_eq!(first["is_default"], true);
    assert_eq!(first["kind"], "oidc");
    let first_id = first["id"].as_str().unwrap().to_string();

    // A second default atomically replaces the first.
    let (status, second) =
        post_oidc_strategy(&gateway, "second", &jwks.url(), None, true, true).await;
    assert_eq!(status, StatusCode::CREATED, "{second}");
    let second_id = second["id"].as_str().unwrap().to_string();
    let (_, listed) = gateway
        .control("GET", "/api/v1/mcp/auth-strategies", json!(null))
        .await;
    let defaults: Vec<&serde_json::Value> = listed
        .as_array()
        .unwrap()
        .iter()
        .filter(|strategy| strategy["is_default"] == true)
        .collect();
    assert_eq!(defaults.len(), 1, "{listed}");
    assert_eq!(defaults[0]["id"], second_id);

    // Duplicate names conflict.
    let (status, duplicate) =
        post_oidc_strategy(&gateway, "first", &jwks.url(), None, false, true).await;
    assert_eq!(status, StatusCode::CONFLICT, "{duplicate}");

    let (status, fetched) = gateway
        .control(
            "GET",
            &format!("/api/v1/mcp/auth-strategies/{first_id}"),
            json!(null),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{fetched}");
    assert_eq!(fetched["config"]["issuer"], OIDC_ISSUER);

    // Partial updates preserve omitted fields and can move the default.
    let (status, promoted) = gateway
        .control(
            "PUT",
            &format!("/api/v1/mcp/auth-strategies/{first_id}"),
            json!({"config": oidc_config(&jwks.url(), Some("updated-audience")), "is_default": true}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{promoted}");
    assert_eq!(promoted["name"], "first");
    assert_eq!(promoted["is_default"], true);
    assert_eq!(promoted["config"]["audience"], "updated-audience");
    let (_, listed) = gateway
        .control("GET", "/api/v1/mcp/auth-strategies", json!(null))
        .await;
    let defaults: Vec<&serde_json::Value> = listed
        .as_array()
        .unwrap()
        .iter()
        .filter(|strategy| strategy["is_default"] == true)
        .collect();
    assert_eq!(defaults.len(), 1, "{listed}");
    assert_eq!(defaults[0]["id"], first_id);

    // Invalid updates are rejected before writing.
    let (status, body) = gateway
        .control(
            "PUT",
            &format!("/api/v1/mcp/auth-strategies/{first_id}"),
            json!({"config": {"issuer": OIDC_ISSUER, "jwks_uri": "relative"}}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    assert_eq!(
        gateway
            .control_delete(&format!("/api/v1/mcp/auth-strategies/{second_id}"))
            .await,
        StatusCode::NO_CONTENT
    );
    let (status, _) = gateway
        .control(
            "GET",
            &format!("/api/v1/mcp/auth-strategies/{second_id}"),
            json!(null),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        gateway
            .control_delete(&format!("/api/v1/mcp/auth-strategies/{second_id}"))
            .await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn auth_strategy_control_plane_accepts_discovery_and_introspection_configs() {
    let jwks = start_jwks(test_jwks("test-kid")).await;
    let discovery = start_discovery(json!({
        "issuer": OIDC_ISSUER,
        "jwks_uri": jwks.url(),
        "introspection_endpoint": "https://issuer.example.test/introspect",
    }))
    .await;
    let introspection = start_introspection().await;
    let secret_ref = oidc_client_secret_ref("control-plane");
    let gateway = Gateway::new().await;

    let config = discovery_oidc_config(&discovery.url(), None);
    let (status, created) =
        post_strategy_config(&gateway, "discovery-sso", config.clone(), false, true).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["config"]["discovery_url"], discovery.url());
    assert!(created["config"]["jwks_uri"].is_null(), "{created}");

    let config = introspection_oidc_config(&introspection.url(), &secret_ref, None);
    let (status, created) =
        post_strategy_config(&gateway, "introspection-sso", config, false, true).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["config"]["token_validation"], "introspection");
    assert_eq!(
        created["config"]["introspection"]["client_id"],
        "octra-test-client"
    );
    assert_eq!(
        created["config"]["introspection"]["client_secret_ref"],
        secret_ref
    );

    // An introspection strategy may omit the endpoint when discovery
    // provides it.
    let config = json!({
        "issuer": OIDC_ISSUER,
        "discovery_url": discovery.url(),
        "token_validation": "introspection",
        "introspection": {
            "client_id": "octra-test-client",
            "client_secret_ref": secret_ref,
        },
    });
    let (status, created) =
        post_strategy_config(&gateway, "discovered-introspection", config, false, true).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert!(
        created["config"]["introspection"]["endpoint"].is_null(),
        "{created}"
    );
}

#[tokio::test]
async fn server_auth_strategy_references_are_validated_and_round_trip() {
    let upstream = start_upstream(UpstreamMode::Json).await;
    let gateway = Gateway::new().await;
    let jwks = start_jwks(test_jwks("test-kid")).await;
    let (status, strategy) =
        post_oidc_strategy(&gateway, "server-sso", &jwks.url(), None, false, true).await;
    assert_eq!(status, StatusCode::CREATED, "{strategy}");
    let strategy_id = strategy["id"].as_str().unwrap().to_string();

    let (status, body) = gateway
        .control(
            "POST",
            "/api/v1/mcp/servers",
            json!({
                "name": "remote",
                "transport": "http",
                "url": upstream.url(),
                "auth_strategy_id": "missing-strategy",
            }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unknown auth_strategy_id")
    );

    let (status, server) = gateway
        .control(
            "POST",
            "/api/v1/mcp/servers",
            json!({
                "name": "remote",
                "transport": "http",
                "url": upstream.url(),
                "auth_strategy_id": strategy_id,
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{server}");
    assert_eq!(server["auth_strategy_id"], strategy_id);
    let server_id = server["id"].as_str().unwrap().to_string();

    let (status, body) = gateway
        .control(
            "PUT",
            &format!("/api/v1/mcp/servers/{server_id}"),
            json!({
                "name": "remote",
                "transport": "http",
                "url": upstream.url(),
                "auth_strategy_id": "missing-strategy",
            }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // An explicit null clears the reference while other fields stay intact.
    let (status, cleared) = gateway
        .control(
            "PUT",
            &format!("/api/v1/mcp/servers/{server_id}"),
            json!({
                "name": "remote",
                "transport": "http",
                "url": upstream.url(),
                "auth_strategy_id": null,
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{cleared}");
    assert!(cleared["auth_strategy_id"].is_null());

    let (status, updated) = gateway
        .control(
            "PUT",
            &format!("/api/v1/mcp/servers/{server_id}"),
            json!({
                "name": "remote",
                "transport": "http",
                "url": upstream.url(),
                "auth_strategy_id": strategy_id,
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["auth_strategy_id"], strategy_id);

    // Deleting the strategy clears the server's foreign key reference.
    assert_eq!(
        gateway
            .control_delete(&format!("/api/v1/mcp/auth-strategies/{strategy_id}"))
            .await,
        StatusCode::NO_CONTENT
    );
    let (status, server) = gateway
        .control(
            "GET",
            &format!("/api/v1/mcp/servers/{server_id}"),
            json!(null),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{server}");
    assert!(server["auth_strategy_id"].is_null());
}

#[tokio::test]
async fn oidc_tokens_authenticate_and_group_claims_drive_acls() {
    let upstream = start_upstream(UpstreamMode::Json).await;
    let gateway = Gateway::new().await;
    let jwks = start_jwks(test_jwks("test-kid")).await;
    let server_id = gateway.add_http_server("remote", &upstream).await;
    let team_id = create_consumer_group(&gateway, "team", &[]).await;

    let (status, strategy) = post_oidc_strategy(
        &gateway,
        "default-sso",
        &jwks.url(),
        Some(OIDC_AUDIENCE),
        true,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{strategy}");

    let token = valid_token();
    let session = gateway.open_session(Some(&token)).await;

    // The group claim resolves to the existing consumer group id.
    let (status, body) = gateway
        .rpc(
            &session,
            json!(2),
            "tools/call",
            json!({"name": "remote__echo", "arguments": {"message": "oidc"}}),
            Some(&token),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["result"]["content"][0]["text"], "oidc");

    let invocations = gateway.storage.mcp.list_invocations(5, None).await.unwrap();
    let call = invocations
        .iter()
        .find(|row| row.status == "ok")
        .expect("call is recorded");
    assert_eq!(call.consumer_id.as_deref(), Some("oidc-user"));

    // A group ACL grants the token's mapped group over a default deny.
    let echo_id = find_tool_id(&gateway, &server_id, "echo").await;
    let (status, _) = set_acl(&gateway, json!({"tool_id": echo_id, "allowed": false})).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = set_acl(
        &gateway,
        json!({"tool_id": echo_id, "consumer_group_id": team_id, "allowed": true}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (_, body) = gateway
        .rpc(
            &session,
            json!(3),
            "tools/call",
            json!({"name": "remote__echo", "arguments": {"message": "group"}}),
            Some(&token),
        )
        .await;
    assert_eq!(body["result"]["content"][0]["text"], "group");

    // A token without the mapped group does not match the group ACL.
    let now = epoch_now();
    let outsider = mint_token(
        "test-kid",
        OIDC_ISSUER,
        Some(OIDC_AUDIENCE),
        "oidc-user",
        json!(["other"]),
        "tools:call",
        now + 3600,
        now - 10,
    );
    let session = gateway.open_session(Some(&outsider)).await;
    let (_, body) = gateway
        .rpc(
            &session,
            json!(4),
            "tools/call",
            json!({"name": "remote__echo"}),
            Some(&outsider),
        )
        .await;
    assert_eq!(body["error"]["code"], -32005);
}

#[tokio::test]
async fn oidc_rejects_invalid_tokens_and_reports_auth_errors() {
    let upstream = start_upstream(UpstreamMode::Json).await;
    let gateway = Gateway::new().await;
    let jwks = start_jwks(test_jwks("test-kid")).await;
    gateway.add_http_server("remote", &upstream).await;
    let (status, strategy) = post_oidc_strategy(
        &gateway,
        "default-sso",
        &jwks.url(),
        Some(OIDC_AUDIENCE),
        true,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{strategy}");

    // Missing bearer returns 401 with a challenge.
    let (status, headers, _) = gateway
        .post_mcp(
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18"}}),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(
        headers
            .get("www-authenticate")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("Bearer"))
    );

    let now = epoch_now();
    let cases = [
        (
            mint_token(
                "test-kid",
                OIDC_ISSUER,
                Some(OIDC_AUDIENCE),
                "oidc-user",
                json!(["team"]),
                "tools:call",
                now - 3600,
                now - 3700,
            ),
            StatusCode::UNAUTHORIZED,
        ),
        (
            mint_token(
                "test-kid",
                "https://other-issuer.example.test",
                Some(OIDC_AUDIENCE),
                "oidc-user",
                json!(["team"]),
                "tools:call",
                now + 3600,
                now - 10,
            ),
            StatusCode::UNAUTHORIZED,
        ),
        (
            mint_token(
                "test-kid",
                OIDC_ISSUER,
                Some("other-audience"),
                "oidc-user",
                json!(["team"]),
                "tools:call",
                now + 3600,
                now - 10,
            ),
            StatusCode::UNAUTHORIZED,
        ),
        (
            mint_token(
                "other-kid",
                OIDC_ISSUER,
                Some(OIDC_AUDIENCE),
                "oidc-user",
                json!(["team"]),
                "tools:call",
                now + 3600,
                now - 10,
            ),
            StatusCode::UNAUTHORIZED,
        ),
        (
            mint_token(
                "test-kid",
                OIDC_ISSUER,
                Some(OIDC_AUDIENCE),
                "oidc-user",
                json!(["team"]),
                "tools:list",
                now + 3600,
                now - 10,
            ),
            StatusCode::FORBIDDEN,
        ),
        (
            mint_token(
                "test-kid",
                OIDC_ISSUER,
                Some(OIDC_AUDIENCE),
                "oidc-user",
                json!(["team"]),
                "tools:call",
                now + 3600,
                now + 600,
            ),
            StatusCode::UNAUTHORIZED,
        ),
    ];

    for (token, expected) in cases {
        let (status, _, body) = gateway
            .post_mcp(
                json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18"}}),
                Some(&token),
                None,
            )
            .await;
        assert_eq!(status, expected, "{body}");
    }

    // The unknown kid forced a cache refresh before being rejected.
    assert!(
        jwks.state.requests() >= 2,
        "unknown kid should refresh the JWKS once"
    );

    // The valid token still works after the rejected attempts.
    let token = valid_token();
    let session = gateway.open_session(Some(&token)).await;
    let (status, body) = gateway
        .rpc(&session, json!(2), "tools/list", json!({}), Some(&token))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn default_strategy_applies_globally_and_per_server_strategy_overrides_it() {
    let upstream = start_upstream(UpstreamMode::Json).await;
    let gateway = Gateway::new().await;
    let jwks = start_jwks(test_jwks("test-kid")).await;
    let server_id = gateway.add_http_server("remote", &upstream).await;

    let (status, default) = post_oidc_strategy(
        &gateway,
        "default-sso",
        &jwks.url(),
        Some("default-aud"),
        true,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{default}");
    let (status, per_server) = post_oidc_strategy(
        &gateway,
        "server-sso",
        &jwks.url(),
        Some("server-aud"),
        false,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{per_server}");
    let per_server_id = per_server["id"].as_str().unwrap();

    let (status, updated) = gateway
        .control(
            "PUT",
            &format!("/api/v1/mcp/servers/{server_id}"),
            json!({
                "name": "remote",
                "transport": "http",
                "url": upstream.url(),
                "auth_strategy_id": per_server_id,
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["auth_strategy_id"], per_server_id);

    let now = epoch_now();
    let default_token = mint_token(
        "test-kid",
        OIDC_ISSUER,
        Some("default-aud"),
        "default-user",
        json!(["team"]),
        "tools:call",
        now + 3600,
        now - 10,
    );
    let server_token = mint_token(
        "test-kid",
        OIDC_ISSUER,
        Some("server-aud"),
        "server-user",
        json!(["team"]),
        "tools:call",
        now + 3600,
        now - 10,
    );

    // Global `/mcp` uses the default strategy. The server explicitly pinned to
    // `server-sso` has a different effective strategy, so it is not exposed.
    let session = gateway.open_session(Some(&default_token)).await;
    let (status, body) = gateway
        .rpc(
            &session,
            json!(2),
            "tools/list",
            json!({}),
            Some(&default_token),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        tool_names(&body).is_empty(),
        "a server with a different auth strategy must not be visible globally: {body}"
    );
    let (status, body) = gateway
        .rpc(
            &session,
            json!(3),
            "tools/call",
            json!({"name": "remote__echo", "arguments": {"message": "must-not-run"}}),
            Some(&default_token),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["error"]["code"], -32002, "{body}");
    assert!(
        upstream.state.calls.lock().unwrap().is_empty(),
        "a server with a different auth strategy must not be callable globally: {body}"
    );
    let (status, _, _) = gateway.initialize_at("/mcp", Some(&server_token)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // The scoped endpoint uses the server strategy and keeps its tools.
    let session = gateway
        .open_session_at("/mcp/remote", Some(&server_token))
        .await;
    let (status, body) = gateway
        .rpc_at(
            "/mcp/remote",
            &session,
            json!(4),
            "tools/list",
            json!({}),
            Some(&server_token),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(tool_names(&body), vec!["remote__boom", "remote__echo"]);
    let (status, body) = gateway
        .rpc_at(
            "/mcp/remote",
            &session,
            json!(5),
            "tools/call",
            json!({"name": "remote__echo", "arguments": {"message": "scoped"}}),
            Some(&server_token),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["result"]["content"][0]["text"], "scoped");
    let (status, _, _) = gateway
        .initialize_at("/mcp/remote", Some(&default_token))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn global_endpoint_rejects_tool_calls_for_servers_with_a_different_auth_strategy() {
    let upstream = start_upstream(UpstreamMode::Json).await;
    let gateway = Gateway::new().await;
    let jwks = start_jwks(test_jwks("test-kid")).await;
    let server_id = gateway.add_http_server("remote", &upstream).await;

    let (status, default) = post_oidc_strategy(
        &gateway,
        "default-sso",
        &jwks.url(),
        Some("default-aud"),
        true,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{default}");
    let (status, per_server) = post_oidc_strategy(
        &gateway,
        "server-sso",
        &jwks.url(),
        Some("server-aud"),
        false,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{per_server}");
    let per_server_id = per_server["id"].as_str().unwrap();

    let (status, updated) = gateway
        .control(
            "PUT",
            &format!("/api/v1/mcp/servers/{server_id}"),
            json!({
                "name": "remote",
                "transport": "http",
                "url": upstream.url(),
                "auth_strategy_id": per_server_id,
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");

    let now = epoch_now();
    let default_token = mint_token(
        "test-kid",
        OIDC_ISSUER,
        Some("default-aud"),
        "default-user",
        json!(["team"]),
        "tools:call",
        now + 3600,
        now - 10,
    );

    let session = gateway.open_session(Some(&default_token)).await;
    let (status, body) = gateway
        .rpc(
            &session,
            json!(2),
            "tools/list",
            json!({}),
            Some(&default_token),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(tool_names(&body).is_empty(), "{body}");

    // The remote tool must behave exactly like an unknown tool: no upstream
    // call is made and the JSON-RPC error is the not-found code.
    let (status, body) = gateway
        .rpc(
            &session,
            json!(3),
            "tools/call",
            json!({"name": "remote__echo", "arguments": {"message": "must-not-run"}}),
            Some(&default_token),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["error"]["code"], -32002, "{body}");
    assert!(
        upstream.state.calls.lock().unwrap().is_empty(),
        "an excluded server must not receive the call: {body}"
    );

    // The server-scoped endpoint still uses the server strategy.
    let server_token = mint_token(
        "test-kid",
        OIDC_ISSUER,
        Some("server-aud"),
        "server-user",
        json!(["team"]),
        "tools:call",
        now + 3600,
        now - 10,
    );
    let scoped = gateway
        .open_session_at("/mcp/remote", Some(&server_token))
        .await;
    let (status, body) = gateway
        .rpc_at(
            "/mcp/remote",
            &scoped,
            json!(4),
            "tools/call",
            json!({"name": "remote__echo", "arguments": {"message": "scoped"}}),
            Some(&server_token),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["result"]["content"][0]["text"], "scoped");
}

#[tokio::test]
async fn disabled_strategy_falls_back_to_api_keys() {
    let upstream = start_upstream(UpstreamMode::Json).await;
    let gateway = Gateway::new().await;
    let jwks = start_jwks(test_jwks("test-kid")).await;
    gateway.add_http_server("remote", &upstream).await;

    let (status, strategy) = post_oidc_strategy(
        &gateway,
        "disabled-sso",
        &jwks.url(),
        Some(OIDC_AUDIENCE),
        true,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{strategy}");
    assert_eq!(strategy["enabled"], false);

    let key = create_key(&gateway, "api-key-fallback").await;
    let token = valid_token();

    // The disabled strategy is ignored, so a JWT is treated as an API key.
    let (status, _, _) = gateway
        .post_mcp(
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18"}}),
            Some(&token),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // API keys still authenticate the endpoint.
    let session = gateway.open_session(Some(&key)).await;
    let (status, body) = gateway
        .rpc(
            &session,
            json!(2),
            "tools/call",
            json!({"name": "remote__echo", "arguments": {"message": "key"}}),
            Some(&key),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["result"]["content"][0]["text"], "key");
}

#[tokio::test]
async fn oidc_jwks_failures_return_service_unavailable() {
    let upstream = start_upstream(UpstreamMode::Json).await;
    let gateway = Gateway::new().await;
    let server_id = gateway.add_http_server("remote", &upstream).await;

    let (status, strategy) = post_oidc_strategy(
        &gateway,
        "unreachable-sso",
        "http://127.0.0.1:9/.well-known/jwks.json",
        Some(OIDC_AUDIENCE),
        false,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{strategy}");
    let strategy_id = strategy["id"].as_str().unwrap();

    let (status, updated) = gateway
        .control(
            "PUT",
            &format!("/api/v1/mcp/servers/{server_id}"),
            json!({
                "name": "remote",
                "transport": "http",
                "url": upstream.url(),
                "auth_strategy_id": strategy_id,
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");

    let (status, _, body) = gateway
        .initialize_at("/mcp/remote", Some(&valid_token()))
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"]["code"], "jwks_unavailable");
    assert!(body["error"]["message"].as_str().unwrap().contains("JWKS"));
}

#[tokio::test]
async fn oidc_discovery_supplies_jwks_and_refreshes_on_kid_miss() {
    let jwks = start_jwks(test_jwks("test-kid")).await;
    let discovery = start_discovery(json!({
        "issuer": OIDC_ISSUER,
        "jwks_uri": jwks.url(),
    }))
    .await;
    let gateway = Gateway::new().await;
    let config = discovery_oidc_config(&discovery.url(), Some(OIDC_AUDIENCE));
    let (status, strategy) =
        post_strategy_config(&gateway, "discovery-sso", config, true, true).await;
    assert_eq!(status, StatusCode::CREATED, "{strategy}");

    let token = valid_token();
    let (status, _, body) = gateway.initialize(Some(&token)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _, body) = gateway.initialize(Some(&token)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        discovery.state.requests(),
        1,
        "the discovery document should be cached"
    );

    // A key rotation makes the cached JWKS miss; the gateway refreshes the
    // discovery document and JWKS once.
    jwks.state.set(test_jwks("rotated-kid"));
    let now = epoch_now();
    let rotated = mint_token(
        "rotated-kid",
        OIDC_ISSUER,
        Some(OIDC_AUDIENCE),
        "oidc-user",
        json!(["team"]),
        "tools:call",
        now + 3600,
        now - 10,
    );
    let (status, _, body) = gateway.initialize(Some(&rotated)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        discovery.state.requests(),
        2,
        "a failed key lookup should refresh discovery once"
    );

    let (status, _, body) = gateway.initialize(Some(&rotated)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        discovery.state.requests(),
        2,
        "the refreshed document should be cached again"
    );
}

#[tokio::test]
async fn oidc_discovery_cache_expires_after_ttl() {
    let jwks = start_jwks(test_jwks("test-kid")).await;
    let discovery = start_discovery(json!({
        "issuer": OIDC_ISSUER,
        "jwks_uri": jwks.url(),
    }))
    .await;
    let gateway = Gateway::new().await;
    let mut config = discovery_oidc_config(&discovery.url(), Some(OIDC_AUDIENCE));
    config["jwks_cache_secs"] = json!(1);
    let (status, strategy) =
        post_strategy_config(&gateway, "discovery-ttl", config, true, true).await;
    assert_eq!(status, StatusCode::CREATED, "{strategy}");

    let token = valid_token();
    let (status, _, body) = gateway.initialize(Some(&token)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(discovery.state.requests(), 1);

    tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;

    let (status, _, body) = gateway.initialize(Some(&token)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        discovery.state.requests(),
        2,
        "an expired discovery document should be fetched again"
    );
}

#[tokio::test]
async fn oidc_discovery_supplies_introspection_endpoint() {
    let discovery = start_discovery(json!({
        "issuer": OIDC_ISSUER,
        "introspection_endpoint": "",
    }))
    .await;
    let introspection = start_introspection().await;
    discovery.state.set(json!({
        "issuer": OIDC_ISSUER,
        "introspection_endpoint": introspection.url(),
    }));
    let secret_ref = oidc_client_secret_ref("discovered-introspection");
    let gateway = Gateway::new().await;
    let config = json!({
        "issuer": OIDC_ISSUER,
        "discovery_url": discovery.url(),
        "token_validation": "introspection",
        "required_scopes": ["tools:call"],
        "audience": OIDC_AUDIENCE,
        "introspection": {
            "client_id": "octra-test-client",
            "client_secret_ref": secret_ref,
        },
    });
    let (status, strategy) =
        post_strategy_config(&gateway, "discovered-introspection", config, true, true).await;
    assert_eq!(status, StatusCode::CREATED, "{strategy}");

    let now = epoch_now();
    introspection.state.set_response(
        "opaque-discovered",
        introspection_claims(now + 3600, Some(OIDC_AUDIENCE), "tools:call"),
    );
    let (status, _, body) = gateway.initialize(Some("opaque-discovered")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(discovery.state.requests(), 1);
    assert_eq!(introspection.state.requests(), 1);
}

#[tokio::test]
async fn oidc_discovery_is_derived_from_the_issuer() {
    let introspection = start_introspection().await;
    let discovery = start_discovery(json!({
        "introspection_endpoint": introspection.url(),
    }))
    .await;
    let issuer = format!("http://{}", discovery.addr);
    discovery.state.set(json!({
        "issuer": issuer,
        "introspection_endpoint": introspection.url(),
    }));
    let secret_ref = oidc_client_secret_ref("derived-discovery");
    let gateway = Gateway::new().await;
    let config = json!({
        "issuer": issuer,
        "token_validation": "introspection",
        "required_scopes": ["tools:call"],
        "audience": OIDC_AUDIENCE,
        "introspection": {
            "client_id": "octra-test-client",
            "client_secret_ref": secret_ref,
        },
    });
    let (status, strategy) =
        post_strategy_config(&gateway, "derived-discovery", config, true, true).await;
    assert_eq!(status, StatusCode::CREATED, "{strategy}");

    let now = epoch_now();
    introspection.state.set_response(
        "opaque-derived",
        introspection_claims(now + 3600, Some(OIDC_AUDIENCE), "tools:call"),
    );
    let (status, _, body) = gateway.initialize(Some("opaque-derived")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(discovery.state.requests(), 1);
    assert_eq!(introspection.state.requests(), 1);
}

#[tokio::test]
async fn oidc_introspection_active_token_authenticates_maps_claims_and_caches() {
    let upstream = start_upstream(UpstreamMode::Json).await;
    let gateway = Gateway::new().await;
    let introspection = start_introspection().await;
    let secret_ref = oidc_client_secret_ref("active");
    let server_id = gateway.add_http_server("remote", &upstream).await;
    let team_id = create_consumer_group(&gateway, "team", &[]).await;
    let config = introspection_oidc_config(&introspection.url(), &secret_ref, Some(OIDC_AUDIENCE));
    let (status, strategy) =
        post_strategy_config(&gateway, "introspection-sso", config, true, true).await;
    assert_eq!(status, StatusCode::CREATED, "{strategy}");

    let now = epoch_now();
    introspection.state.set_response(
        "opaque-active",
        introspection_claims(now + 3600, Some(OIDC_AUDIENCE), "tools:call tools:list"),
    );

    let echo_id = find_tool_id(&gateway, &server_id, "echo").await;
    let (status, _) = set_acl(&gateway, json!({"tool_id": echo_id, "allowed": false})).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = set_acl(
        &gateway,
        json!({"tool_id": echo_id, "consumer_group_id": team_id, "allowed": true}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let session = gateway.open_session(Some("opaque-active")).await;
    let (status, body) = gateway
        .rpc(
            &session,
            json!(2),
            "tools/call",
            json!({"name": "remote__echo", "arguments": {"message": "opaque"}}),
            Some("opaque-active"),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["result"]["content"][0]["text"], "opaque");

    let invocations = gateway.storage.mcp.list_invocations(5, None).await.unwrap();
    let call = invocations
        .iter()
        .find(|row| row.status == "ok")
        .expect("call is recorded");
    assert_eq!(call.consumer_id.as_deref(), Some("opaque-user"));

    // Cached introspection reuses the active result without calling the
    // server again.
    let (status, body) = gateway
        .rpc(
            &session,
            json!(3),
            "tools/list",
            json!({}),
            Some("opaque-active"),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(introspection.state.requests(), 1);
}

#[tokio::test]
async fn oidc_introspection_rejects_inactive_expired_audience_and_scope() {
    let gateway = Gateway::new().await;
    let introspection = start_introspection().await;
    let secret_ref = oidc_client_secret_ref("rejections");
    let config = introspection_oidc_config(&introspection.url(), &secret_ref, Some(OIDC_AUDIENCE));
    let (status, strategy) =
        post_strategy_config(&gateway, "introspection-rejections", config, true, true).await;
    assert_eq!(status, StatusCode::CREATED, "{strategy}");

    let now = epoch_now();
    introspection
        .state
        .set_response("opaque-inactive", json!({"active": false}));
    introspection.state.set_response(
        "opaque-expired",
        introspection_claims(now - 10, Some(OIDC_AUDIENCE), "tools:call"),
    );
    introspection.state.set_response(
        "opaque-wrong-audience",
        introspection_claims(now + 3600, Some("other-audience"), "tools:call"),
    );
    introspection.state.set_response(
        "opaque-missing-scope",
        introspection_claims(now + 3600, Some(OIDC_AUDIENCE), "tools:list"),
    );
    introspection
        .state
        .set_response("opaque-malformed", json!({"sub": "opaque-user"}));

    for (token, expected) in [
        ("opaque-inactive", StatusCode::UNAUTHORIZED),
        ("opaque-expired", StatusCode::UNAUTHORIZED),
        ("opaque-wrong-audience", StatusCode::UNAUTHORIZED),
        ("opaque-missing-scope", StatusCode::FORBIDDEN),
        ("opaque-malformed", StatusCode::SERVICE_UNAVAILABLE),
    ] {
        let (status, _, body) = gateway.initialize(Some(token)).await;
        assert_eq!(status, expected, "{token}: {body}");
    }

    // Inactive results are cached too.
    let requests = introspection.state.requests();
    let (status, _, body) = gateway.initialize(Some("opaque-inactive")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(introspection.state.requests(), requests);
}

#[tokio::test]
async fn oidc_introspection_uses_basic_auth_and_falls_back_to_form() {
    let gateway = Gateway::new().await;
    let introspection = start_introspection().await;
    let secret_ref = oidc_client_secret_ref("basic-form");
    let config = introspection_oidc_config(&introspection.url(), &secret_ref, Some(OIDC_AUDIENCE));
    let (status, strategy) =
        post_strategy_config(&gateway, "introspection-auth", config, true, true).await;
    assert_eq!(status, StatusCode::CREATED, "{strategy}");

    let now = epoch_now();
    introspection.state.set_response(
        "opaque-basic",
        introspection_claims(now + 3600, Some(OIDC_AUDIENCE), "tools:call"),
    );
    let (status, _, body) = gateway.initialize(Some("opaque-basic")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        introspection
            .state
            .last_authorization()
            .as_deref()
            .is_some_and(|value| value.starts_with("Basic ")),
        "Basic auth should be preferred"
    );
    assert_eq!(introspection.state.requests(), 1);

    // The endpoint rejects Basic, so the gateway retries with form fields.
    introspection.state.require_form_auth(true);
    introspection.state.set_response(
        "opaque-form",
        introspection_claims(now + 3600, Some(OIDC_AUDIENCE), "tools:call"),
    );
    let (status, _, body) = gateway.initialize(Some("opaque-form")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(introspection.state.requests(), 3);
    let form = introspection.state.last_form().expect("form body recorded");
    assert_eq!(
        form.get("client_id").map(String::as_str),
        Some("octra-test-client")
    );
    assert_eq!(
        form.get("client_secret").map(String::as_str),
        Some("test-client-secret")
    );

    let (status, _, body) = gateway.initialize(Some("opaque-form")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        introspection.state.requests(),
        3,
        "cached results should not call the server again"
    );
}

#[tokio::test]
async fn oidc_introspection_network_failure_returns_service_unavailable() {
    let gateway = Gateway::new().await;
    let secret_ref = oidc_client_secret_ref("network-failure");
    let config = introspection_oidc_config(
        "http://127.0.0.1:9/introspect",
        &secret_ref,
        Some(OIDC_AUDIENCE),
    );
    let (status, strategy) =
        post_strategy_config(&gateway, "introspection-network", config, true, true).await;
    assert_eq!(status, StatusCode::CREATED, "{strategy}");

    let (status, _, body) = gateway.initialize(Some("opaque-network")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
}

#[tokio::test]
async fn oidc_auto_mode_uses_jwks_for_jwts_and_introspection_for_opaque_tokens() {
    let gateway = Gateway::new().await;
    let jwks = start_jwks(test_jwks("test-kid")).await;
    let introspection = start_introspection().await;
    let secret_ref = oidc_client_secret_ref("auto");
    let mut config = oidc_config(&jwks.url(), Some(OIDC_AUDIENCE));
    config["token_validation"] = json!("auto");
    config["introspection"] = json!({
        "endpoint": introspection.url(),
        "client_id": "octra-test-client",
        "client_secret_ref": secret_ref,
        "cache_secs": 60,
    });
    let (status, strategy) = post_strategy_config(&gateway, "auto-sso", config, true, true).await;
    assert_eq!(status, StatusCode::CREATED, "{strategy}");

    let token = valid_token();
    let (status, _, body) = gateway.initialize(Some(&token)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        introspection.state.requests(),
        0,
        "JWT tokens should use JWKS in auto mode"
    );

    let now = epoch_now();
    introspection.state.set_response(
        "opaque-auto",
        introspection_claims(now + 3600, Some(OIDC_AUDIENCE), "tools:call"),
    );
    let (status, _, body) = gateway.initialize(Some("opaque-auto")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(introspection.state.requests(), 1);

    // A JWT-shaped value with an unsupported header also falls back.
    introspection.state.set_response(
        "not.a.jwt",
        introspection_claims(now + 3600, Some(OIDC_AUDIENCE), "tools:call"),
    );
    let (status, _, body) = gateway.initialize(Some("not.a.jwt")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(introspection.state.requests(), 2);
}

// ----- OpenAPI conversion modes -----------------------------------------

fn conversion_openapi_document(server_url: &str) -> String {
    json!({
        "openapi": "3.0.3",
        "info": {"title": "Pets", "version": "1.0.0"},
        "servers": [{"url": server_url}],
        "paths": {
            "/pets/{petId}": {
                "get": {
                    "operationId": "getPet",
                    "summary": "Get a pet",
                    "parameters": [
                        {
                            "name": "petId",
                            "in": "path",
                            "required": true,
                            "schema": {"type": "string"}
                        },
                        {
                            "name": "tag",
                            "in": "query",
                            "schema": {"type": "array", "items": {"type": "string"}}
                        },
                        {
                            "name": "X-Tenant",
                            "in": "header",
                            "required": true,
                            "schema": {"type": "string"}
                        }
                    ],
                    "responses": {"200": {"description": "ok"}}
                }
            },
            "/pets": {
                "post": {
                    "operationId": "createPet",
                    "requestBody": {
                        "required": true,
                        "content": {
                            "application/json": {
                                "schema": {
                                    "type": "object",
                                    "properties": {"name": {"type": "string"}}
                                }
                            }
                        }
                    },
                    "responses": {"201": {"description": "created"}}
                }
            },
            "/text": {
                "get": {
                    "operationId": "getText",
                    "responses": {"200": {"description": "ok"}}
                }
            },
            "/fail": {
                "get": {
                    "operationId": "fail",
                    "responses": {"500": {"description": "failed"}}
                }
            }
        }
    })
    .to_string()
}

fn single_operation_document(server_url: &str, operation_id: &str, path: &str) -> String {
    let mut paths = serde_json::Map::new();
    paths.insert(
        path.to_string(),
        json!({
            "get": {
                "operationId": operation_id,
                "responses": {"200": {"description": "ok"}}
            }
        }),
    );
    json!({
        "openapi": "3.0.3",
        "info": {"title": "Single", "version": "1.0.0"},
        "servers": [{"url": server_url}],
        "paths": paths
    })
    .to_string()
}

async fn create_conversion_server(
    gateway: &Gateway,
    name: &str,
    mode: &str,
    document: String,
    auth: Option<serde_json::Value>,
) -> serde_json::Value {
    let mut openapi = json!({"document": document});
    if let Some(auth) = auth {
        openapi["auth"] = auth;
    }
    let (status, body) = gateway
        .control(
            "POST",
            "/api/v1/mcp/servers",
            json!({
                "name": name,
                "type": mode,
                "config": {"openapi": openapi},
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body
}

async fn conversion_tool_names(gateway: &Gateway, server_id: &str) -> Vec<String> {
    let (status, body) = gateway
        .control(
            "GET",
            &format!("/api/v1/mcp/tools?server_id={server_id}"),
            json!(null),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body.as_array()
        .expect("tool list")
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn conversion_listener_generates_lists_and_executes_http_tools() {
    let api = start_api_mock().await;
    let gateway = Gateway::new().await;
    let document = conversion_openapi_document(&format!("{}/api", api.url()));
    let server =
        create_conversion_server(&gateway, "pets", "conversion-listener", document, None).await;
    assert_eq!(server["type"], "conversion-listener");
    assert_eq!(server["tool_count"], 4, "{server}");
    let server_id = server["id"].as_str().unwrap();

    let (status, tools) = gateway
        .control(
            "GET",
            &format!("/api/v1/mcp/tools?server_id={server_id}"),
            json!(null),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{tools}");
    let get = tools
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "getPet")
        .expect("getPet tool");
    assert_eq!(get["kind"], "http");
    assert_eq!(get["http_method"], "GET");
    assert_eq!(get["http_path"], "/pets/{petId}");
    assert_eq!(get["schema"]["properties"]["path_petId"]["type"], "string");
    assert_eq!(
        get["schema"]["properties"]["header_X-Tenant"]["type"],
        "string"
    );

    let session = gateway.open_session(None).await;
    let (_, body) = gateway
        .rpc(&session, json!(2), "tools/list", json!({}), None)
        .await;
    let names = tool_names(&body);
    for expected in [
        "pets__createPet",
        "pets__fail",
        "pets__getPet",
        "pets__getText",
    ] {
        assert!(names.contains(&expected.to_string()), "{names:?}");
    }

    let (status, body) = gateway
        .rpc(
            &session,
            json!(3),
            "tools/call",
            json!({
                "name": "pets__getPet",
                "arguments": {
                    "path_petId": "a b",
                    "query_tag": ["one", "two"],
                    "header_X-Tenant": "team-a"
                }
            }),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let structured = &body["result"]["structuredContent"];
    assert_eq!(structured["method"], "GET");
    assert_eq!(structured["path"], "/api/pets/a%20b");
    assert!(structured["query"].as_str().unwrap().contains("tag=one"));
    assert!(structured["query"].as_str().unwrap().contains("tag=two"));
    assert_eq!(structured["x_tenant"], "team-a");
    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    assert_eq!(text, serde_json::to_string(structured).unwrap());

    let (status, body) = gateway
        .rpc(
            &session,
            json!(4),
            "tools/call",
            json!({
                "name": "pets__createPet",
                "arguments": {"body": {"name": "Fido"}}
            }),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["result"]["structuredContent"]["method"], "POST");
    assert_eq!(body["result"]["structuredContent"]["path"], "/api/pets");
    assert_eq!(body["result"]["structuredContent"]["body"]["name"], "Fido");

    let requests = api.state.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[1].body["name"], "Fido");
}

#[tokio::test]
async fn conversion_auth_non_2xx_and_text_responses_are_mapped() {
    let api = start_api_mock().await;
    let gateway = Gateway::new().await;
    let secret_path = std::env::temp_dir().join(format!(
        "octra-openapi-it-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    std::fs::write(&secret_path, "openapi-secret-token\n").unwrap();
    let secret_ref = format!("file:{}", secret_path.display());
    let document = conversion_openapi_document(&api.url());

    let server = create_conversion_server(
        &gateway,
        "secured",
        "conversion-listener",
        document.clone(),
        Some(json!({"type": "bearer", "secret_ref": secret_ref})),
    )
    .await;
    assert_eq!(server["tool_count"], 4, "{server}");
    let rendered = server.to_string();
    assert!(!rendered.contains("openapi-secret-token"), "{rendered}");
    assert!(!rendered.contains("secret_ref"), "{rendered}");

    let server_id = server["id"].as_str().unwrap();
    let (_, tools) = gateway
        .control(
            "GET",
            &format!("/api/v1/mcp/tools?server_id={server_id}"),
            json!(null),
        )
        .await;
    assert!(!tools.to_string().contains("secret_ref"), "{tools}");

    let session = gateway.open_session(None).await;
    let (_, body) = gateway
        .rpc(
            &session,
            json!(2),
            "tools/call",
            json!({
                "name": "secured__getPet",
                "arguments": {
                    "path_petId": "1",
                    "header_X-Tenant": "team-a"
                }
            }),
            None,
        )
        .await;
    assert_eq!(
        body["result"]["structuredContent"]["authorization"],
        "Bearer openapi-secret-token"
    );

    let (_, body) = gateway
        .rpc(
            &session,
            json!(3),
            "tools/call",
            json!({"name": "secured__fail"}),
            None,
        )
        .await;
    assert_eq!(body["result"]["isError"], true, "{body}");
    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("HTTP 500"), "{text}");
    assert!(text.contains("upstream failed"), "{text}");

    let (_, body) = gateway
        .rpc(
            &session,
            json!(4),
            "tools/call",
            json!({"name": "secured__getText"}),
            None,
        )
        .await;
    assert_eq!(body["result"]["content"][0]["text"], "plain body");
    assert!(body["result"].get("structuredContent").is_none(), "{body}");

    let query_server = create_conversion_server(
        &gateway,
        "query-auth",
        "conversion-listener",
        document,
        Some(json!({
            "type": "query",
            "secret_ref": secret_ref,
            "param": "token"
        })),
    )
    .await;
    assert_eq!(query_server["tool_count"], 4);
    let (_, body) = gateway
        .rpc(
            &session,
            json!(5),
            "tools/call",
            json!({
                "name": "query-auth__getPet",
                "arguments": {"path_petId": "2", "header_X-Tenant": "team-b"}
            }),
            None,
        )
        .await;
    assert_eq!(body["result"]["structuredContent"]["method"], "GET");
    let requests = api.state.requests.lock().unwrap().clone();
    assert!(
        requests
            .last()
            .unwrap()
            .query
            .contains("token=openapi-secret-token")
    );

    let _ = std::fs::remove_file(secret_path);
}

#[tokio::test]
async fn conversion_only_tools_are_sourced_by_listeners_only() {
    let api = start_api_mock().await;
    let gateway = Gateway::new().await;
    let server = create_conversion_server(
        &gateway,
        "openapi-source",
        "conversion-only",
        conversion_openapi_document(&api.url()),
        None,
    )
    .await;
    let server_id = server["id"].as_str().unwrap().to_string();

    let session = gateway.open_session(None).await;
    let (_, body) = gateway
        .rpc(&session, json!(2), "tools/list", json!({}), None)
        .await;
    assert!(
        tool_names(&body).is_empty(),
        "conversion-only tools must not appear without a listener: {body}"
    );

    let (status, _, body) = gateway.initialize_at("/mcp/openapi-source", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("conversion-only")
    );

    gateway.add_listener("front", &["openapi-source"]).await;
    let (_, body) = gateway
        .rpc(&session, json!(3), "tools/list", json!({}), None)
        .await;
    assert!(
        tool_names(&body).contains(&"openapi-source__getPet".to_string()),
        "{body}"
    );

    let (_, body) = gateway
        .rpc(
            &session,
            json!(4),
            "tools/call",
            json!({
                "name": "openapi-source__getPet",
                "arguments": {
                    "path_petId": "77",
                    "header_X-Tenant": "team-c"
                }
            }),
            None,
        )
        .await;
    assert_eq!(body["result"]["structuredContent"]["path"], "/pets/77");
    assert_eq!(
        gateway
            .storage
            .mcp
            .list_tools(&server_id)
            .await
            .unwrap()
            .len(),
        4
    );
}

#[tokio::test]
async fn conversion_refresh_replaces_tools_and_manual_mutation_is_rejected() {
    let api = start_api_mock().await;
    let gateway = Gateway::new().await;
    let document = single_operation_document(&api.url(), "first", "/one");
    let server = create_conversion_server(
        &gateway,
        "refreshable",
        "conversion-listener",
        document,
        None,
    )
    .await;
    let server_id = server["id"].as_str().unwrap().to_string();
    assert_eq!(
        conversion_tool_names(&gateway, &server_id).await,
        vec!["first"]
    );

    let updated_document = single_operation_document(&api.url(), "second", "/two");
    let (status, updated) = gateway
        .control(
            "PUT",
            &format!("/api/v1/mcp/servers/{server_id}"),
            json!({
                "name": "refreshable",
                "config": {"openapi": {"document": updated_document}}
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(
        conversion_tool_names(&gateway, &server_id).await,
        vec!["second"]
    );

    // Omitting config keeps the stored document, and refresh re-parses it.
    let (status, kept) = gateway
        .control(
            "PUT",
            &format!("/api/v1/mcp/servers/{server_id}"),
            json!({"name": "refreshable"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{kept}");
    assert_eq!(kept["tool_count"], 1, "{kept}");
    let (status, stats) = gateway
        .control(
            "POST",
            &format!("/api/v1/mcp/servers/{server_id}/refresh"),
            json!(null),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{stats}");
    assert_eq!(stats["tools"], 1, "{stats}");
    assert_eq!(
        conversion_tool_names(&gateway, &server_id).await,
        vec!["second"]
    );

    let (status, body) = gateway
        .control(
            "POST",
            "/api/v1/mcp/tools",
            json!({
                "server_id": server_id,
                "name": "manual",
                "description": "manual",
            }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("generated from config.openapi")
    );

    let tool_id = gateway
        .storage
        .mcp
        .list_tools(&server_id)
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap()
        .id;
    let status = gateway
        .control_delete(&format!("/api/v1/mcp/tools/{tool_id}"))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        conversion_tool_names(&gateway, &server_id).await,
        vec!["second"]
    );
}

#[tokio::test]
async fn conversion_document_url_is_fetched_at_create_time() {
    let api = start_api_mock().await;
    let gateway = Gateway::new().await;
    let (status, server) = gateway
        .control(
            "POST",
            "/api/v1/mcp/servers",
            json!({
                "name": "fetched",
                "type": "conversion-only",
                "config": {
                    "openapi": {"document_url": format!("{}/openapi.json", api.url())}
                }
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{server}");
    assert_eq!(server["tool_count"], 1, "{server}");
    let server_id = server["id"].as_str().unwrap();
    assert_eq!(
        conversion_tool_names(&gateway, server_id).await,
        vec!["fetchedTool"]
    );
    assert_eq!(api.state.requests.lock().unwrap().len(), 1);
}

fn conversion_styles_document(server_url: &str) -> String {
    json!({
        "openapi": "3.0.3",
        "info": {"title": "Styles", "version": "1.0.0"},
        "servers": [{"url": server_url}],
        "paths": {
            "/styles/{id}": {
                "get": {
                    "operationId": "styleTool",
                    "parameters": [
                        {
                            "name": "id",
                            "in": "path",
                            "required": true,
                            "style": "matrix",
                            "explode": true,
                            "schema": {"type": "array", "items": {"type": "string"}}
                        },
                        {
                            "name": "filter",
                            "in": "query",
                            "style": "deepObject",
                            "schema": {"type": "object"}
                        },
                        {
                            "name": "letters",
                            "in": "query",
                            "style": "pipeDelimited",
                            "explode": false,
                            "schema": {"type": "array", "items": {"type": "string"}}
                        },
                        {
                            "name": "raw",
                            "in": "query",
                            "allowReserved": true,
                            "schema": {"type": "string"}
                        },
                        {
                            "name": "X-Flags",
                            "in": "header",
                            "schema": {"type": "array", "items": {"type": "string"}}
                        },
                        {
                            "name": "session",
                            "in": "cookie",
                            "required": true,
                            "schema": {"type": "string"}
                        },
                        {
                            "name": "prefs",
                            "in": "cookie",
                            "schema": {"type": "object"}
                        }
                    ],
                    "responses": {"200": {"description": "ok"}}
                }
            },
            "/form": {
                "post": {
                    "operationId": "formTool",
                    "requestBody": {
                        "required": true,
                        "content": {
                            "application/x-www-form-urlencoded": {
                                "schema": {"type": "object"}
                            }
                        }
                    },
                    "responses": {"200": {"description": "ok"}}
                }
            },
            "/multipart": {
                "post": {
                    "operationId": "multipartTool",
                    "requestBody": {
                        "required": true,
                        "content": {
                            "multipart/form-data": {
                                "schema": {
                                    "type": "object",
                                    "properties": {
                                        "name": {"type": "string"},
                                        "file": {"type": "string", "x-contentMediaType": "image/png"}
                                    }
                                }
                            }
                        }
                    },
                    "responses": {"200": {"description": "ok"}}
                }
            },
            "/text": {
                "post": {
                    "operationId": "textTool",
                    "requestBody": {
                        "required": true,
                        "content": {
                            "text/plain": {"schema": {"type": "string"}}
                        }
                    },
                    "responses": {"200": {"description": "ok"}}
                }
            }
        }
    })
    .to_string()
}

#[tokio::test]
async fn conversion_serializes_styles_cookies_and_non_json_bodies() {
    let api = start_api_mock().await;
    let gateway = Gateway::new().await;
    let server = create_conversion_server(
        &gateway,
        "styles",
        "conversion-listener",
        conversion_styles_document(&format!("{}/api", api.url())),
        None,
    )
    .await;
    assert_eq!(server["tool_count"], 4, "{server}");
    let session = gateway.open_session(None).await;

    let (status, body) = gateway
        .rpc(
            &session,
            json!(2),
            "tools/call",
            json!({
                "name": "styles__styleTool",
                "arguments": {
                    "path_id": ["a", "b"],
                    "query_filter": {"role": "admin"},
                    "query_letters": ["x", "y"],
                    "query_raw": "/v1/a b",
                    "header_X-Flags": ["a", "b"],
                    "cookie_session": "s1",
                    "cookie_prefs": {"theme": "dark"}
                }
            }),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let request = api.state.requests.lock().unwrap().last().cloned().unwrap();
    assert_eq!(request.path, "/api/styles/;id=a;id=b");
    assert!(
        request.query.contains("filter%5Brole%5D=admin"),
        "{}",
        request.query
    );
    assert!(request.query.contains("letters=x%7Cy"), "{}", request.query);
    assert!(request.query.contains("raw=/v1/a%20b"), "{}", request.query);
    assert_eq!(
        request
            .headers
            .get("x-flags")
            .and_then(|value| value.to_str().ok()),
        Some("a,b")
    );
    assert_eq!(
        request
            .headers
            .get("cookie")
            .and_then(|value| value.to_str().ok()),
        Some("session=s1; theme=dark")
    );

    let (status, body) = gateway
        .rpc(
            &session,
            json!(3),
            "tools/call",
            json!({
                "name": "styles__formTool",
                "arguments": {"body": {"name": "Fido", "tags": ["a", "b"]}}
            }),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let request = api.state.requests.lock().unwrap().last().cloned().unwrap();
    assert_eq!(
        request
            .headers
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("application/x-www-form-urlencoded")
    );
    let form_body = request.body.as_str().unwrap();
    assert!(form_body.contains("name=Fido"), "{form_body}");
    assert!(form_body.contains("tags=a"), "{form_body}");
    assert!(form_body.contains("tags=b"), "{form_body}");

    let (status, body) = gateway
        .rpc(
            &session,
            json!(4),
            "tools/call",
            json!({
                "name": "styles__multipartTool",
                "arguments": {
                    "body": {
                        "name": "Fido",
                        "file": {"content_base64": "aGk="}
                    }
                }
            }),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let request = api.state.requests.lock().unwrap().last().cloned().unwrap();
    let content_type = request
        .headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap();
    assert!(
        content_type.starts_with("multipart/form-data; boundary="),
        "{content_type}"
    );
    let multipart_body = request.body.as_str().unwrap();
    assert!(multipart_body.contains("name=\"name\""), "{multipart_body}");
    assert!(multipart_body.contains("Fido"), "{multipart_body}");
    assert!(multipart_body.contains("name=\"file\""), "{multipart_body}");
    assert!(
        multipart_body.contains("Content-Type: image/png"),
        "{multipart_body}"
    );
    assert!(multipart_body.contains("hi"), "{multipart_body}");

    let (status, body) = gateway
        .rpc(
            &session,
            json!(5),
            "tools/call",
            json!({
                "name": "styles__textTool",
                "arguments": {"body": "hello world"}
            }),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let request = api.state.requests.lock().unwrap().last().cloned().unwrap();
    assert_eq!(request.body.as_str(), Some("hello world"));
    assert_eq!(
        request
            .headers
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/plain")
    );

    let unsupported = json!({
        "openapi": "3.0.3",
        "info": {"title": "Unsupported", "version": "1"},
        "servers": [{"url": api.url()}],
        "paths": {
            "/xml": {
                "post": {
                    "operationId": "xmlTool",
                    "requestBody": {
                        "content": {"application/xml": {"schema": {"type": "object"}}}
                    },
                    "responses": {"200": {"description": "ok"}}
                }
            }
        }
    })
    .to_string();
    let (status, body) = gateway
        .control(
            "POST",
            "/api/v1/mcp/servers",
            json!({
                "name": "unsupported-body",
                "type": "conversion-only",
                "config": {"openapi": {"document": unsupported}}
            }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unsupported OpenAPI request body media types"),
        "{body}"
    );
}

#[tokio::test]
async fn conversion_resolves_local_and_http_external_refs() {
    let api = start_api_mock().await;
    let gateway = Gateway::new().await;

    let directory = std::env::temp_dir().join(format!(
        "octra-openapi-integration-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(
        directory.join("common.yaml"),
        r#"
paths:
  /local-ref:
    get:
      operationId: localRefTool
      parameters:
        - $ref: '#/components/parameters/Tag'
      responses:
        '200':
          description: ok
components:
  parameters:
    Tag:
      name: tag
      in: query
      schema:
        type: string
"#,
    )
    .unwrap();
    let local_document = json!({
        "openapi": "3.0.3",
        "info": {"title": "Local refs", "version": "1"},
        "servers": [{"url": "https://api.example.test"}],
        "paths": {
            "/local-ref": {"$ref": "./common.yaml#/paths/~1local-ref"}
        }
    })
    .to_string();
    let (status, server) = gateway
        .control(
            "POST",
            "/api/v1/mcp/servers",
            json!({
                "name": "local-ref",
                "type": "conversion-only",
                "config": {
                    "openapi": {
                        "document": local_document,
                        "document_base_dir": directory.display().to_string()
                    }
                }
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{server}");
    assert_eq!(
        conversion_tool_names(&gateway, server["id"].as_str().unwrap()).await,
        vec!["localRefTool"]
    );
    let _ = std::fs::remove_dir_all(directory);

    let (status, server) = gateway
        .control(
            "POST",
            "/api/v1/mcp/servers",
            json!({
                "name": "http-ref",
                "type": "conversion-only",
                "config": {
                    "openapi": {"document_url": format!("{}/openapi-relative.json", api.url())}
                }
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{server}");
    assert_eq!(
        conversion_tool_names(&gateway, server["id"].as_str().unwrap()).await,
        vec!["httpRefTool"]
    );
    let requested = api
        .state
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|request| request.path.clone())
        .collect::<Vec<_>>();
    assert!(
        requested.contains(&"/openapi-relative.json".to_string()),
        "{requested:?}"
    );
    assert!(
        requested.contains(&"/openapi-shared.json".to_string()),
        "{requested:?}"
    );

    let missing_ref_document = json!({
        "openapi": "3.0.3",
        "info": {"title": "Missing refs", "version": "1"},
        "servers": [{"url": "https://api.example.test"}],
        "paths": {
            "/missing": {
                "get": {
                    "operationId": "missingRef",
                    "parameters": [{
                        "$ref": format!(
                            "{}/missing-shared.json#/components/parameters/Tag",
                            api.url()
                        )
                    }],
                    "responses": {"200": {"description": "ok"}}
                }
            }
        }
    })
    .to_string();
    let (status, body) = gateway
        .control(
            "POST",
            "/api/v1/mcp/servers",
            json!({
                "name": "missing-ref",
                "type": "conversion-only",
                "config": {"openapi": {"document": missing_ref_document}}
            }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("cannot resolve external OpenAPI reference fragment"),
        "{body}"
    );
}

#[tokio::test]
async fn upstream_catalog_change_reaches_get_stream_and_refreshes_tools() {
    let upstream = start_upstream(UpstreamMode::Json).await;
    let gateway = Gateway::new().await;
    gateway.add_http_server("remote", &upstream).await;
    wait_for_idle_get(&upstream, Duration::from_secs(3)).await;
    let baseline_lists = upstream.state.tool_lists();

    let session = gateway.open_session(None).await;
    let (_, body) = gateway
        .rpc(
            &session,
            json!(2),
            "tools/call",
            json!({"name": "remote__echo", "arguments": {"message": "live"}}),
            None,
        )
        .await;
    assert_eq!(body["result"]["content"][0]["text"], "live", "{body}");
    wait_for_active_get(&upstream, 1, Duration::from_secs(5)).await;

    let mut stream = open_sse(&gateway, "/mcp", Some(&session)).await;
    upstream.state.notify(
        "notifications/tools/list_changed",
        json!({"source": "upstream"}),
    );

    let event = stream.expect_event(Duration::from_secs(5)).await;
    let notification: serde_json::Value = serde_json::from_str(&event.data).unwrap();
    assert_eq!(notification["method"], "notifications/tools/list_changed");
    assert_eq!(notification["params"]["source"], "upstream");

    // The gateway refreshes the catalog once, then publishes its own notice.
    wait_for_tool_lists(&upstream, baseline_lists + 1, Duration::from_secs(5)).await;
    let refreshed = stream.expect_event(Duration::from_secs(5)).await;
    let notification: serde_json::Value = serde_json::from_str(&refreshed.data).unwrap();
    assert_eq!(notification["method"], "notifications/tools/list_changed");
    assert!(notification.get("params").is_none(), "{notification}");

    let (_, body) = gateway
        .rpc(&session, json!(3), "tools/list", json!({}), None)
        .await;
    assert_eq!(tool_names(&body), vec!["remote__boom", "remote__echo"]);
}

#[tokio::test]
async fn scoped_get_stream_only_forwards_visible_server_notifications() {
    let first = start_upstream(UpstreamMode::Json).await;
    let second = start_upstream(UpstreamMode::Json).await;
    let gateway = Gateway::new().await;
    gateway.add_http_server("first", &first).await;
    gateway.add_http_server("second", &second).await;
    gateway.add_listener("front", &["first"]).await;
    wait_for_idle_get(&first, Duration::from_secs(3)).await;
    wait_for_idle_get(&second, Duration::from_secs(3)).await;

    // Open one live upstream connection per server through its own endpoint.
    for (name, message) in [("first", "one"), ("second", "two")] {
        let session = gateway.open_session_at(&format!("/mcp/{name}"), None).await;
        let (_, body) = gateway
            .rpc_at(
                &format!("/mcp/{name}"),
                &session,
                json!(2),
                "tools/call",
                json!({"name": format!("{name}__echo"), "arguments": {"message": message}}),
                None,
            )
            .await;
        assert_eq!(body["result"]["content"][0]["text"], message, "{body}");
    }
    wait_for_active_get(&first, 1, Duration::from_secs(5)).await;
    wait_for_active_get(&second, 1, Duration::from_secs(5)).await;

    let session = gateway.open_session_at("/mcp/front", None).await;
    let mut stream = open_sse(&gateway, "/mcp/front", Some(&session)).await;

    // The second server is not a source of the listener.
    second
        .state
        .notify("notifications/message", json!({"source": "second"}));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        stream
            .next_event(Duration::from_millis(100))
            .await
            .is_none()
    );

    first
        .state
        .notify("notifications/message", json!({"source": "first"}));
    let event = stream.expect_event(Duration::from_secs(5)).await;
    let notification: serde_json::Value = serde_json::from_str(&event.data).unwrap();
    assert_eq!(notification["method"], "notifications/message");
    assert_eq!(notification["params"]["source"], "first");
}

#[tokio::test]
async fn get_stream_stays_keepalive_only_without_a_valid_session() {
    let upstream = start_upstream(UpstreamMode::Json).await;
    let gateway = Gateway::new().await;
    gateway.add_http_server("remote", &upstream).await;

    let mut missing = open_sse(&gateway, "/mcp", None).await;
    let mut unknown = open_sse(&gateway, "/mcp", Some("unknown-session")).await;
    assert!(
        missing
            .next_event(Duration::from_millis(200))
            .await
            .is_none()
    );
    assert!(
        unknown
            .next_event(Duration::from_millis(200))
            .await
            .is_none()
    );
}

#[tokio::test]
async fn upstream_notification_stream_reconnects_after_close() {
    let upstream = start_upstream(UpstreamMode::Json).await;
    let gateway = Gateway::new().await;
    gateway.add_http_server("remote", &upstream).await;
    wait_for_idle_get(&upstream, Duration::from_secs(3)).await;

    // The next live GET stream closes after one event; the transport must
    // reconnect and keep delivering later notifications.
    upstream.state.close_get_after_notification(true);
    let session = gateway.open_session(None).await;
    let (_, body) = gateway
        .rpc(
            &session,
            json!(2),
            "tools/call",
            json!({"name": "remote__echo", "arguments": {"message": "live"}}),
            None,
        )
        .await;
    assert_eq!(body["result"]["content"][0]["text"], "live", "{body}");
    wait_for_active_get(&upstream, 1, Duration::from_secs(5)).await;
    let connections = upstream.state.get_connections();

    let mut stream = open_sse(&gateway, "/mcp", Some(&session)).await;
    upstream
        .state
        .notify("notifications/message", json!({"seq": 1}));

    let event = stream.expect_event(Duration::from_secs(5)).await;
    let notification: serde_json::Value = serde_json::from_str(&event.data).unwrap();
    assert_eq!(notification["params"]["seq"], 1);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
    while tokio::time::Instant::now() < deadline {
        if upstream.state.get_connections() > connections
            && upstream.state.active_get_streams() >= 1
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        upstream.state.get_connections() > connections,
        "the notification stream did not reconnect"
    );

    upstream
        .state
        .notify("notifications/message", json!({"seq": 2}));
    let event = stream.expect_event(Duration::from_secs(5)).await;
    let notification: serde_json::Value = serde_json::from_str(&event.data).unwrap();
    assert_eq!(notification["params"]["seq"], 2);
}

#[tokio::test]
async fn reload_stops_notification_forwarding_without_panicking() {
    let upstream = start_upstream(UpstreamMode::Json).await;
    let gateway = Gateway::new().await;
    gateway.add_http_server("remote", &upstream).await;
    wait_for_idle_get(&upstream, Duration::from_secs(3)).await;

    let session = gateway.open_session(None).await;
    let (_, body) = gateway
        .rpc(
            &session,
            json!(2),
            "tools/call",
            json!({"name": "remote__echo", "arguments": {"message": "live"}}),
            None,
        )
        .await;
    assert_eq!(body["result"]["content"][0]["text"], "live", "{body}");
    wait_for_active_get(&upstream, 1, Duration::from_secs(5)).await;

    let mut stream = open_sse(&gateway, "/mcp", Some(&session)).await;
    gateway.runtime.reload().await.unwrap();
    wait_for_idle_get(&upstream, Duration::from_secs(3)).await;

    upstream
        .state
        .notify("notifications/message", json!({"seq": 1}));
    assert!(
        stream
            .next_event(Duration::from_millis(300))
            .await
            .is_none()
    );

    // The session itself is still valid after a runtime reload.
    let (status, body) = gateway
        .rpc(&session, json!(3), "tools/list", json!({}), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn deleting_a_session_ends_open_notification_streams() {
    let upstream = start_upstream(UpstreamMode::Json).await;
    let gateway = Gateway::new().await;
    gateway.add_http_server("remote", &upstream).await;

    let session = gateway.open_session(None).await;
    let mut stream = open_sse(&gateway, "/mcp", Some(&session)).await;

    let request = axum::http::Request::builder()
        .method("DELETE")
        .uri("/mcp")
        .header("mcp-session-id", &session)
        .body(Body::empty())
        .unwrap();
    let response = tower::ServiceExt::oneshot(gateway.data_router(), request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    assert!(
        stream.body_ended(Duration::from_secs(3)).await,
        "the GET stream must end when its session is deleted"
    );
}

#[tokio::test]
async fn rejects_cross_origin_and_non_json_requests() {
    let gateway = Gateway::new().await;
    let initialize = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {"protocolVersion": "2025-06-18", "capabilities": {}}
    });

    let (status, body) = gateway
        .post_mcp_with_origin(initialize.clone(), "http://evil.example")
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"]["code"], "origin_not_allowed");

    let (status, body) = gateway
        .post_mcp_with_origin(initialize.clone(), "http://127.0.0.1:3000")
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["jsonrpc"], "2.0", "{body}");
    assert_ne!(body["error"]["code"], "origin_not_allowed", "{body}");

    let (status, _, body) = gateway
        .post_mcp_with_content_type(initialize, None, None, "text/plain")
        .await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{body}");
}

#[tokio::test]
async fn require_key_rejects_open_installations() {
    let mut config = OctraConfig::default();
    config.mcp.require_key = true;
    let gateway = Gateway::new_with_config(config).await;
    let (status, _, body) = gateway
        .post_mcp(
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {"protocolVersion": "2025-06-18", "capabilities": {}}
            }),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
}

#[tokio::test]
async fn disabling_the_last_api_key_does_not_reopen_the_endpoint() {
    let gateway = Gateway::new().await;
    let key = create_key(&gateway, "agent").await;
    let records = gateway.storage.mcp.list_api_keys().await.unwrap();
    assert_eq!(records.len(), 1);
    gateway
        .storage
        .mcp
        .set_api_key_enabled(&records[0].id, false)
        .await
        .unwrap();

    for presented in [None, Some(key.as_str())] {
        let (status, _, body) = gateway
            .post_mcp(
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {"protocolVersion": "2025-06-18", "capabilities": {}}
                }),
                presented,
                None,
            )
            .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    }
}
