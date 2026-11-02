//! Data plane: the `/mcp` Streamable HTTP endpoint.
//!
//! POST carries JSON-RPC (`initialize`, `ping`, `tools/list`, `tools/call`);
//! notifications are accepted with 202; GET holds an SSE keepalive stream;
//! DELETE terminates the gateway session. Every request other than
//! `initialize` is bound to an `Mcp-Session-Id` created by `initialize`.

use std::time::{Duration, Instant};

use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::header::{self, HeaderName, HeaderValue};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use futures_util::stream;
use octra_data_plane::{body_bytes_limited, error_response, new_request_id};
use octra_mcp_core::domain::{acl_decision, session_status, status};
use octra_mcp_core::jsonrpc::{
    JsonRpcError, JsonRpcMessage, JsonRpcRequest, JsonRpcResponse, codes,
};
use octra_mcp_core::protocol::{
    Implementation, InitializeResult, ServerCapabilities, methods, parse_tool_name,
};
use octra_mcp_core::{CallToolParams, ListToolsResult, McpServer, McpTool, Tool};
use octra_storage::mcp::InvocationRecord;
use serde_json::{Map, Value, json};

use crate::McpDataState;
use crate::auth::{AuthError, Caller, authenticate};
use crate::client::McpError;

pub const SESSION_HEADER: HeaderName = HeaderName::from_static("mcp-session-id");
const SSE_KEEPALIVE: Duration = Duration::from_secs(15);

pub fn router(state: McpDataState) -> Router {
    Router::new()
        .route(
            "/mcp",
            post(handle_post).get(handle_get).delete(handle_delete),
        )
        .route(
            "/mcp/",
            post(handle_post).get(handle_get).delete(handle_delete),
        )
        .with_state(state)
}

// ----- POST --------------------------------------------------------------

async fn handle_post(
    State(state): State<McpDataState>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let caller = match authenticate(&state.storage, &headers).await {
        Ok(caller) => caller,
        Err(err) => return auth_failure(err),
    };

    let body = match body_bytes_limited(body, state.config.gateway.max_body_bytes).await {
        Ok(body) => body,
        Err(err) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_body",
                &format!("cannot read request body: {err}"),
            );
        }
    };

    let payload = match serde_json::from_slice::<Value>(&body) {
        Ok(payload) => payload,
        Err(_) => {
            return json_error_status(StatusCode::BAD_REQUEST, JsonRpcError::parse_error());
        }
    };

    let batch = payload.is_array();
    let items = match payload {
        Value::Array(items) => items,
        single => vec![single],
    };
    if items.is_empty() {
        return json_error_status(
            StatusCode::BAD_REQUEST,
            JsonRpcError::invalid_request("empty JSON-RPC batch"),
        );
    }

    let mut replies = Vec::with_capacity(items.len());
    let mut session: Option<String> = None;

    for item in items {
        match JsonRpcMessage::classify(item) {
            Ok(JsonRpcMessage::Request(request)) => {
                if let Some(reply) =
                    handle_request(&state, &caller, &headers, request, &mut session).await
                {
                    replies.push(reply);
                }
            }
            Ok(JsonRpcMessage::Response(_)) => replies.push(JsonRpcResponse::failure(
                Value::Null,
                JsonRpcError::invalid_request("clients must not POST JSON-RPC responses"),
            )),
            Err(error) => {
                // A payload that is not a JSON-RPC message at all is a client
                // error as well as a protocol error.
                if error.code == codes::PARSE_ERROR || error.code == codes::INVALID_REQUEST {
                    return json_error_status(StatusCode::BAD_REQUEST, error);
                }
                replies.push(JsonRpcResponse::failure(Value::Null, error));
            }
        }
    }

    if replies.is_empty() {
        return with_session(StatusCode::ACCEPTED.into_response(), session);
    }

    let body = if batch || replies.len() > 1 {
        Value::Array(
            replies
                .into_iter()
                .map(|reply| serde_json::to_value(reply).unwrap_or(Value::Null))
                .collect(),
        )
    } else {
        serde_json::to_value(replies.remove(0)).unwrap_or(Value::Null)
    };
    with_session(json_status(StatusCode::OK, body, None), session)
}

/// Handles one JSON-RPC request. Notifications return `None` (no reply).
async fn handle_request(
    state: &McpDataState,
    caller: &Caller,
    headers: &HeaderMap,
    request: JsonRpcRequest,
    session: &mut Option<String>,
) -> Option<JsonRpcResponse> {
    let Some(id) = request.id.clone() else {
        handle_notification(state, headers, &request.method).await;
        return None;
    };

    let outcome = match request.method.as_str() {
        methods::INITIALIZE => initialize(state, request.params, session).await,
        methods::PING => match require_session(state, headers).await {
            Ok(_) => Ok(Map::new().into()),
            Err(error) => Err(error),
        },
        methods::TOOLS_LIST => match require_session(state, headers).await {
            Ok(_) => tools_list(state, caller).await,
            Err(error) => Err(error),
        },
        methods::TOOLS_CALL => match require_session(state, headers).await {
            Ok(_) => tools_call(state, caller, request.params).await,
            Err(error) => Err(error),
        },
        other => Err(JsonRpcError::method_not_found(other)),
    };

    Some(match outcome {
        Ok(result) => JsonRpcResponse::success(id, result),
        Err(error) => JsonRpcResponse::failure(id, error),
    })
}

/// Notifications never produce a response. `initialized` is the only one the
/// gateway acts on; everything else is accepted and dropped.
async fn handle_notification(state: &McpDataState, headers: &HeaderMap, method: &str) {
    if method == methods::INITIALIZED || require_session(state, headers).await.is_ok() {
        tracing::debug!(method, "accepted MCP notification");
    } else {
        tracing::debug!(method, "dropped notification for an unknown session");
    }
}

async fn initialize(
    state: &McpDataState,
    params: Option<Value>,
    session: &mut Option<String>,
) -> Result<Value, JsonRpcError> {
    let params =
        params.ok_or_else(|| JsonRpcError::invalid_params("initialize requires params"))?;
    let requested = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .map(str::to_string);
    if let Some(client) = params.get("clientInfo").and_then(Value::as_object)
        && let Some(name) = client.get("name").and_then(Value::as_str)
    {
        tracing::debug!(client = name, "MCP initialize");
    }

    let version = octra_mcp_core::protocol::negotiate_protocol_version(requested.as_deref(), None);
    let session_id = create_session(state).await?;
    *session = Some(session_id);

    let result = InitializeResult {
        protocol_version: version,
        capabilities: ServerCapabilities {
            tools: Some(json!({ "listChanged": false })),
            ..Default::default()
        },
        server_info: Implementation {
            name: "octra-mcp-gateway".to_string(),
            version: octra_core::VERSION.to_string(),
            title: Some("Octra MCP Gateway".to_string()),
        },
        instructions: None,
    };
    Ok(serde_json::to_value(result).unwrap_or(Value::Null))
}

/// Validates the session header and returns the session id.
async fn require_session(
    state: &McpDataState,
    headers: &HeaderMap,
) -> Result<String, JsonRpcError> {
    let session_id = header_str(headers, &SESSION_HEADER).ok_or_else(|| {
        JsonRpcError::new(codes::INVALID_REQUEST, "missing Mcp-Session-Id header")
    })?;

    match state.storage.mcp.get_session(&session_id).await {
        Ok(Some(session)) if session.status == session_status::OPEN => Ok(session.id),
        Ok(_) => Err(JsonRpcError::new(
            codes::INVALID_REQUEST,
            format!("session not found: {session_id}"),
        )),
        Err(err) => Err(storage_error(err)),
    }
}

/// Creates a gateway session that answers a later DELETE.
///
/// `mcp_sessions.server_id` references `mcp_servers` while a gateway session
/// spans every server, so the first registered server anchors it. With no
/// server registered there is nothing to route tools to anyway.
async fn create_session(state: &McpDataState) -> Result<String, JsonRpcError> {
    let servers = state
        .storage
        .mcp
        .list_servers()
        .await
        .map_err(storage_error)?;
    let anchor = servers
        .iter()
        .find(|server| server.enabled)
        .or_else(|| servers.first())
        .ok_or_else(|| JsonRpcError::new(codes::INTERNAL_ERROR, "no MCP server is registered"))?;

    let id = new_request_id();
    state
        .storage
        .mcp
        .insert_session(&id, &anchor.id)
        .await
        .map_err(storage_error)?;
    Ok(id)
}

async fn tools_list(state: &McpDataState, caller: &Caller) -> Result<Value, JsonRpcError> {
    let catalog = state
        .storage
        .mcp
        .list_catalog()
        .await
        .map_err(storage_error)?;
    let servers = state
        .storage
        .mcp
        .list_servers()
        .await
        .map_err(storage_error)?;
    let enabled: std::collections::HashSet<&str> = servers
        .iter()
        .filter(|server| server.enabled)
        .map(|server| server.id.as_str())
        .collect();

    let mut tools = Vec::new();
    for entry in &catalog {
        if !entry.tool.enabled || !enabled.contains(entry.tool.server_id.as_str()) {
            continue;
        }
        match acl_allows(state, caller, &entry.tool.id).await {
            Ok(true) => tools.push(Tool {
                name: entry.qualified_name(),
                title: None,
                description: (!entry.tool.description.is_empty())
                    .then(|| entry.tool.description.clone()),
                input_schema: entry.tool.schema.clone(),
                output_schema: None,
                annotations: None,
                extra: Map::new(),
            }),
            Ok(false) => {}
            Err(err) => return Err(storage_error(err)),
        }
    }

    let result = ListToolsResult {
        tools,
        next_cursor: None,
    };
    Ok(serde_json::to_value(result).unwrap_or(Value::Null))
}

async fn tools_call(
    state: &McpDataState,
    caller: &Caller,
    params: Option<Value>,
) -> Result<Value, JsonRpcError> {
    let params: CallToolParams = serde_json::from_value(
        params.ok_or_else(|| JsonRpcError::invalid_params("tools/call requires params"))?,
    )
    .map_err(|err| JsonRpcError::invalid_params(format!("invalid tools/call params: {err}")))?;

    let started = Instant::now();
    let (server, tool) = resolve_tool(state, caller, &params.name).await?;

    let result = state
        .runtime
        .servers()
        .call(
            &server,
            &tool.name,
            params.arguments,
            state.runtime.tool_call_timeout(),
            state.runtime.http(),
        )
        .await;

    match result {
        Ok(value) => {
            record(state, &server, &tool, caller, status::OK, started, None).await;
            Ok(value)
        }
        Err(err) => {
            let recorded = match err {
                McpError::Timeout => status::TIMEOUT,
                _ => status::ERROR,
            };
            record(
                state,
                &server,
                &tool,
                caller,
                recorded,
                started,
                Some(&err.to_string()),
            )
            .await;
            Err(JsonRpcError::with_data(
                err.code(),
                err.to_string(),
                json!({ "server": server.name, "tool": tool.name }),
            ))
        }
    }
}

/// Parses `<server>__<tool>`, applies enablement and ACL, and records every
/// decision as an invocation row (including denials and unknown tools when
/// the owning server can be identified).
async fn resolve_tool(
    state: &McpDataState,
    caller: &Caller,
    qualified: &str,
) -> Result<(McpServer, McpTool), JsonRpcError> {
    let not_found = || {
        JsonRpcError::new(
            codes::TOOL_NOT_FOUND,
            format!("unknown MCP tool: {qualified}"),
        )
    };

    let Some((server_name, tool_name)) = parse_tool_name(qualified) else {
        return Err(not_found());
    };
    let Some(server) = state
        .storage
        .mcp
        .get_server_by_name(server_name)
        .await
        .map_err(storage_error)?
    else {
        return Err(not_found());
    };
    let Some(tool) = state
        .storage
        .mcp
        .list_tools(&server.id)
        .await
        .map_err(storage_error)?
        .into_iter()
        .find(|tool| tool.name == tool_name)
    else {
        return Err(not_found());
    };

    if !server.enabled || !tool.enabled {
        let error = JsonRpcError::new(
            codes::TOOL_NOT_FOUND,
            format!("MCP tool {qualified} is disabled"),
        );
        record_decision(state, &server, &tool, caller, status::NOT_FOUND, &error).await;
        return Err(error);
    }

    let allowed = acl_allows(state, caller, &tool.id)
        .await
        .map_err(storage_error)?;
    if !allowed {
        let error = JsonRpcError::new(
            codes::FORBIDDEN,
            format!("consumer is not allowed to call {qualified}"),
        );
        record_decision(state, &server, &tool, caller, status::DENIED, &error).await;
        return Err(error);
    }

    Ok((server, tool))
}

/// Tools without ACL rows follow the open-local default (`ACL_DEFAULT_ALLOW`);
/// tools with rows are callable only by consumers holding an allow rule.
async fn acl_allows(
    state: &McpDataState,
    caller: &Caller,
    tool_id: &str,
) -> octra_core::Result<bool> {
    let flags = state.storage.mcp.acl_flags(tool_id).await?;
    if flags.is_empty() {
        return Ok(acl_decision(flags, caller.consumer_id.is_some()));
    }
    match caller.consumer_id.as_deref() {
        Some(consumer) => state.storage.mcp.acl_allows(tool_id, consumer).await,
        None => Ok(false),
    }
}

async fn record(
    state: &McpDataState,
    server: &McpServer,
    tool: &McpTool,
    caller: &Caller,
    outcome: &str,
    started: Instant,
    error: Option<&str>,
) {
    let duration_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
    let request_id = new_request_id();
    if let Err(err) = state
        .storage
        .mcp
        .record_invocation(&InvocationRecord {
            server_id: &server.id,
            tool_id: &tool.id,
            consumer_id: caller.consumer_id.as_deref(),
            request_id: &request_id,
            status: outcome,
            duration_ms,
            error,
        })
        .await
    {
        tracing::warn!(%err, "cannot persist MCP invocation");
    }
}

async fn record_decision(
    state: &McpDataState,
    server: &McpServer,
    tool: &McpTool,
    caller: &Caller,
    outcome: &str,
    error: &JsonRpcError,
) {
    let request_id = new_request_id();
    if let Err(err) = state
        .storage
        .mcp
        .record_invocation(&InvocationRecord {
            server_id: &server.id,
            tool_id: &tool.id,
            consumer_id: caller.consumer_id.as_deref(),
            request_id: &request_id,
            status: outcome,
            duration_ms: 0,
            error: Some(&error.message),
        })
        .await
    {
        tracing::warn!(%err, "cannot persist MCP invocation");
    }
}

// ----- GET / DELETE ------------------------------------------------------

async fn handle_get(State(state): State<McpDataState>, headers: HeaderMap) -> Response {
    if let Err(err) = authenticate(&state.storage, &headers).await {
        return auth_failure(err);
    }
    if let Err(error) = require_session(&state, &headers).await {
        return json_error_status(StatusCode::NOT_FOUND, error);
    }

    // Octra pushes no server-initiated notifications yet; the stream exists
    // so clients that hold the optional GET channel open can detect a dead
    // gateway through keepalives.
    let stream = stream::pending::<Result<Event, std::convert::Infallible>>();
    Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(SSE_KEEPALIVE)
                .text("octra-mcp-keepalive"),
        )
        .into_response()
}

async fn handle_delete(State(state): State<McpDataState>, headers: HeaderMap) -> Response {
    if let Err(err) = authenticate(&state.storage, &headers).await {
        return auth_failure(err);
    }
    let Some(session_id) = header_str(&headers, &SESSION_HEADER) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "missing_session",
            "Mcp-Session-Id header is required",
        );
    };

    match state.storage.mcp.terminate_session(&session_id).await {
        Ok(Some(session)) if session.status == session_status::TERMINATED => {
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(Some(_)) => StatusCode::NO_CONTENT.into_response(),
        Ok(None) => error_response(
            StatusCode::NOT_FOUND,
            "session_not_found",
            "MCP session not found",
        ),
        Err(err) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "storage_error",
            &err.to_string(),
        ),
    }
}

// ----- helpers -----------------------------------------------------------

fn header_str(headers: &HeaderMap, name: &HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn with_session(mut response: Response, session: Option<String>) -> Response {
    if let Some(session) = session
        && let Ok(value) = HeaderValue::from_str(&session)
    {
        response.headers_mut().insert(SESSION_HEADER, value);
    }
    response
}

fn json_status(status: StatusCode, body: Value, session: Option<String>) -> Response {
    with_session((status, Json(body)).into_response(), session)
}

fn json_error_status(status: StatusCode, error: JsonRpcError) -> Response {
    let body =
        serde_json::to_value(JsonRpcResponse::failure(Value::Null, error)).unwrap_or(Value::Null);
    with_session((status, Json(body)).into_response(), None)
}

fn auth_failure(err: AuthError) -> Response {
    match err {
        AuthError::Missing | AuthError::Inactive => {
            let mut response = error_response(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "an MCP API key is required",
            );
            response.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static("Bearer realm=\"octra-mcp\""),
            );
            response
        }
        AuthError::Invalid => error_response(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "invalid or inactive MCP API key",
        ),
        AuthError::Storage(message) => {
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "storage_error", &message)
        }
    }
}

fn storage_error(err: octra_core::Error) -> JsonRpcError {
    tracing::warn!(%err, "MCP data plane storage error");
    JsonRpcError::internal("storage error")
}
