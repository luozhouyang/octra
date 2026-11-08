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
use octra_mcp_core::domain::{
    MCP_SERVER_MODE_LISTENER, McpServer, McpServerAcl, McpTool, NewMcpServer, default_server_mode,
    effective_route_path, is_source_server_mode, route_path_key, status, validate_route_path,
    validate_secret_reference, validate_server_mode,
};
use octra_mcp_core::{Transport, domain::McpApiKey};
use octra_storage::mcp::DiscoveredTool;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::auth::{display_prefix, generate_key, hash_key};
use crate::data::validate_acl_expressions;
use crate::{DiscoveryStats, McpControlState};

type ApiResult<T> = Result<T, ApiError>;

pub fn router(state: McpControlState) -> Router {
    Router::new()
        .route("/api/v1/mcp/servers", get(list_servers).post(create_server))
        .route(
            "/api/v1/mcp/servers/{id}",
            get(get_server).put(update_server).delete(delete_server),
        )
        .route(
            "/api/v1/mcp/servers/{id}/acls",
            get(list_server_acls).post(create_server_acl),
        )
        .route("/api/v1/mcp/servers/{id}/refresh", post(refresh_server))
        .route(
            "/api/v1/mcp/server-acls/{id}",
            get(get_server_acl)
                .put(update_server_acl)
                .delete(delete_server_acl),
        )
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
    /// Kong calls this field `type`; `mode` is accepted as an alias.
    #[serde(rename = "type", alias = "mode")]
    pub mode: Option<String>,
    pub sources: Option<Vec<String>>,
    pub route_path: Option<String>,
    pub acl_allow_when: Option<Vec<String>>,
    pub acl_deny_when: Option<Vec<String>>,
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
            mode: None,
            sources: None,
            route_path: None,
            acl_allow_when: None,
            acl_deny_when: None,
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
    ///
    /// When `type`/`mode` is omitted the server keeps the legacy
    /// `passthrough-listener` default; callers that update an existing server
    /// restore its stored mode and sources before validating.
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

        let mode = self.mode.unwrap_or_else(default_server_mode);
        validate_server_mode(&mode).map_err(ApiError::bad_request)?;

        let server = NewMcpServer {
            name: self.name.trim().to_string(),
            transport,
            mode,
            sources: self.sources.unwrap_or_default(),
            route_path: self.route_path.map(|path| path.trim().to_string()),
            acl_allow_when: self.acl_allow_when.unwrap_or_default(),
            acl_deny_when: self.acl_deny_when.unwrap_or_default(),
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

/// Validates route path syntax and rejects a path already used by another
/// server. Name-derived defaults (`/mcp/<name>`) participate so an explicit
/// path cannot shadow another server's endpoint.
async fn validate_server_route(
    state: &McpControlState,
    candidate: &NewMcpServer,
    exclude_id: Option<&str>,
) -> ApiResult<()> {
    if let Some(path) = candidate.route_path.as_deref() {
        validate_route_path(path)
            .map_err(|err| ApiError::bad_request(format!("route_path: {err}")))?;
    }

    let candidate_path = effective_route_path(candidate.route_path.as_deref(), &candidate.name);
    let candidate_key = route_path_key(&candidate_path);
    for server in state.storage.mcp.list_servers().await.map_err(storage)? {
        if Some(server.id.as_str()) == exclude_id {
            continue;
        }
        // A duplicate name is reported by the name unique constraint with the
        // established message; do not mask it with a route-path error.
        if server.name == candidate.name {
            return Ok(());
        }
        let existing_path = server.effective_route_path();
        if route_path_key(&existing_path) == candidate_key {
            return Err(ApiError::conflict(format!(
                "route_path '{candidate_path}' is already used by MCP server '{}'",
                server.name
            )));
        }
    }
    Ok(())
}

fn validate_server_acl_expressions(server: &NewMcpServer) -> ApiResult<()> {
    validate_acl_expressions(&server.acl_allow_when, &server.acl_deny_when)
        .map_err(ApiError::bad_request)
}

/// Resolves `sources` references (server ids or names) to canonical server
/// names and rejects references a listener may not aggregate.
///
/// Only `listener` servers may declare sources, and each reference must point
/// at an existing `upstream-server` or `passthrough-listener`.
async fn resolve_sources(
    state: &McpControlState,
    mode: &str,
    sources: &[String],
) -> ApiResult<Vec<String>> {
    if sources.is_empty() {
        return Ok(Vec::new());
    }
    if mode != MCP_SERVER_MODE_LISTENER {
        return Err(ApiError::bad_request(format!(
            "sources can only be set when type is 'listener', not '{mode}'"
        )));
    }

    let mut resolved: Vec<String> = Vec::new();
    for reference in sources {
        let reference = reference.trim();
        if reference.is_empty() {
            return Err(ApiError::bad_request(
                "sources entries must be non-empty server ids or names",
            ));
        }
        let target = state
            .storage
            .mcp
            .get_server(reference)
            .await
            .map_err(storage)?;
        let target = match target {
            Some(target) => Some(target),
            None => state
                .storage
                .mcp
                .get_server_by_name(reference)
                .await
                .map_err(storage)?,
        };
        let Some(target) = target else {
            return Err(ApiError::bad_request(format!(
                "unknown MCP source server: {reference}"
            )));
        };
        if !is_source_server_mode(&target.mode) {
            return Err(ApiError::bad_request(format!(
                "MCP source server '{reference}' has type '{}'; expected 'upstream-server' or \
                 'passthrough-listener'",
                target.mode
            )));
        }
        if !resolved.contains(&target.name) {
            resolved.push(target.name.clone());
        }
    }
    Ok(resolved)
}

#[derive(Debug, Serialize)]
pub struct ServerResponse {
    #[serde(flatten)]
    pub server: McpServer,
    pub tool_count: usize,
}

async fn server_response(
    state: &McpControlState,
    mut server: McpServer,
) -> ApiResult<ServerResponse> {
    let tool_count = state
        .storage
        .mcp
        .list_tools(&server.id)
        .await
        .map_err(storage)?
        .len();
    // Responses always expose the effective path, even when the column is null.
    server.route_path = Some(server.effective_route_path());
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
    let mut new_server = request.into_new_server()?;
    new_server.sources = resolve_sources(&state, &new_server.mode, &new_server.sources).await?;
    validate_server_route(&state, &new_server, None).await?;
    validate_server_acl_expressions(&new_server)?;
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
    let mode_given = request.mode.is_some();
    let sources_given = request.sources.is_some();
    let route_path_given = request.route_path.is_some();
    let allow_when_given = request.acl_allow_when.is_some();
    let deny_when_given = request.acl_deny_when.is_some();
    let mut candidate = request.into_new_server()?;
    if candidate.name.trim().is_empty() {
        candidate.name = existing.name.clone();
    }
    // Omitted fields keep the stored values, mirroring how an omitted name
    // keeps the existing one. An explicit `sources` list still requires
    // `type: listener`.
    if !mode_given {
        candidate.mode = existing.mode.clone();
    }
    if !sources_given {
        candidate.sources = existing.sources.clone();
    }
    if !route_path_given {
        candidate.route_path = existing.route_path.clone();
    }
    if !allow_when_given {
        candidate.acl_allow_when = existing.acl_allow_when.clone();
    }
    if !deny_when_given {
        candidate.acl_deny_when = existing.acl_deny_when.clone();
    }
    candidate.sources = resolve_sources(&state, &candidate.mode, &candidate.sources).await?;
    validate_server_route(&state, &candidate, Some(&id)).await?;
    validate_server_acl_expressions(&candidate)?;

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
    /// Exactly one of `consumer_id` and `consumer_group_id` may be set; with
    /// neither, the rule is the tool default.
    pub consumer_id: Option<String>,
    pub consumer_group_id: Option<String>,
    pub allowed: bool,
}

impl Default for AclRequest {
    fn default() -> Self {
        Self {
            tool_id: String::new(),
            consumer_id: None,
            consumer_group_id: None,
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

/// Validates the two optional scope ids shared by tool and server ACLs.
async fn validate_acl_scope(
    state: &McpControlState,
    consumer_id: Option<&str>,
    consumer_group_id: Option<&str>,
) -> ApiResult<()> {
    if consumer_id.is_some() && consumer_group_id.is_some() {
        return Err(ApiError::bad_request(
            "pass either consumer_id or consumer_group_id, not both",
        ));
    }
    if let Some(consumer_id) = consumer_id
        && !state
            .storage
            .mcp
            .consumer_exists(consumer_id)
            .await
            .map_err(storage)?
    {
        return Err(ApiError::bad_request(format!(
            "unknown consumer_id: {consumer_id}"
        )));
    }
    if let Some(consumer_group_id) = consumer_group_id
        && state
            .storage
            .api
            .get_consumer_group(consumer_group_id)
            .await
            .map_err(storage)?
            .is_none()
    {
        return Err(ApiError::bad_request(format!(
            "unknown consumer_group_id: {consumer_group_id}"
        )));
    }
    Ok(())
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
    validate_acl_scope(
        &state,
        request.consumer_id.as_deref(),
        request.consumer_group_id.as_deref(),
    )
    .await?;

    let acl = state
        .storage
        .mcp
        .upsert_acl(
            &request.tool_id,
            request.consumer_id.as_deref(),
            request.consumer_group_id.as_deref(),
            request.allowed,
        )
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
        .upsert_acl(
            &existing.tool_id,
            existing.consumer_id.as_deref(),
            existing.consumer_group_id.as_deref(),
            allowed,
        )
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

// ----- server ACLs -------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct ServerAclRequest {
    /// Exactly one of `consumer_id` and `consumer_group_id` may be set; with
    /// neither, the rule is the server default.
    pub consumer_id: Option<String>,
    pub consumer_group_id: Option<String>,
    pub allowed: bool,
}

impl Default for ServerAclRequest {
    fn default() -> Self {
        Self {
            consumer_id: None,
            consumer_group_id: None,
            allowed: true,
        }
    }
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct ServerAclUpdate {
    pub allowed: Option<bool>,
}

async fn require_server(state: &McpControlState, id: &str) -> ApiResult<()> {
    if state
        .storage
        .mcp
        .get_server(id)
        .await
        .map_err(storage)?
        .is_none()
    {
        return Err(ApiError::not_found("MCP server"));
    }
    Ok(())
}

async fn list_server_acls(
    State(state): State<McpControlState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<McpServerAcl>>> {
    require_server(&state, &id).await?;
    Ok(Json(
        state
            .storage
            .mcp
            .list_server_acls(&id)
            .await
            .map_err(storage)?,
    ))
}

async fn create_server_acl(
    State(state): State<McpControlState>,
    Path(id): Path<String>,
    body: Body,
) -> ApiResult<(StatusCode, Json<McpServerAcl>)> {
    require_server(&state, &id).await?;
    let request: ServerAclRequest = json_body(&state, body).await?;
    validate_acl_scope(
        &state,
        request.consumer_id.as_deref(),
        request.consumer_group_id.as_deref(),
    )
    .await?;

    let acl = state
        .storage
        .mcp
        .upsert_server_acl(
            &id,
            request.consumer_id.as_deref(),
            request.consumer_group_id.as_deref(),
            request.allowed,
        )
        .await
        .map_err(storage)?;
    state.runtime.reload_after_mutation().await;
    Ok((StatusCode::CREATED, Json(acl)))
}

async fn get_server_acl(
    State(state): State<McpControlState>,
    Path(id): Path<String>,
) -> ApiResult<Json<McpServerAcl>> {
    state
        .storage
        .mcp
        .get_server_acl(&id)
        .await
        .map_err(storage)?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("MCP server ACL"))
}

async fn update_server_acl(
    State(state): State<McpControlState>,
    Path(id): Path<String>,
    body: Body,
) -> ApiResult<Json<McpServerAcl>> {
    let existing = state
        .storage
        .mcp
        .get_server_acl(&id)
        .await
        .map_err(storage)?
        .ok_or_else(|| ApiError::not_found("MCP server ACL"))?;
    let request: ServerAclUpdate = json_body(&state, body).await?;
    let allowed = request.allowed.unwrap_or(existing.allowed);

    let acl = state
        .storage
        .mcp
        .upsert_server_acl(
            &existing.server_id,
            existing.consumer_id.as_deref(),
            existing.consumer_group_id.as_deref(),
            allowed,
        )
        .await
        .map_err(storage)?;
    state.runtime.reload_after_mutation().await;
    Ok(Json(acl))
}

async fn delete_server_acl(
    State(state): State<McpControlState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    if state
        .storage
        .mcp
        .delete_server_acl(&id)
        .await
        .map_err(storage)?
    {
        state.runtime.reload_after_mutation().await;
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found("MCP server ACL"))
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
