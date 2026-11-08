//! Data plane behaviour against a mock Streamable HTTP MCP upstream.

mod support;

use axum::http::StatusCode;
use octra_mcp_core::domain::default_server_mode;
use octra_mcp_core::{NewMcpServer, Transport};
use serde_json::json;
use support::{
    Gateway, UpstreamMode, create_consumer_group, create_key, create_keyed_consumer,
    sign_test_token, start_api_mock, start_jwks, start_upstream, test_jwks,
};

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

async fn post_oidc_strategy(
    gateway: &Gateway,
    name: &str,
    jwks_uri: &str,
    audience: Option<&str>,
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
                "config": oidc_config(jwks_uri, audience),
                "enabled": enabled,
                "is_default": is_default,
            }),
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

    // Global `/mcp` uses the default strategy.
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
    let (status, _, _) = gateway.initialize_at("/mcp", Some(&server_token)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // The scoped endpoint overrides it with the server strategy.
    let session = gateway
        .open_session_at("/mcp/remote", Some(&server_token))
        .await;
    let (status, body) = gateway
        .rpc_at(
            "/mcp/remote",
            &session,
            json!(3),
            "tools/list",
            json!({}),
            Some(&server_token),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _, _) = gateway
        .initialize_at("/mcp/remote", Some(&default_token))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
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
