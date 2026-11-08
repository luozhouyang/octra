//! Data plane behaviour against a mock Streamable HTTP MCP upstream.

mod support;

use axum::http::StatusCode;
use serde_json::json;
use support::{
    Gateway, UpstreamMode, create_consumer_group, create_key, create_keyed_consumer, start_upstream,
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
            .contains("expected 'upstream-server' or 'passthrough-listener'")
    );

    // Conversion modes are recognized Kong names but unimplemented here.
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
            .contains("conversion modes are out of scope")
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
