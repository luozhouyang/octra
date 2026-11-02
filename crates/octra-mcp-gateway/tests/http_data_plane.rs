//! Data plane behaviour against a mock Streamable HTTP MCP upstream.

mod support;

use axum::http::StatusCode;
use serde_json::json;
use support::{Gateway, UpstreamMode, create_key, start_upstream};

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
    assert_eq!(status, StatusCode::OK);
    let (_, body) = gateway
        .rpc("no-such-session", json!(1), "tools/list", json!({}), None)
        .await;
    assert_eq!(body["error"]["code"], -32600);
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
    assert_eq!(body["error"]["code"], -32600);
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("session not found")
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
