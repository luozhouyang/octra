//! Data plane: the `/mcp` Streamable HTTP endpoint.
//!
//! POST carries JSON-RPC (`initialize`, `ping`, `tools/list`, `tools/call`);
//! notifications are accepted with 202; GET holds an SSE keepalive stream;
//! DELETE terminates the gateway session. Every request other than
//! `initialize` is bound to an `Mcp-Session-Id` created by `initialize`.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::header::{self, HeaderName, HeaderValue};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use cel_interpreter::{Context as CelContext, Program, Value as CelValue};
use futures_util::stream;
use octra_data_plane::{body_bytes_limited, error_response, new_request_id};
use octra_mcp_core::domain::{
    MCP_SERVER_MODE_CONVERSION_ONLY, MCP_SERVER_MODE_LISTENER, acl_allows_default_then_tool,
    acl_allows_scoped, is_source_server_mode, route_path_key, session_status, status,
};
use octra_mcp_core::jsonrpc::{
    JsonRpcError, JsonRpcMessage, JsonRpcRequest, JsonRpcResponse, codes,
};
use octra_mcp_core::protocol::{
    Implementation, InitializeResult, ServerCapabilities, methods, parse_tool_name,
};
use octra_mcp_core::{CallToolParams, ListToolsResult, McpServer, McpTool, Tool};
use octra_storage::mcp::InvocationRecord;
use serde_json::{Map, Value, json};
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::ReceiverStream;

use crate::McpDataState;
use crate::auth::{AuthError, Caller, authenticate, resolve_strategy};
use crate::client::McpError;
use crate::conversion;

pub const SESSION_HEADER: HeaderName = HeaderName::from_static("mcp-session-id");
pub const PROTOCOL_VERSION_HEADER: HeaderName = HeaderName::from_static("mcp-protocol-version");
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
        .route(
            "/mcp/{*rest}",
            post(handle_post_scoped)
                .get(handle_get_scoped)
                .delete(handle_delete_scoped),
        )
        .with_state(state)
}

/// A resolved per-server endpoint: the anchor server plus the servers whose
/// tools this endpoint exposes.
#[derive(Clone)]
struct RouteScope {
    server: McpServer,
    visible_server_ids: HashSet<String>,
}

/// Resolves `/mcp/{*rest}` by server name, server id, or explicit route path.
async fn resolve_scope(state: &McpDataState, rest: &str) -> Result<RouteScope, Response> {
    let servers =
        state.storage.mcp.list_servers().await.map_err(|err| {
            json_error_status(StatusCode::INTERNAL_SERVER_ERROR, storage_error(err))
        })?;

    let server = servers
        .iter()
        .find(|server| server.name == rest || server.id == rest)
        .or_else(|| {
            let requested = rest.trim_matches('/');
            servers.iter().find(|server| {
                server
                    .route_path
                    .as_deref()
                    .is_some_and(|path| route_path_key(path) == requested)
            })
        })
        .cloned()
        .ok_or_else(|| route_not_found(rest))?;

    // A conversion-only server exists only as a source for listeners. It has
    // no incoming MCP endpoint of its own.
    if server.mode == MCP_SERVER_MODE_CONVERSION_ONLY {
        return Err(json_error_status(
            StatusCode::NOT_FOUND,
            JsonRpcError::new(
                codes::TOOL_NOT_FOUND,
                format!(
                    "MCP server '{}' is conversion-only and cannot be served directly",
                    server.name
                ),
            ),
        ));
    }

    let visible_server_ids = scoped_visible_server_ids(&servers, &server);
    Ok(RouteScope {
        server,
        visible_server_ids,
    })
}

fn route_not_found(rest: &str) -> Response {
    json_error_status(
        StatusCode::NOT_FOUND,
        JsonRpcError::new(
            codes::TOOL_NOT_FOUND,
            format!("unknown MCP route: /mcp/{rest}"),
        ),
    )
}

// ----- POST --------------------------------------------------------------

async fn handle_post(
    State(state): State<McpDataState>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    handle_post_inner(state, headers, body, None, "/mcp".to_string()).await
}

async fn handle_post_scoped(
    State(state): State<McpDataState>,
    Path(rest): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let path = format!("/mcp/{rest}");
    let scope = match resolve_scope(&state, &rest).await {
        Ok(scope) => scope,
        Err(response) => return response,
    };
    handle_post_inner(state, headers, body, Some(scope), path).await
}

async fn handle_post_inner(
    state: McpDataState,
    headers: HeaderMap,
    body: Body,
    scope: Option<RouteScope>,
    path: String,
) -> Response {
    if let Some(response) = reject_cross_origin(&state, &headers) {
        return response;
    }
    if !is_json_content_type(&headers) {
        return error_response(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
            "MCP requests must use Content-Type: application/json",
        );
    }
    let caller = match authenticate_request(&state, &headers, scope.as_ref()).await {
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
                if let Some(reply) = handle_request(
                    &state,
                    &caller,
                    &headers,
                    request,
                    &mut session,
                    scope.as_ref(),
                    &path,
                )
                .await
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

    let session_missing = replies.iter().any(|reply| {
        reply
            .error
            .as_ref()
            .is_some_and(|error| error.code == codes::SESSION_NOT_FOUND)
    });

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
    let status = if session_missing {
        StatusCode::NOT_FOUND
    } else {
        StatusCode::OK
    };
    with_session(json_status(status, body, None), session)
}

/// Handles one JSON-RPC request. Notifications return `None` (no reply).
async fn handle_request(
    state: &McpDataState,
    caller: &Caller,
    headers: &HeaderMap,
    request: JsonRpcRequest,
    session: &mut Option<String>,
    scope: Option<&RouteScope>,
    path: &str,
) -> Option<JsonRpcResponse> {
    let Some(id) = request.id.clone() else {
        handle_notification(state, headers, scope, &request.method).await;
        return None;
    };

    let method = request.method.as_str();
    if method != methods::INITIALIZE
        && let Some(version) = header_str(headers, &PROTOCOL_VERSION_HEADER)
        && !octra_mcp_core::protocol::is_supported(&version)
    {
        return Some(JsonRpcResponse::failure(
            id,
            JsonRpcError::new(
                codes::INVALID_REQUEST,
                format!("unsupported MCP-Protocol-Version: {version}"),
            ),
        ));
    }
    let outcome = match request.method.as_str() {
        methods::INITIALIZE => initialize(state, request.params, session, scope).await,
        methods::PING => match require_session(state, headers, scope).await {
            Ok(_) => Ok(Map::new().into()),
            Err(error) => Err(error),
        },
        methods::TOOLS_LIST => match require_session(state, headers, scope).await {
            Ok(_) => tools_list(state, caller, scope, path).await,
            Err(error) => Err(error),
        },
        methods::TOOLS_CALL => match require_session(state, headers, scope).await {
            Ok(_) => tools_call(state, caller, request.params, scope, path).await,
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
async fn handle_notification(
    state: &McpDataState,
    headers: &HeaderMap,
    scope: Option<&RouteScope>,
    method: &str,
) {
    if method == methods::INITIALIZED || require_session(state, headers, scope).await.is_ok() {
        tracing::debug!(method, "accepted MCP notification");
    } else {
        tracing::debug!(method, "dropped notification for an unknown session");
    }
}

async fn initialize(
    state: &McpDataState,
    params: Option<Value>,
    session: &mut Option<String>,
    scope: Option<&RouteScope>,
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
    let session_id = create_session(state, scope).await?;
    *session = Some(session_id);

    let result = InitializeResult {
        protocol_version: version,
        capabilities: ServerCapabilities {
            tools: Some(json!({ "listChanged": true })),
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
    scope: Option<&RouteScope>,
) -> Result<String, JsonRpcError> {
    let session_id = header_str(headers, &SESSION_HEADER).ok_or_else(|| {
        JsonRpcError::new(codes::INVALID_REQUEST, "missing Mcp-Session-Id header")
    })?;

    let session = match state.storage.mcp.get_session(&session_id).await {
        Ok(Some(session)) if session.status == session_status::OPEN => session,
        Ok(_) => return Err(session_not_found(&session_id)),
        Err(err) => return Err(storage_error(err)),
    };
    if session_expired(&session, state.config.mcp.session_ttl_secs) {
        if let Err(err) = state.storage.mcp.terminate_session(&session.id).await {
            tracing::warn!(session = %session.id, %err, "cannot expire MCP session");
        }
        return Err(session_not_found(&session_id));
    }
    if let Some(scope) = scope
        && session.server_id != scope.server.id
    {
        return Err(session_not_found(&session_id));
    }
    Ok(session.id)
}

fn session_not_found(session_id: &str) -> JsonRpcError {
    JsonRpcError::new(
        codes::SESSION_NOT_FOUND,
        format!("session not found: {session_id}"),
    )
}

/// Sessions are bounded by age, not by idle time: a client that keeps using a
/// session must still re-initialize after the configured lifetime.
fn session_expired(session: &octra_mcp_core::domain::McpSession, ttl_secs: u64) -> bool {
    if ttl_secs == 0 {
        return false;
    }
    let Ok(created) = chrono::DateTime::parse_from_rfc3339(&session.created_at) else {
        return true;
    };
    let age = chrono::Utc::now().signed_duration_since(created.with_timezone(&chrono::Utc));
    age.num_seconds() >= ttl_secs as i64
}

/// Creates a gateway session that answers a later DELETE.
///
/// A scoped endpoint anchors the session to its selected server. The global
/// `/mcp` endpoint spans every server, so the first enabled server anchors it.
/// With no server registered there is nothing to route tools to anyway.
async fn create_session(
    state: &McpDataState,
    scope: Option<&RouteScope>,
) -> Result<String, JsonRpcError> {
    let anchor_id = match scope {
        Some(scope) => scope.server.id.clone(),
        None => {
            let servers = state
                .storage
                .mcp
                .list_servers()
                .await
                .map_err(storage_error)?;
            servers
                .iter()
                .find(|server| server.enabled && server.mode != MCP_SERVER_MODE_CONVERSION_ONLY)
                .or_else(|| servers.first())
                .map(|server| server.id.clone())
                .ok_or_else(|| {
                    JsonRpcError::new(codes::INTERNAL_ERROR, "no MCP server is registered")
                })?
        }
    };

    let id = new_request_id();
    state
        .storage
        .mcp
        .insert_session(&id, &anchor_id)
        .await
        .map_err(storage_error)?;
    Ok(id)
}

async fn tools_list(
    state: &McpDataState,
    caller: &Caller,
    scope: Option<&RouteScope>,
    path: &str,
) -> Result<Value, JsonRpcError> {
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
    let visible = request_visible_server_ids(state, &servers, scope)
        .await
        .map_err(storage_error)?;

    let mut tools = Vec::new();
    for entry in &catalog {
        if !entry.tool.enabled || !visible.contains(&entry.tool.server_id) {
            continue;
        }
        let Some(server) = servers
            .iter()
            .find(|server| server.id == entry.tool.server_id)
        else {
            continue;
        };
        match acl_allows(state, caller, server, &entry.tool, "POST", path).await {
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
    scope: Option<&RouteScope>,
    path: &str,
) -> Result<Value, JsonRpcError> {
    let params: CallToolParams = serde_json::from_value(
        params.ok_or_else(|| JsonRpcError::invalid_params("tools/call requires params"))?,
    )
    .map_err(|err| JsonRpcError::invalid_params(format!("invalid tools/call params: {err}")))?;

    let started = Instant::now();
    let (server, tool) = resolve_tool(state, caller, &params.name, scope, path).await?;

    let result = if tool.is_http() {
        conversion::execute_http_tool(
            state.runtime.http(),
            &server,
            &tool,
            params.arguments,
            state.runtime.tool_call_timeout(),
        )
        .await
        .and_then(|result| {
            serde_json::to_value(result)
                .map_err(|err| McpError::Internal(format!("cannot encode tool result: {err}")))
        })
    } else {
        state
            .runtime
            .servers()
            .call(
                &server,
                &tool.name,
                params.arguments,
                state.runtime.tool_call_timeout(),
                state.runtime.http(),
            )
            .await
    };

    match result {
        Ok(value) => {
            let is_error = value
                .get("isError")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if is_error {
                record(
                    state,
                    &server,
                    &tool,
                    caller,
                    status::ERROR,
                    started,
                    Some("tool returned isError=true"),
                )
                .await;
            } else {
                record(state, &server, &tool, caller, status::OK, started, None).await;
            }
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
    scope: Option<&RouteScope>,
    path: &str,
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

    // Only the endpoint's auth-compatible servers are addressable. A global
    // aggregate therefore cannot call a server whose effective strategy
    // differs from its own.
    let servers = state
        .storage
        .mcp
        .list_servers()
        .await
        .map_err(storage_error)?;
    let visible = request_visible_server_ids(state, &servers, scope)
        .await
        .map_err(storage_error)?;
    if !visible.contains(&server.id) {
        return Err(not_found());
    }

    if !server.enabled || !tool.enabled {
        let error = JsonRpcError::new(
            codes::TOOL_NOT_FOUND,
            format!("MCP tool {qualified} is disabled"),
        );
        record_decision(state, &server, &tool, caller, status::NOT_FOUND, &error).await;
        return Err(error);
    }

    let allowed = acl_allows(state, caller, &server, &tool, "POST", path)
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

/// Applies the scoped ACL precedence.
///
/// Tool rows (consumer, group, default) override the server entirely. A tool
/// without rows is governed by the server's CEL expressions when any exist;
/// otherwise server rows apply, and a server without rows follows the
/// open-local default.
async fn acl_allows(
    state: &McpDataState,
    caller: &Caller,
    server: &McpServer,
    tool: &McpTool,
    method: &str,
    path: &str,
) -> octra_core::Result<bool> {
    let tool_rows = state.storage.mcp.list_acls(Some(&tool.id)).await?;
    if !tool_rows.is_empty() {
        return Ok(acl_allows_scoped(
            &tool_rows,
            caller.consumer_id.as_deref(),
            &caller.consumer_group_ids,
        ));
    }

    if !server.acl_allow_when.is_empty() || !server.acl_deny_when.is_empty() {
        return Ok(match cel_allows(server, caller, tool, method, path) {
            Ok(allowed) => allowed,
            Err(err) => {
                // Evaluation failures fail closed for both listing and calls.
                tracing::warn!(
                    server = %server.name,
                    tool = %tool.name,
                    consumer = ?caller.consumer_id,
                    error = %err,
                    "MCP CEL ACL evaluation failed; denying"
                );
                false
            }
        });
    }

    let server_rows = state.storage.mcp.list_server_acls(&server.id).await?;
    Ok(acl_allows_default_then_tool(
        &[],
        &server_rows,
        caller.consumer_id.as_deref(),
        &caller.consumer_group_ids,
    ))
}

/// Validates every CEL expression configured on a server.
pub(crate) fn validate_acl_expressions(allow: &[String], deny: &[String]) -> Result<(), String> {
    for (field, expressions) in [("acl_allow_when", allow), ("acl_deny_when", deny)] {
        for (index, expression) in expressions.iter().enumerate() {
            validate_cel_expression(expression)
                .map_err(|err| format!("{field}[{index}]: {err}"))?;
        }
    }
    Ok(())
}

fn validate_cel_expression(source: &str) -> Result<(), String> {
    let length = source.chars().count();
    if source.trim().is_empty() {
        return Err("CEL expression must not be empty".to_string());
    }
    if length > 1024 {
        return Err(format!(
            "CEL expression must be at most 1024 characters, got {length}"
        ));
    }

    let program =
        Program::compile(source).map_err(|err| format!("invalid CEL expression: {err}"))?;
    let context = sample_cel_context()?;
    match program.execute(&context) {
        Ok(CelValue::Bool(_)) => Ok(()),
        Ok(value) => Err(format!(
            "CEL expression must evaluate to a boolean, got {}",
            value.type_of()
        )),
        Err(err) => Err(format!("CEL expression cannot be evaluated: {err}")),
    }
}

/// Kong-style deny-wins decision over the server expressions.
fn cel_allows(
    server: &McpServer,
    caller: &Caller,
    tool: &McpTool,
    method: &str,
    path: &str,
) -> Result<bool, String> {
    let context = cel_context(caller, server, tool, method, path)?;

    for expression in &server.acl_deny_when {
        if eval_cel_bool(expression, &context)? {
            return Ok(false);
        }
    }

    if server.acl_allow_when.is_empty() {
        return Ok(true);
    }
    for expression in &server.acl_allow_when {
        if eval_cel_bool(expression, &context)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn eval_cel_bool(source: &str, context: &CelContext<'_>) -> Result<bool, String> {
    let program =
        Program::compile(source).map_err(|err| format!("invalid CEL expression: {err}"))?;
    match program.execute(context) {
        Ok(CelValue::Bool(value)) => Ok(value),
        Ok(value) => Err(format!(
            "CEL expression must evaluate to a boolean, got {}",
            value.type_of()
        )),
        Err(err) => Err(format!("CEL evaluation failed: {err}")),
    }
}

fn cel_context(
    caller: &Caller,
    server: &McpServer,
    tool: &McpTool,
    method: &str,
    path: &str,
) -> Result<CelContext<'static>, String> {
    let mut context = CelContext::default();
    context
        .add_variable(
            "consumer",
            json!({
                "id": caller.consumer_id.as_deref(),
                "username": caller.consumer_name.as_deref(),
            }),
        )
        .map_err(|err| err.to_string())?;
    context
        .add_variable(
            "consumer_group",
            json!({ "names": caller.consumer_group_names }),
        )
        .map_err(|err| err.to_string())?;
    context
        .add_variable(
            "tool",
            json!({
                "name": tool.name,
                "server": server.name,
                "qualified_name": tool.qualified_name(&server.name),
            }),
        )
        .map_err(|err| err.to_string())?;
    context
        .add_variable(
            "request",
            json!({
                "method": method,
                "path": path,
                "server": server.name,
            }),
        )
        .map_err(|err| err.to_string())?;
    Ok(context)
}

fn sample_cel_context() -> Result<CelContext<'static>, String> {
    let mut context = CelContext::default();
    context
        .add_variable(
            "consumer",
            json!({"id": "sample-consumer", "username": "sample-user"}),
        )
        .map_err(|err| err.to_string())?;
    context
        .add_variable("consumer_group", json!({ "names": ["sample-group"] }))
        .map_err(|err| err.to_string())?;
    context
        .add_variable(
            "tool",
            json!({
                "name": "sample-tool",
                "server": "sample-server",
                "qualified_name": "sample-server__sample-tool",
            }),
        )
        .map_err(|err| err.to_string())?;
    context
        .add_variable(
            "request",
            json!({
                "method": "POST",
                "path": "/mcp/sample-server",
                "server": "sample-server",
            }),
        )
        .map_err(|err| err.to_string())?;
    Ok(context)
}

/// The servers whose tools the aggregate `/mcp` endpoint presents.
///
/// Without an enabled `listener` the endpoint keeps the legacy behaviour and
/// presents every enabled server except `conversion-only` sources. A listener
/// narrows the catalog to its configured sources plus its own tools; a
/// listener with an empty source list aggregates every enabled server that may
/// be a source, including `conversion-only` servers.
fn visible_server_ids(servers: &[McpServer]) -> HashSet<String> {
    let listeners: Vec<&McpServer> = servers
        .iter()
        .filter(|server| server.enabled && server.mode == MCP_SERVER_MODE_LISTENER)
        .collect();
    if listeners.is_empty() {
        return servers
            .iter()
            .filter(|server| server.enabled && server.mode != MCP_SERVER_MODE_CONVERSION_ONLY)
            .map(|server| server.id.clone())
            .collect();
    }

    let mut visible: HashSet<String> = listeners
        .iter()
        .map(|listener| listener.id.clone())
        .collect();
    for listener in listeners {
        if listener.sources.is_empty() {
            visible.extend(
                servers
                    .iter()
                    .filter(|server| server.enabled && is_source_server_mode(&server.mode))
                    .map(|server| server.id.clone()),
            );
            continue;
        }
        for reference in &listener.sources {
            if let Some(source) = servers.iter().find(|server| {
                server.enabled
                    && is_source_server_mode(&server.mode)
                    && (server.name == *reference || server.id == *reference)
            }) {
                visible.insert(source.id.clone());
            }
        }
    }
    visible
}

/// The servers a scoped `/mcp/{rest}` endpoint exposes.
///
/// A listener aggregates its own tools plus its `sources` (every enabled
/// upstream when the list is empty). Every other mode exposes only itself.
/// Disabled servers expose nothing.
fn scoped_visible_server_ids(servers: &[McpServer], scope: &McpServer) -> HashSet<String> {
    if !scope.enabled {
        return HashSet::new();
    }
    if scope.mode != MCP_SERVER_MODE_LISTENER {
        return HashSet::from([scope.id.clone()]);
    }

    let mut visible = HashSet::from([scope.id.clone()]);
    if scope.sources.is_empty() {
        visible.extend(
            servers
                .iter()
                .filter(|server| server.enabled && is_source_server_mode(&server.mode))
                .map(|server| server.id.clone()),
        );
        return visible;
    }
    for reference in &scope.sources {
        if let Some(source) = servers.iter().find(|server| {
            server.enabled
                && is_source_server_mode(&server.mode)
                && (server.name == *reference || server.id == *reference)
        }) {
            visible.insert(source.id.clone());
        }
    }
    visible
}

/// The effective auth strategy id of each server in an aggregate endpoint.
///
/// A server follows its explicit strategy only while that strategy still
/// exists and is enabled; otherwise the enabled default strategy applies.
/// `None` means the server falls back to MCP API keys (or anonymous access
/// when no key exists).
struct EffectiveAuthStrategies {
    default_id: Option<String>,
    by_server: HashMap<String, Option<String>>,
}

impl EffectiveAuthStrategies {
    /// The strategy an aggregate endpoint itself authenticates with.
    fn endpoint_strategy_id(&self, scope: Option<&RouteScope>) -> Option<&str> {
        match scope {
            Some(scope) => self
                .by_server
                .get(&scope.server.id)
                .and_then(|strategy| strategy.as_deref()),
            None => self.default_id.as_deref(),
        }
    }

    /// Keeps only the servers whose effective strategy matches the endpoint's,
    /// so an aggregate endpoint cannot expose a server authenticated by a
    /// different (possibly weaker) strategy.
    fn retain_compatible(
        &self,
        visible: HashSet<String>,
        endpoint_strategy_id: Option<&str>,
    ) -> HashSet<String> {
        visible
            .into_iter()
            .filter(|server_id| {
                self.by_server
                    .get(server_id)
                    .is_some_and(|strategy| strategy.as_deref() == endpoint_strategy_id)
            })
            .collect()
    }
}

async fn load_effective_auth_strategies(
    storage: &octra_storage::Storage,
    servers: &[McpServer],
) -> octra_core::Result<EffectiveAuthStrategies> {
    let default = storage.mcp.get_default_auth_strategy().await?;
    let strategies = storage.mcp.list_auth_strategies().await?;
    let enabled: HashSet<&str> = strategies
        .iter()
        .filter(|strategy| strategy.enabled)
        .map(|strategy| strategy.id.as_str())
        .collect();
    let default_id = default.as_ref().map(|strategy| strategy.id.as_str());
    let by_server = servers
        .iter()
        .map(|server| {
            let effective = server
                .auth_strategy_id
                .as_deref()
                .filter(|id| enabled.contains(id))
                .or(default_id)
                .map(str::to_string);
            (server.id.clone(), effective)
        })
        .collect();
    Ok(EffectiveAuthStrategies {
        default_id: default_id.map(str::to_string),
        by_server,
    })
}

/// The servers a request may see: the endpoint's topology-visible servers,
/// narrowed to those whose effective auth strategy matches the endpoint's.
async fn request_visible_server_ids(
    state: &McpDataState,
    servers: &[McpServer],
    scope: Option<&RouteScope>,
) -> octra_core::Result<HashSet<String>> {
    let visible = match scope {
        Some(scope) => scope.visible_server_ids.clone(),
        None => visible_server_ids(servers),
    };
    let strategies = load_effective_auth_strategies(&state.storage, servers).await?;
    let endpoint_strategy_id = strategies.endpoint_strategy_id(scope);
    Ok(strategies.retain_compatible(visible, endpoint_strategy_id))
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
    handle_get_inner(state, headers, None).await
}

async fn handle_get_scoped(
    State(state): State<McpDataState>,
    Path(rest): Path<String>,
    headers: HeaderMap,
) -> Response {
    let scope = match resolve_scope(&state, &rest).await {
        Ok(scope) => scope,
        Err(response) => return response,
    };
    handle_get_inner(state, headers, Some(&scope)).await
}

async fn handle_get_inner(
    state: McpDataState,
    headers: HeaderMap,
    scope: Option<&RouteScope>,
) -> Response {
    if let Some(response) = reject_cross_origin(&state, &headers) {
        return response;
    }
    if let Err(err) = authenticate_request(&state, &headers, scope).await {
        return auth_failure(err);
    }

    let session = match header_str(&headers, &SESSION_HEADER) {
        Some(session_id) => match state.storage.mcp.get_session(&session_id).await {
            Ok(Some(session)) if session.status == session_status::OPEN => Some(session),
            Ok(_) => None,
            Err(err) => {
                tracing::warn!(%err, "cannot load MCP session for the GET stream");
                None
            }
        },
        None => None,
    };
    // Clients may probe the GET channel before initializing; keep the stream
    // open with keepalives instead of failing the request.
    let Some(session) = session else {
        return keepalive_only();
    };

    let servers = match state.storage.mcp.list_servers().await {
        Ok(servers) => servers,
        Err(err) => {
            tracing::warn!(%err, "cannot list MCP servers for the GET stream");
            return keepalive_only();
        }
    };
    let visible = match scope {
        Some(scope) => scope.visible_server_ids.clone(),
        None => servers
            .iter()
            .find(|server| server.id == session.server_id)
            .filter(|server| server.mode == MCP_SERVER_MODE_LISTENER)
            .map(|listener| scoped_visible_server_ids(&servers, listener))
            .unwrap_or_else(|| visible_server_ids(&servers)),
    };
    let strategies = match load_effective_auth_strategies(&state.storage, &servers).await {
        Ok(strategies) => strategies,
        Err(err) => {
            tracing::warn!(%err, "cannot load MCP auth strategies for the GET stream");
            return keepalive_only();
        }
    };
    let endpoint_strategy_id = strategies.endpoint_strategy_id(scope);
    let visible = strategies.retain_compatible(visible, endpoint_strategy_id);

    let (sender, receiver) = mpsc::channel::<Result<Event, std::convert::Infallible>>(64);
    let (cancel, cancel_rx) = tokio::sync::watch::channel(false);
    for server_id in visible {
        let Some(mut notifications) = state.runtime.notifications(&server_id) else {
            continue;
        };
        let sender = sender.clone();
        let mut cancel_rx = cancel_rx.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    item = notifications.recv() => match item {
                        Ok(value) => {
                            if !is_notification(&value) {
                                continue;
                            }
                            let method = value
                                .get("method")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string();
                            let Ok(data) = serde_json::to_string(&value) else {
                                continue;
                            };
                            tracing::trace!(
                                server = %server_id,
                                method,
                                "delivering MCP notification"
                            );
                            let event = Event::default().event("message").data(data);
                            if sender.send(Ok(event)).await.is_err() {
                                return;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(dropped)) => {
                            tracing::debug!(
                                server = %server_id,
                                dropped,
                                "skipped lagged MCP notifications"
                            );
                        }
                        Err(broadcast::error::RecvError::Closed) => return,
                    },
                    changed = cancel_rx.changed() => {
                        if changed.is_err() || *cancel_rx.borrow() {
                            return;
                        }
                    }
                }
            }
        });
    }

    let storage = state.storage.clone();
    let session_id = session.id.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_millis(500));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if !session_is_open(&storage, &session_id).await {
                        let _ = cancel.send(true);
                        return;
                    }
                }
                _ = sender.closed() => {
                    let _ = cancel.send(true);
                    return;
                }
            }
        }
    });

    Sse::new(ReceiverStream::new(receiver))
        .keep_alive(
            KeepAlive::new()
                .interval(SSE_KEEPALIVE)
                .text("octra-mcp-keepalive"),
        )
        .into_response()
}

fn keepalive_only() -> Response {
    Sse::new(stream::pending::<Result<Event, std::convert::Infallible>>())
        .keep_alive(
            KeepAlive::new()
                .interval(SSE_KEEPALIVE)
                .text("octra-mcp-keepalive"),
        )
        .into_response()
}

fn is_notification(value: &Value) -> bool {
    value.get("method").is_some() && value.get("id").is_none()
}

async fn session_is_open(storage: &octra_storage::Storage, session_id: &str) -> bool {
    matches!(
        storage.mcp.get_session(session_id).await,
        Ok(Some(session)) if session.status == session_status::OPEN
    )
}

async fn handle_delete(State(state): State<McpDataState>, headers: HeaderMap) -> Response {
    handle_delete_inner(state, headers, None).await
}

async fn handle_delete_scoped(
    State(state): State<McpDataState>,
    Path(rest): Path<String>,
    headers: HeaderMap,
) -> Response {
    let scope = match resolve_scope(&state, &rest).await {
        Ok(scope) => scope,
        Err(response) => return response,
    };
    handle_delete_inner(state, headers, Some(&scope)).await
}

async fn handle_delete_inner(
    state: McpDataState,
    headers: HeaderMap,
    scope: Option<&RouteScope>,
) -> Response {
    if let Some(response) = reject_cross_origin(&state, &headers) {
        return response;
    }
    if let Err(err) = authenticate_request(&state, &headers, scope).await {
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

/// Browsers attach an Origin header to cross-site requests. Rejecting an
/// untrusted Origin blocks DNS-rebinding and simple-request CSRF against a
/// localhost MCP endpoint; non-browser clients normally omit the header.
fn reject_cross_origin(state: &McpDataState, headers: &HeaderMap) -> Option<Response> {
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    if origin_allowed(origin, &state.config.mcp.allowed_origins) {
        return None;
    }
    tracing::warn!(%origin, "rejected MCP request from an untrusted Origin");
    Some(error_response(
        StatusCode::FORBIDDEN,
        "origin_not_allowed",
        "request Origin is not allowed",
    ))
}

fn origin_allowed(origin: &str, allowed: &[String]) -> bool {
    if allowed.iter().any(|value| value == "*" || value == origin) {
        return true;
    }
    let Ok(url) = reqwest::Url::parse(origin) else {
        return false;
    };
    if !matches!(url.scheme(), "http" | "https") {
        return false;
    }
    matches!(
        url.host_str(),
        Some("localhost") | Some("127.0.0.1") | Some("::1")
    )
}

fn is_json_content_type(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(';')
                .next()
                .is_some_and(|media| media.trim().eq_ignore_ascii_case("application/json"))
        })
}

/// Selects the request's auth strategy and authenticates the caller.
async fn authenticate_request(
    state: &McpDataState,
    headers: &HeaderMap,
    scope: Option<&RouteScope>,
) -> Result<Caller, AuthError> {
    let strategy = resolve_strategy(&state.storage, scope.map(|scope| &scope.server)).await?;
    authenticate(
        &state.storage,
        Some(state.runtime.http()),
        headers,
        strategy.as_ref(),
        state.config.mcp.require_key,
    )
    .await
}

fn auth_failure(err: AuthError) -> Response {
    match err {
        AuthError::Missing | AuthError::MissingBearer | AuthError::Inactive => {
            unauthorized("an MCP API key or bearer token is required")
        }
        AuthError::Invalid => unauthorized("invalid or inactive MCP API key"),
        AuthError::InvalidToken(message) => {
            unauthorized(&format!("invalid bearer token: {message}"))
        }
        AuthError::InsufficientScope(scope) => error_response(
            StatusCode::FORBIDDEN,
            "insufficient_scope",
            &format!("bearer token is missing required scope: {scope}"),
        ),
        AuthError::Jwks(message) => {
            tracing::warn!(error = %message, "MCP JWKS validation is unavailable");
            error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "jwks_unavailable",
                &message,
            )
        }
        AuthError::Storage(message) => {
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "storage_error", &message)
        }
    }
}

fn unauthorized(message: &str) -> Response {
    let mut response = error_response(StatusCode::UNAUTHORIZED, "unauthorized", message);
    response.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Bearer realm=\"octra-mcp\""),
    );
    response
}

fn storage_error(err: octra_core::Error) -> JsonRpcError {
    tracing::warn!(%err, "MCP data plane storage error");
    JsonRpcError::internal("storage error")
}
