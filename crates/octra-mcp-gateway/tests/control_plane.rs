//! Control plane CRUD, validation, and key lifecycle.

mod support;

use axum::http::StatusCode;
use serde_json::json;
use support::{Gateway, UpstreamMode, start_upstream};

#[tokio::test]
async fn server_crud_validates_and_reports_conflicts() {
    let gateway = Gateway::new().await;

    // Missing fields and invalid transports are 400s.
    let (status, body) = gateway
        .control("POST", "/api/v1/mcp/servers", json!({"name": "empty"}))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "invalid_request");

    let (status, body) = gateway
        .control(
            "POST",
            "/api/v1/mcp/servers",
            json!({"name": "ssh", "transport": "sse", "url": "https://example.test"}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    let (status, body) = gateway
        .control(
            "POST",
            "/api/v1/mcp/servers",
            json!({"name": "bad url", "url": "ftp://example.test"}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    let (status, body) = gateway
        .control(
            "POST",
            "/api/v1/mcp/servers",
            json!({"name": "leaky", "url": "https://example.test", "headers": {"X-Token": "inline:abc"}}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    let (status, body) = gateway
        .control(
            "POST",
            "/api/v1/mcp/servers",
            json!({"name": "metadata", "url": "http://169.254.169.254/latest"}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // An unreachable server is still registered; discovery failures are only
    // logged, so the operator can fix the endpoint and refresh later.
    let (status, body) = gateway
        .control(
            "POST",
            "/api/v1/mcp/servers",
            json!({"name": "offline", "url": "http://127.0.0.1:9/mcp"}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["tool_count"], 0);
    let id = body["id"].as_str().unwrap().to_string();

    let (status, body) = gateway
        .control(
            "POST",
            "/api/v1/mcp/servers",
            json!({"name": "offline", "url": "http://127.0.0.1:9/mcp"}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    let (status, body) = gateway
        .control("GET", &format!("/api/v1/mcp/servers/{id}"), json!(null))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "offline");
    assert_eq!(body["transport"], "http");

    let (status, body) = gateway
        .control(
            "PUT",
            &format!("/api/v1/mcp/servers/{id}"),
            json!({"name": "offline-renamed", "url": "http://127.0.0.1:9/mcp", "enabled": false}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["name"], "offline-renamed");
    assert_eq!(body["enabled"], false);

    let (status, _) = gateway
        .control(
            "PUT",
            "/api/v1/mcp/servers/missing",
            json!({"name": "x", "url": "http://x"}),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, list) = gateway
        .control("GET", "/api/v1/mcp/servers", json!(null))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 1);

    assert_eq!(
        gateway
            .control_delete(&format!("/api/v1/mcp/servers/{id}"))
            .await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        gateway.control_delete("/api/v1/mcp/servers/missing").await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn creating_a_server_discovers_its_tools() {
    let upstream = start_upstream(UpstreamMode::Json).await;
    let gateway = Gateway::new().await;
    let id = gateway.add_http_server("remote", &upstream).await;

    let (_, tools) = gateway
        .control(
            "GET",
            &format!("/api/v1/mcp/tools?server_id={id}"),
            json!(null),
        )
        .await;
    let names: Vec<&str> = tools
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["qualified_name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["remote__boom", "remote__echo"]);
    assert_eq!(tools[1]["server_name"], "remote");

    // Manual tool lifecycle on top of the discovered set.
    let (status, tool) = gateway
        .control(
            "POST",
            "/api/v1/mcp/tools",
            json!({"server_id": id, "name": "manual", "description": "Added by hand"}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{tool}");
    assert_eq!(tool["qualified_name"], "remote__manual");
    assert_eq!(tool["schema"]["type"], "object");

    let (status, _) = gateway
        .control(
            "POST",
            "/api/v1/mcp/tools",
            json!({"server_id": id, "name": "manual"}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);

    let (status, _) = gateway
        .control(
            "POST",
            "/api/v1/mcp/tools",
            json!({"server_id": id, "name": "bad__name"}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (_, updated) = gateway
        .control(
            "PUT",
            &format!("/api/v1/mcp/tools/{}", tool["id"].as_str().unwrap()),
            json!({"description": "Edited", "enabled": false}),
        )
        .await;
    assert_eq!(updated["description"], "Edited");
    assert_eq!(updated["enabled"], false);

    let (_, all) = gateway
        .control("GET", "/api/v1/mcp/tools", json!(null))
        .await;
    assert_eq!(all.as_array().unwrap().len(), 3);

    assert_eq!(
        gateway
            .control_delete(&format!(
                "/api/v1/mcp/tools/{}",
                tool["id"].as_str().unwrap()
            ))
            .await,
        StatusCode::NO_CONTENT
    );
    let (_, all) = gateway
        .control("GET", "/api/v1/mcp/tools", json!(null))
        .await;
    assert_eq!(all.as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn keys_return_plaintext_once_and_are_listed_without_it() {
    let gateway = Gateway::new().await;

    let (status, created) = gateway
        .control(
            "POST",
            "/api/v1/mcp/keys",
            json!({"name": "ci", "consumer_name": "ci-agent", "expires_at": "2999-01-01"}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let value = created["key_value"].as_str().unwrap().to_string();
    assert!(value.starts_with("octra_"));
    assert_eq!(created["key_prefix"], &value[..12]);
    assert_eq!(created["expires_at"], "2999-01-01");
    assert!(created["consumer_id"].is_string());
    let id = created["id"].as_str().unwrap().to_string();

    let (_, listed) = gateway
        .control("GET", "/api/v1/mcp/keys", json!(null))
        .await;
    let listed = listed.as_array().unwrap();
    assert_eq!(listed.len(), 1);
    assert!(
        listed[0].get("key_value").is_none(),
        "the plaintext must never be returned again"
    );

    let (status, _) = gateway
        .control("POST", "/api/v1/mcp/keys", json!({"name": ""}))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, _) = gateway
        .control(
            "POST",
            "/api/v1/mcp/keys",
            json!({"name": "bad", "expires_at": "soon"}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, _) = gateway
        .control(
            "POST",
            "/api/v1/mcp/keys",
            json!({"name": "bad", "consumer_id": "nobody"}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, _) = gateway
        .control("GET", &format!("/api/v1/mcp/keys/{id}"), json!(null))
        .await;
    assert_eq!(status, StatusCode::OK);

    assert_eq!(
        gateway
            .control_delete(&format!("/api/v1/mcp/keys/{id}"))
            .await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        gateway
            .control("GET", "/api/v1/mcp/keys", json!(null))
            .await
            .1,
        json!([])
    );
}

#[tokio::test]
async fn acl_crud_requires_known_tool_and_consumer() {
    let upstream = start_upstream(UpstreamMode::Json).await;
    let gateway = Gateway::new().await;
    let server_id = gateway.add_http_server("remote", &upstream).await;
    let (_, keys) = gateway
        .control(
            "POST",
            "/api/v1/mcp/keys",
            json!({"name": "a", "consumer_name": "consumer-a"}),
        )
        .await;
    let consumer = keys["consumer_id"].as_str().unwrap().to_string();

    let (_, tools) = gateway
        .control(
            "GET",
            &format!("/api/v1/mcp/tools?server_id={server_id}"),
            json!(null),
        )
        .await;
    let tool = tools[0]["id"].as_str().unwrap().to_string();

    let (status, body) = gateway
        .control(
            "POST",
            "/api/v1/mcp/acls",
            json!({"tool_id": "missing", "consumer_id": consumer}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    let (status, body) = gateway
        .control(
            "POST",
            "/api/v1/mcp/acls",
            json!({"tool_id": tool, "consumer_id": "missing"}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    let (status, acl) = gateway
        .control(
            "POST",
            "/api/v1/mcp/acls",
            json!({"tool_id": tool, "consumer_id": consumer, "allowed": true}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{acl}");
    assert_eq!(acl["allowed"], true);
    let acl_id = acl["id"].as_str().unwrap().to_string();

    // Re-posting the same pair updates instead of conflicting.
    let (status, acl) = gateway
        .control(
            "POST",
            "/api/v1/mcp/acls",
            json!({"tool_id": tool, "consumer_id": consumer, "allowed": false}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(acl["id"], acl_id);
    assert_eq!(acl["allowed"], false);

    let (_, updated) = gateway
        .control(
            "PUT",
            &format!("/api/v1/mcp/acls/{acl_id}"),
            json!({"allowed": true}),
        )
        .await;
    assert_eq!(updated["allowed"], true);

    let (_, listed) = gateway
        .control(
            "GET",
            &format!("/api/v1/mcp/acls?tool_id={tool}"),
            json!(null),
        )
        .await;
    assert_eq!(listed.as_array().unwrap().len(), 1);

    let (status, _) = gateway
        .control("GET", "/api/v1/mcp/acls?tool_id=missing", json!(null))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    assert_eq!(
        gateway
            .control_delete(&format!("/api/v1/mcp/acls/{acl_id}"))
            .await,
        StatusCode::NO_CONTENT
    );
}

#[tokio::test]
async fn sessions_and_invocations_are_readable() {
    let upstream = start_upstream(UpstreamMode::Json).await;
    let gateway = Gateway::new().await;
    gateway.add_http_server("remote", &upstream).await;
    let session = gateway.open_session(None).await;
    gateway
        .rpc(
            &session,
            json!(2),
            "tools/call",
            json!({"name": "remote__echo", "arguments": {"message": "x"}}),
            None,
        )
        .await;

    let (status, sessions) = gateway
        .control("GET", "/api/v1/mcp/sessions", json!(null))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(sessions.as_array().unwrap().len(), 1);
    assert_eq!(sessions[0]["id"], session);
    assert_eq!(sessions[0]["status"], "open");

    let (status, session_body) = gateway
        .control(
            "GET",
            &format!("/api/v1/mcp/sessions/{session}"),
            json!(null),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(session_body["id"], session);

    let (status, terminated) = gateway
        .control(
            "DELETE",
            &format!("/api/v1/mcp/sessions/{session}"),
            json!(null),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(terminated["status"], "terminated");

    let (_, invocations) = gateway
        .control("GET", "/api/v1/mcp/invocations?limit=10", json!(null))
        .await;
    let invocations = invocations.as_array().unwrap();
    assert_eq!(invocations.len(), 1);
    assert_eq!(invocations[0]["status"], "ok");
    let (status, single) = gateway
        .control(
            "GET",
            &format!("/api/v1/mcp/invocations/{}", invocations[0]["id"]),
            json!(null),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(single["request_id"], invocations[0]["request_id"]);

    let (status, _) = gateway
        .control("GET", "/api/v1/mcp/invocations/999999", json!(null))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
