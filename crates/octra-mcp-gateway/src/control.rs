//! Control plane: `/api/v1/mcp/*` CRUD for servers, tools, sessions,
//! invocations, API keys, and ACLs, plus explicit tool discovery.
//!
//! Mutations of servers, tools, and ACLs reload the runtime synchronously.

use std::collections::BTreeMap;

use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use octra_data_plane::body_bytes_limited;
use octra_mcp_core::domain::{McpServer, McpTool, NewMcpServer, status, validate_secret_reference};
use octra_mcp_core::{Transport, domain::McpApiKey};
use octra_storage::mcp::DiscoveredTool;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::auth::{display_prefix, generate_key, hash_key};
use crate::{DiscoveryStats, McpControlState};

type ApiResult<T> = Result<T, ApiError>;

pub fn router(state: McpControlState) -> Router {
    Router::new()
        .route("/api/v1/mcp/servers", get(list_servers).post(create_server))
        .route(
            "/api/v1/mcp/servers/{id}",
            get(get_server).put(update_server).delete(delete_server),
        )
        .route("/api/v1/mcp/servers/{id}/refresh", post(refresh_server))
        .route("/api/v1/mcp/tools", get(list_tools).post(create_tool))
        .route(
            "/api/v1/mcp/tools/{id}",
            get(get_tool).put(update_tool).delete(delete_tool),
        )
        .route("/api/v1/mcp/sessions", get(list_sessions))
        .route(
            "/api/v1/mcp/sessions/{id}",
            get(get_session).delete(delete_session),
        )
        .route("/api/v1/mcp/invocations", get(list_invocations))
        .route("/api/v1/mcp/invocations/{id}", get(get_invocation))
        .route("/api/v1/mcp/keys", get(list_keys).post(create_key))
        .route("/api/v1/mcp/keys/{id}", get(get_key).delete(delete_key))
        .route("/api/v1/mcp/acls", get(list_acls).post(create_acl))
        .route(
            "/api/v1/mcp/acls/{id}",
            get(get_acl).put(update_acl).delete(delete_acl),
        )
        .with_state(state)
}

// ----- errors ------------------------------------------------------------

pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "invalid_request",
            message: message.into(),
        }
    }

    pub fn not_found(what: &str) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "not_found",
            message: format!("{what} not found"),
        }
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            code: "conflict",
            message: message.into(),
        }
    }

    pub fn upstream(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            code: "upstream_error",
            message: message.into(),
        }
    }

    pub fn internal(err: impl std::fmt::Display) -> Self {
        tracing::error!(error = %err, "mcp control api request failed");
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "internal_error",
            message: "internal error".to_string(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = json!({
            "error": {
                "code": self.code,
                "message": self.message,
            }
        });
        (self.status, Json(body)).into_response()
    }
}

fn storage(err: octra_core::Error) -> ApiError {
    ApiError::internal(err)
}

/// Maps a storage failure on a UNIQUE column to a 409 with a readable reason.
fn conflict_on_unique(err: octra_core::Error, what: &str) -> ApiError {
    match err {
        octra_core::Error::Storage(message) if message.contains("duplicate value") => {
            ApiError::conflict(format!("{what} already exists"))
        }
        other => ApiError::internal(other),
    }
}

async fn json_body<T: serde::de::DeserializeOwned>(
    state: &McpControlState,
    body: Body,
) -> ApiResult<T> {
    let bytes = body_bytes_limited(body, state.runtime.config.gateway.max_body_bytes)
        .await
        .map_err(|err| ApiError::bad_request(format!("cannot read request body: {err}")))?;
    serde_json::from_slice(&bytes)
        .map_err(|err| ApiError::bad_request(format!("invalid JSON body: {err}")))
}

// ----- servers -----------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct ServerRequest {
    pub name: String,
    pub transport: Option<Transport>,
    pub command: Option<String>,
    pub url: Option<String>,
    pub enabled: bool,
    pub config: Option<Value>,
    pub headers: Option<BTreeMap<String, String>>,
}

impl Default for ServerRequest {
    fn default() -> Self {
        Self {
            name: String::new(),
            transport: None,
            command: None,
            url: None,
            enabled: true,
            config: None,
            headers: None,
        }
    }
}

impl ServerRequest {
    /// Builds the repository record after transport inference and validation.
    pub fn into_new_server(self) -> ApiResult<NewMcpServer> {
        let transport = self.transport.unwrap_or_else(|| {
            if self
                .url
                .as_deref()
                .is_some_and(|url| !url.trim().is_empty())
            {
                Transport::Http
            } else {
                Transport::Stdio
            }
        });

        let mut headers = Map::new();
        for (name, value) in self.headers.unwrap_or_default() {
            let value = value.trim().to_string();
            if value.starts_with("inline:") {
                return Err(ApiError::bad_request(
                    "inline secrets are not allowed; use env:NAME or keyring:NAME references",
                ));
            }
            if value.contains(':') && validate_secret_reference(&value).is_err() {
                return Err(ApiError::bad_request(format!(
                    "header {name} must be a literal value or an env:/keyring: reference"
                )));
            }
            headers.insert(name, Value::String(value));
        }

        let server = NewMcpServer {
            name: self.name.trim().to_string(),
            transport,
            command: self.command,
            url: self.url,
            enabled: self.enabled,
            config: self.config.unwrap_or_else(|| json!({})),
            headers,
        };
        server.validate().map_err(ApiError::bad_request)?;
        Ok(server)
    }
}

#[derive(Debug, Serialize)]
pub struct ServerResponse {
    #[serde(flatten)]
    pub server: McpServer,
    pub tool_count: usize,
}

async fn server_response(state: &McpControlState, server: McpServer) -> ApiResult<ServerResponse> {
    let tool_count = state
        .storage
        .mcp
        .list_tools(&server.id)
        .await
        .map_err(storage)?
        .len();
    Ok(ServerResponse { server, tool_count })
}

async fn list_servers(
    State(state): State<McpControlState>,
) -> ApiResult<Json<Vec<ServerResponse>>> {
    let mut servers = Vec::new();
    for server in state.storage.mcp.list_servers().await.map_err(storage)? {
        servers.push(server_response(&state, server).await?);
    }
    Ok(Json(servers))
}

async fn create_server(State(state): State<McpControlState>, body: Body) -> ApiResult<Response> {
    let request: ServerRequest = json_body(&state, body).await?;
    let new_server = request.into_new_server()?;
    let display_name = new_server.name.clone();

    let server = state
        .storage
        .mcp
        .insert_server(&new_server)
        .await
        .map_err(|err| conflict_on_unique(err, &format!("MCP server '{display_name}'")))?;

    // Discovery failures must not fail the create: the server is registered
    // and can be refreshed later from the control plane.
    match state.runtime.refresh_server(&server.id).await {
        Ok(_) => {}
        Err(err) => tracing::warn!(server = %server.name, %err, "initial MCP discovery failed"),
    }

    match state
        .storage
        .mcp
        .get_server(&server.id)
        .await
        .map_err(storage)?
    {
        Some(server) => Ok((
            StatusCode::CREATED,
            Json(server_response(&state, server).await?),
        )
            .into_response()),
        None => Err(ApiError::internal("server vanished after creation")),
    }
}

async fn get_server(
    State(state): State<McpControlState>,
    Path(id): Path<String>,
) -> ApiResult<Json<ServerResponse>> {
    let server = state
        .storage
        .mcp
        .get_server(&id)
        .await
        .map_err(storage)?
        .ok_or_else(|| ApiError::not_found("MCP server"))?;
    Ok(Json(server_response(&state, server).await?))
}

async fn update_server(
    State(state): State<McpControlState>,
    Path(id): Path<String>,
    body: Body,
) -> ApiResult<Json<ServerResponse>> {
    let existing = state
        .storage
        .mcp
        .get_server(&id)
        .await
        .map_err(storage)?
        .ok_or_else(|| ApiError::not_found("MCP server"))?;

    let request: ServerRequest = json_body(&state, body).await?;
    let mut candidate = request.into_new_server()?;
    if candidate.name.trim().is_empty() {
        candidate.name = existing.name.clone();
    }

    let display_name = candidate.name.clone();
    let updated = state
        .storage
        .mcp
        .update_server(&id, &candidate)
        .await
        .map_err(|err| conflict_on_unique(err, &format!("MCP server '{display_name}'")))?
        .ok_or_else(|| ApiError::not_found("MCP server"))?;

    let definition_changed = definition_changed(&existing, &updated);
    state.runtime.reload_after_mutation().await;
    if definition_changed {
        match state.runtime.refresh_server(&id).await {
            Ok(_) => {}
            Err(err) => tracing::warn!(server = %updated.name, %err, "MCP re-discovery failed"),
        }
    }

    let updated = state
        .storage
        .mcp
        .get_server(&id)
        .await
        .map_err(storage)?
        .unwrap_or(updated);
    Ok(Json(server_response(&state, updated).await?))
}

fn definition_changed(before: &McpServer, after: &McpServer) -> bool {
    before.transport != after.transport
        || before.command != after.command
        || before.url != after.url
        || before.config != after.config
        || before.headers != after.headers
        || before.enabled != after.enabled
}

async fn delete_server(
    State(state): State<McpControlState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let deleted = state
        .storage
        .mcp
        .delete_server(&id)
        .await
        .map_err(storage)?;
    if !deleted {
        return Err(ApiError::not_found("MCP server"));
    }
    state.runtime.reload_after_mutation().await;
    Ok(StatusCode::NO_CONTENT)
}

async fn refresh_server(
    State(state): State<McpControlState>,
    Path(id): Path<String>,
) -> ApiResult<Json<DiscoveryStats>> {
    let stats = state
        .runtime
        .refresh_server(&id)
        .await
        .map_err(|err| match err {
            octra_core::Error::Runtime(message) if message.contains("does not exist") => {
                ApiError::not_found("MCP server")
            }
            other => ApiError::upstream(other.to_string()),
        })?;
    Ok(Json(stats))
}

// ----- tools -------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct ToolRequest {
    pub server_id: String,
    pub name: String,
    pub description: String,
    pub schema: Option<Value>,
    pub enabled: bool,
}

impl Default for ToolRequest {
    fn default() -> Self {
        Self {
            server_id: String::new(),
            name: String::new(),
            description: String::new(),
            schema: None,
            enabled: true,
        }
    }
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct ToolUpdate {
    pub description: Option<String>,
    pub schema: Option<Value>,
    pub enabled: Option<bool>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct ToolQuery {
    pub server_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ToolResponse {
    #[serde(flatten)]
    pub tool: McpTool,
    pub server_name: String,
    pub qualified_name: String,
}

fn tool_response(tool: McpTool, server_name: &str) -> ToolResponse {
    ToolResponse {
        qualified_name: tool.qualified_name(server_name),
        server_name: server_name.to_string(),
        tool,
    }
}

async fn list_tools(
    State(state): State<McpControlState>,
    Query(query): Query<ToolQuery>,
) -> ApiResult<Json<Vec<ToolResponse>>> {
    if let Some(server_id) = query.server_id.as_deref() {
        let server = state
            .storage
            .mcp
            .get_server(server_id)
            .await
            .map_err(storage)?
            .ok_or_else(|| ApiError::not_found("MCP server"))?;
        let tools = state
            .storage
            .mcp
            .list_tools(server_id)
            .await
            .map_err(storage)?;
        return Ok(Json(
            tools
                .into_iter()
                .map(|tool| tool_response(tool, &server.name))
                .collect(),
        ));
    }

    let catalog = state.storage.mcp.list_catalog().await.map_err(storage)?;
    Ok(Json(
        catalog
            .into_iter()
            .map(|entry| tool_response(entry.tool, &entry.server_name))
            .collect(),
    ))
}

async fn resolve_server(state: &McpControlState, server_id: &str) -> ApiResult<McpServer> {
    state
        .storage
        .mcp
        .get_server(server_id)
        .await
        .map_err(storage)?
        .ok_or_else(|| ApiError::bad_request(format!("unknown server_id: {server_id}")))
}

async fn find_tool(state: &McpControlState, id: &str) -> ApiResult<(McpTool, McpServer)> {
    let tool = state
        .storage
        .mcp
        .get_tool(id)
        .await
        .map_err(storage)?
        .ok_or_else(|| ApiError::not_found("MCP tool"))?;
    let server = state
        .storage
        .mcp
        .get_server(&tool.server_id)
        .await
        .map_err(storage)?
        .ok_or_else(|| ApiError::not_found("MCP server"))?;
    Ok((tool, server))
}

async fn create_tool(
    State(state): State<McpControlState>,
    body: Body,
) -> ApiResult<(StatusCode, Json<ToolResponse>)> {
    let request: ToolRequest = json_body(&state, body).await?;
    let server = resolve_server(&state, &request.server_id).await?;
    let name = request.name.trim().to_string();
    octra_mcp_core::protocol::validate_tool_name(&name)
        .map_err(|err| ApiError::bad_request(err.to_string()))?;

    let schema = match request.schema {
        Some(schema) if schema.is_object() => schema,
        Some(_) => return Err(ApiError::bad_request("schema must be a JSON object")),
        None => json!({"type": "object"}),
    };

    let mut tools = current_tool_set(&state, &server.id).await?;
    if tools.iter().any(|tool| tool.name == name) {
        return Err(ApiError::conflict(format!(
            "tool '{name}' already exists on server '{}'",
            server.name
        )));
    }
    tools.push(DiscoveredTool {
        name: name.clone(),
        description: request.description,
        schema,
    });

    replace_tool_set(&state, &server.id, &tools).await?;
    let tool = find_tool_by_name(&state, &server.id, &name).await?;
    if !request.enabled {
        state
            .storage
            .mcp
            .set_tool_enabled(&tool.id, false)
            .await
            .map_err(storage)?;
    }
    state.runtime.reload_after_mutation().await;

    let tool = state
        .storage
        .mcp
        .get_tool(&tool.id)
        .await
        .map_err(storage)?
        .ok_or_else(|| ApiError::internal("tool vanished after creation"))?;
    Ok((StatusCode::CREATED, Json(tool_response(tool, &server.name))))
}

/// The server's current tools in repository order, ready to be rewritten.
async fn current_tool_set(
    state: &McpControlState,
    server_id: &str,
) -> ApiResult<Vec<DiscoveredTool>> {
    Ok(state
        .storage
        .mcp
        .list_tools(server_id)
        .await
        .map_err(storage)?
        .into_iter()
        .map(|tool| DiscoveredTool {
            name: tool.name,
            description: tool.description,
            schema: tool.schema,
        })
        .collect())
}

/// Rewrites the whole tool set. The upsert-by-name refresh keeps ids, so
/// enable flags and ACL rows of surviving tools are preserved.
async fn replace_tool_set(
    state: &McpControlState,
    server_id: &str,
    tools: &[DiscoveredTool],
) -> ApiResult<()> {
    state
        .storage
        .mcp
        .replace_tools(server_id, tools)
        .await
        .map_err(storage)?;
    Ok(())
}

async fn find_tool_by_name(
    state: &McpControlState,
    server_id: &str,
    name: &str,
) -> ApiResult<McpTool> {
    state
        .storage
        .mcp
        .list_tools(server_id)
        .await
        .map_err(storage)?
        .into_iter()
        .find(|tool| tool.name == name)
        .ok_or_else(|| ApiError::not_found("MCP tool"))
}

async fn get_tool(
    State(state): State<McpControlState>,
    Path(id): Path<String>,
) -> ApiResult<Json<ToolResponse>> {
    let (tool, server) = find_tool(&state, &id).await?;
    Ok(Json(tool_response(tool, &server.name)))
}

async fn update_tool(
    State(state): State<McpControlState>,
    Path(id): Path<String>,
    body: Body,
) -> ApiResult<Json<ToolResponse>> {
    let (tool, server) = find_tool(&state, &id).await?;
    let request: ToolUpdate = json_body(&state, body).await?;
    if request
        .schema
        .as_ref()
        .is_some_and(|schema| !schema.is_object())
    {
        return Err(ApiError::bad_request("schema must be a JSON object"));
    }

    if request.description.is_some() || request.schema.is_some() {
        let mut tools = current_tool_set(&state, &server.id).await?;
        if let Some(entry) = tools.iter_mut().find(|entry| entry.name == tool.name) {
            if let Some(description) = request.description {
                entry.description = description;
            }
            if let Some(schema) = request.schema {
                entry.schema = schema;
            }
        }
        replace_tool_set(&state, &server.id, &tools).await?;
    }
    if let Some(enabled) = request.enabled {
        state
            .storage
            .mcp
            .set_tool_enabled(&tool.id, enabled)
            .await
            .map_err(storage)?;
    }

    state.runtime.reload_after_mutation().await;
    let (tool, server) = find_tool(&state, &id).await?;
    Ok(Json(tool_response(tool, &server.name)))
}

async fn delete_tool(
    State(state): State<McpControlState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let (tool, server) = find_tool(&state, &id).await?;
    let tools: Vec<DiscoveredTool> = current_tool_set(&state, &server.id)
        .await?
        .into_iter()
        .filter(|entry| entry.name != tool.name)
        .collect();
    replace_tool_set(&state, &server.id, &tools).await?;
    state.runtime.reload_after_mutation().await;
    Ok(StatusCode::NO_CONTENT)
}

// ----- sessions ----------------------------------------------------------

async fn list_sessions(
    State(state): State<McpControlState>,
) -> ApiResult<Json<Vec<octra_mcp_core::McpSession>>> {
    Ok(Json(
        state.storage.mcp.list_sessions().await.map_err(storage)?,
    ))
}

async fn get_session(
    State(state): State<McpControlState>,
    Path(id): Path<String>,
) -> ApiResult<Json<octra_mcp_core::McpSession>> {
    state
        .storage
        .mcp
        .get_session(&id)
        .await
        .map_err(storage)?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("MCP session"))
}

async fn delete_session(
    State(state): State<McpControlState>,
    Path(id): Path<String>,
) -> ApiResult<Json<octra_mcp_core::McpSession>> {
    state
        .storage
        .mcp
        .terminate_session(&id)
        .await
        .map_err(storage)?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("MCP session"))
}

// ----- invocations -------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct InvocationQuery {
    pub limit: Option<i64>,
    pub server_id: Option<String>,
}

async fn list_invocations(
    State(state): State<McpControlState>,
    Query(query): Query<InvocationQuery>,
) -> ApiResult<Json<Vec<octra_mcp_core::McpInvocation>>> {
    let limit = query.limit.unwrap_or(100).clamp(1, 1000);
    Ok(Json(
        state
            .storage
            .mcp
            .list_invocations(limit, query.server_id.as_deref())
            .await
            .map_err(storage)?,
    ))
}

async fn get_invocation(
    State(state): State<McpControlState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<octra_mcp_core::McpInvocation>> {
    state
        .storage
        .mcp
        .get_invocation(id)
        .await
        .map_err(storage)?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("MCP invocation"))
}

// ----- API keys ----------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct KeyRequest {
    pub name: String,
    pub consumer_id: Option<String>,
    pub consumer_name: Option<String>,
    pub expires_at: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CreatedKey {
    #[serde(flatten)]
    pub key: McpApiKey,
    /// Returned exactly once at creation and never persisted.
    pub key_value: String,
}

async fn list_keys(State(state): State<McpControlState>) -> ApiResult<Json<Vec<McpApiKey>>> {
    Ok(Json(
        state.storage.mcp.list_api_keys().await.map_err(storage)?,
    ))
}

async fn create_key(
    State(state): State<McpControlState>,
    body: Body,
) -> ApiResult<(StatusCode, Json<CreatedKey>)> {
    let request: KeyRequest = json_body(&state, body).await?;
    if request.name.trim().is_empty() {
        return Err(ApiError::bad_request("key name must not be empty"));
    }
    if request.consumer_id.is_some() && request.consumer_name.is_some() {
        return Err(ApiError::bad_request(
            "pass either consumer_id or consumer_name, not both",
        ));
    }
    if let Some(expires_at) = request.expires_at.as_deref()
        && chrono_like_date(expires_at).is_none()
    {
        return Err(ApiError::bad_request(
            "expires_at must be a YYYY-MM-DD date",
        ));
    }

    let consumer_id = match (request.consumer_id.clone(), request.consumer_name.clone()) {
        (Some(id), _) => {
            if !state
                .storage
                .mcp
                .consumer_exists(&id)
                .await
                .map_err(storage)?
            {
                return Err(ApiError::bad_request(format!("unknown consumer_id: {id}")));
            }
            Some(id)
        }
        (None, Some(name)) => {
            let id = format!("consumer-{}", uuid::Uuid::new_v4());
            state
                .storage
                .mcp
                .ensure_consumer(&id, name.trim())
                .await
                .map_err(|err| conflict_on_unique(err, &format!("consumer '{}'", name.trim())))?;
            Some(id)
        }
        (None, None) => None,
    };

    let plaintext = generate_key();
    let key = state
        .storage
        .mcp
        .insert_api_key(
            request.name.trim(),
            &display_prefix(&plaintext),
            &hash_key(&plaintext),
            consumer_id.as_deref(),
            request.expires_at.as_deref(),
        )
        .await
        .map_err(storage)?;

    Ok((
        StatusCode::CREATED,
        Json(CreatedKey {
            key,
            key_value: plaintext,
        }),
    ))
}

/// Accepts exactly `YYYY-MM-DD`; the database compares it against UTC today.
fn chrono_like_date(value: &str) -> Option<()> {
    let bytes = value.as_bytes();
    let digits =
        |range: std::ops::Range<usize>| bytes[range].iter().all(|byte| byte.is_ascii_digit());
    (bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && digits(0..4)
        && digits(5..7)
        && digits(8..10))
    .then_some(())
}

async fn get_key(
    State(state): State<McpControlState>,
    Path(id): Path<String>,
) -> ApiResult<Json<McpApiKey>> {
    state
        .storage
        .mcp
        .get_api_key(&id)
        .await
        .map_err(storage)?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("MCP API key"))
}

async fn delete_key(
    State(state): State<McpControlState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    if state
        .storage
        .mcp
        .delete_api_key(&id)
        .await
        .map_err(storage)?
    {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found("MCP API key"))
    }
}

// ----- ACLs --------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct AclRequest {
    pub tool_id: String,
    pub consumer_id: String,
    pub allowed: bool,
}

impl Default for AclRequest {
    fn default() -> Self {
        Self {
            tool_id: String::new(),
            consumer_id: String::new(),
            allowed: true,
        }
    }
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct AclQuery {
    pub tool_id: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct AclUpdate {
    pub allowed: Option<bool>,
}

async fn list_acls(
    State(state): State<McpControlState>,
    Query(query): Query<AclQuery>,
) -> ApiResult<Json<Vec<octra_mcp_core::McpToolAcl>>> {
    if let Some(tool_id) = query.tool_id.as_deref()
        && state
            .storage
            .mcp
            .get_tool(tool_id)
            .await
            .map_err(storage)?
            .is_none()
    {
        return Err(ApiError::not_found("MCP tool"));
    }
    Ok(Json(
        state
            .storage
            .mcp
            .list_acls(query.tool_id.as_deref())
            .await
            .map_err(storage)?,
    ))
}

async fn create_acl(
    State(state): State<McpControlState>,
    body: Body,
) -> ApiResult<(StatusCode, Json<octra_mcp_core::McpToolAcl>)> {
    let request: AclRequest = json_body(&state, body).await?;
    if state
        .storage
        .mcp
        .get_tool(&request.tool_id)
        .await
        .map_err(storage)?
        .is_none()
    {
        return Err(ApiError::bad_request(format!(
            "unknown tool_id: {}",
            request.tool_id
        )));
    }
    if !state
        .storage
        .mcp
        .consumer_exists(&request.consumer_id)
        .await
        .map_err(storage)?
    {
        return Err(ApiError::bad_request(format!(
            "unknown consumer_id: {}",
            request.consumer_id
        )));
    }

    let acl = state
        .storage
        .mcp
        .upsert_acl(&request.tool_id, &request.consumer_id, request.allowed)
        .await
        .map_err(storage)?;
    state.runtime.reload_after_mutation().await;
    Ok((StatusCode::CREATED, Json(acl)))
}

async fn get_acl(
    State(state): State<McpControlState>,
    Path(id): Path<String>,
) -> ApiResult<Json<octra_mcp_core::McpToolAcl>> {
    state
        .storage
        .mcp
        .get_acl(&id)
        .await
        .map_err(storage)?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("MCP ACL"))
}

async fn update_acl(
    State(state): State<McpControlState>,
    Path(id): Path<String>,
    body: Body,
) -> ApiResult<Json<octra_mcp_core::McpToolAcl>> {
    let existing = state
        .storage
        .mcp
        .get_acl(&id)
        .await
        .map_err(storage)?
        .ok_or_else(|| ApiError::not_found("MCP ACL"))?;
    let request: AclUpdate = json_body(&state, body).await?;
    let allowed = request.allowed.unwrap_or(existing.allowed);

    let acl = state
        .storage
        .mcp
        .upsert_acl(&existing.tool_id, &existing.consumer_id, allowed)
        .await
        .map_err(storage)?;
    state.runtime.reload_after_mutation().await;
    Ok(Json(acl))
}

async fn delete_acl(
    State(state): State<McpControlState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    if state.storage.mcp.delete_acl(&id).await.map_err(storage)? {
        state.runtime.reload_after_mutation().await;
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found("MCP ACL"))
    }
}

/// Keeps the status vocabulary in this module's public surface for clients
/// that filter invocations by outcome.
pub fn invocation_statuses() -> &'static [&'static str] {
    &[
        status::OK,
        status::ERROR,
        status::TIMEOUT,
        status::DENIED,
        status::NOT_FOUND,
        status::INVALID,
    ]
}
