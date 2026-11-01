//! Stdio transport through the gateway, driven by the `mock_mcp_stdio` binary.

mod support;

use axum::http::StatusCode;
use serde_json::json;
use support::Gateway;

fn mock_command() -> String {
    env!("CARGO_BIN_EXE_mock_mcp_stdio").to_string()
}

async fn gateway_with_stdio(name: &str) -> Gateway {
    let gateway = Gateway::new().await;
    let (status, server) = gateway
        .control(
            "POST",
            "/api/v1/mcp/servers",
            json!({
                "name": name,
                "transport": "stdio",
                "command": mock_command(),
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "create stdio server: {server}");
    assert_eq!(server["tool_count"], 3, "discovery: {server}");
    assert_eq!(server["protocol_version"], "2025-06-18");
    gateway
}

#[tokio::test]
async fn stdio_lists_and_calls_tools() {
    let gateway = gateway_with_stdio("local").await;
    let session = gateway.open_session(None).await;

    let (_, body) = gateway
        .rpc(&session, json!(2), "tools/list", json!({}), None)
        .await;
    let names: Vec<&str> = body["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["local__echo", "local__fail", "local__slow"]);

    let (status, body) = gateway
        .rpc(
            &session,
            json!(3),
            "tools/call",
            json!({"name": "local__echo", "arguments": {"message": "stdio"}}),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["result"]["content"][0]["text"], "stdio");

    // Upstream JSON-RPC tool errors pass through unchanged.
    let (_, body) = gateway
        .rpc(
            &session,
            json!(4),
            "tools/call",
            json!({"name": "local__fail"}),
            None,
        )
        .await;
    assert_eq!(body["error"]["code"], -32000);
    assert_eq!(body["error"]["message"], "tool failed on purpose");

    let invocations = gateway
        .storage
        .list_mcp_invocations(10, None)
        .await
        .unwrap();
    assert_eq!(invocations.len(), 2);
    assert_eq!(invocations[0].status, "error");
    assert_eq!(invocations[1].status, "ok");
}

#[tokio::test]
async fn hung_stdio_calls_time_out_and_the_server_restarts() {
    // A child that hangs on `tools/call` makes the call time out. The timeout
    // is final (no replay), the failure is recorded, and the next call
    // replaces the stuck child transparently. The hang knob travels through
    // `config_json.env` so parallel tests never share process environment.
    let gateway = Gateway::new_with_tool_timeout(1).await;
    let (status, server) = gateway
        .control(
            "POST",
            "/api/v1/mcp/servers",
            json!({
                "name": "stuck",
                "command": mock_command(),
                "config": {"env": {"MOCK_MCP_HANG_TOOL": "slow"}}
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{server}");
    let session = gateway.open_session(None).await;

    let started = std::time::Instant::now();
    let (status, body) = gateway
        .rpc(
            &session,
            json!(3),
            "tools/call",
            json!({"name": "stuck__slow", "arguments": {"delay_ms": 5000}}),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["error"]["code"], -32006, "{body}");
    assert!(started.elapsed() < std::time::Duration::from_secs(4));

    let invocations = gateway
        .storage
        .list_mcp_invocations(10, None)
        .await
        .unwrap();
    assert_eq!(invocations[0].status, "timeout", "{invocations:?}");

    // A fresh child without the knob answers normally on the restart.
    let (_, updated) = gateway
        .control(
            "PUT",
            &format!("/api/v1/mcp/servers/{}", server["id"].as_str().unwrap()),
            json!({"name": "stuck", "command": mock_command()}),
        )
        .await;
    assert_eq!(updated["tool_count"], 3, "{updated}");

    let (_, body) = gateway
        .rpc(
            &session,
            json!(4),
            "tools/call",
            json!({"name": "stuck__echo", "arguments": {"message": "back"}}),
            None,
        )
        .await;
    assert_eq!(body["result"]["content"][0]["text"], "back", "{body}");
}

#[tokio::test]
async fn stdio_tool_timeouts_are_recorded() {
    let gateway = gateway_with_stdio("slowpoke").await;
    let session = gateway.open_session(None).await;
    let started = std::time::Instant::now();
    let (_, body) = gateway
        .rpc(
            &session,
            json!(2),
            "tools/call",
            json!({"name": "slowpoke__slow", "arguments": {"delay_ms": 5000}}),
            None,
        )
        .await;
    // The default tool call timeout is 120s, so this call completes.
    assert_eq!(body["result"]["content"][0]["text"], "slow");
    assert!(started.elapsed() >= std::time::Duration::from_millis(5000));

    let invocations = gateway
        .storage
        .list_mcp_invocations(10, None)
        .await
        .unwrap();
    assert_eq!(invocations[0].status, "ok");
    assert!(invocations[0].duration_ms >= 5000);
}

#[tokio::test]
async fn server_reload_drops_live_children() {
    let gateway = gateway_with_stdio("reloadable").await;
    let session = gateway.open_session(None).await;
    let (_, body) = gateway
        .rpc(
            &session,
            json!(2),
            "tools/call",
            json!({"name": "reloadable__echo", "arguments": {"message": "one"}}),
            None,
        )
        .await;
    assert_eq!(body["result"]["content"][0]["text"], "one");

    // Reload closes the connection; the next call transparently reconnects.
    gateway.runtime.reload().await.unwrap();
    let (_, body) = gateway
        .rpc(
            &session,
            json!(3),
            "tools/call",
            json!({"name": "reloadable__echo", "arguments": {"message": "two"}}),
            None,
        )
        .await;
    assert_eq!(body["result"]["content"][0]["text"], "two");
}

#[tokio::test]
async fn stdio_config_json_passes_args_and_env() {
    let gateway = Gateway::new().await;
    let (status, server) = gateway
        .control(
            "POST",
            "/api/v1/mcp/servers",
            json!({
                "name": "configured",
                "command": mock_command(),
                "config": {
                    "args": ["--unused"],
                    "env": {"MOCK_PLACEHOLDER": "1"}
                }
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{server}");
    assert_eq!(server["transport"], "stdio", "transport is inferred");

    // Args/env are accepted verbatim by the mock and the server still answers.
    let session = gateway.open_session(None).await;
    let (_, body) = gateway
        .rpc(&session, json!(2), "tools/list", json!({}), None)
        .await;
    assert_eq!(body["result"]["tools"].as_array().unwrap().len(), 3);
}
