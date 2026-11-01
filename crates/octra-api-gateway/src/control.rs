//! Control plane CRUD for the API gateway configuration.
//!
//! Every mutation reloads the runtime snapshot synchronously so the data plane
//! observes the change before the response is returned.

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use octra_api_core::{
    ApiCredential, Consumer, CreateApiCredential, CreateConsumer, CreateRoute, CreateService,
    CreateTarget, CreateUpstream, Route, Service, Target, UpdateApiCredential, UpdateConsumer,
    UpdateRoute, UpdateService, UpdateTarget, UpdateUpstream, Upstream,
};
use octra_core::SnapshotInfo;
use octra_plugin_api::Scope;
use octra_storage::plugins::{PluginConfigRecord, PluginRecord};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::ApiControlState;
use crate::error::{ControlError, ControlResult};
use crate::key::generate_api_key;

const MAX_RETRIES: i64 = 10;

pub fn router(state: ApiControlState) -> Router {
    Router::new()
        .route("/api/v1/services", get(list_services).post(create_service))
        .route(
            "/api/v1/services/{id}",
            get(get_service).put(update_service).delete(delete_service),
        )
        .route("/api/v1/routes", get(list_routes).post(create_route))
        .route(
            "/api/v1/routes/{id}",
            get(get_route).put(update_route).delete(delete_route),
        )
        .route(
            "/api/v1/upstreams",
            get(list_upstreams).post(create_upstream),
        )
        .route(
            "/api/v1/upstreams/{id}",
            get(get_upstream)
                .put(update_upstream)
                .delete(delete_upstream),
        )
        .route("/api/v1/targets", get(list_targets).post(create_target))
        .route(
            "/api/v1/targets/{id}",
            get(get_target).put(update_target).delete(delete_target),
        )
        .route(
            "/api/v1/consumers",
            get(list_consumers).post(create_consumer),
        )
        .route(
            "/api/v1/consumers/{id}",
            get(get_consumer)
                .put(update_consumer)
                .delete(delete_consumer),
        )
        .route(
            "/api/v1/credentials",
            get(list_credentials).post(create_credential),
        )
        .route(
            "/api/v1/credentials/{id}",
            get(get_credential)
                .put(update_credential)
                .delete(delete_credential),
        )
        .route("/api/v1/api/reload", post(reload))
        .route("/api/v1/api/metrics", get(metrics))
        .route("/api/v1/plugins", get(list_plugins))
        .route("/api/v1/plugins/reload", post(reload_plugins))
        .route("/api/v1/plugins/{id}", get(get_plugin).put(update_plugin))
        .route(
            "/api/v1/plugins/{id}/configs",
            get(list_plugin_configs).post(create_plugin_config),
        )
        .route(
            "/api/v1/plugin-configs/{id}",
            get(get_plugin_config)
                .put(update_plugin_config)
                .delete(delete_plugin_config_route),
        )
        .with_state(state)
}

#[derive(Debug, Serialize)]
pub struct PluginView {
    pub id: String,
    pub name: String,
    pub version: String,
    pub kind: String,
    pub runtime: String,
    pub enabled: bool,
    pub priority: i64,
    pub description: String,
    pub capabilities: Value,
    pub config_schema: Value,
}

#[derive(Debug, Serialize)]
pub struct PluginConfigView {
    pub id: String,
    pub plugin_id: String,
    pub scope: String,
    pub scope_id: String,
    pub enabled: bool,
    pub config: Value,
}

#[derive(Debug, Deserialize)]
pub struct UpdatePluginRequest {
    pub enabled: Option<bool>,
    pub priority: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct CreatePluginConfigRequest {
    pub scope: String,
    pub scope_id: Option<String>,
    pub enabled: Option<bool>,
    pub config: Option<Value>,
}

#[derive(Debug, Deserialize)]
pub struct UpdatePluginConfigRequest {
    pub enabled: Option<bool>,
    pub config: Option<Value>,
}

fn plugin_view(record: PluginRecord) -> PluginView {
    let manifest: Value = serde_json::from_str(&record.manifest_json).unwrap_or(Value::Null);
    let capabilities = manifest
        .get("capabilities")
        .cloned()
        .unwrap_or_else(|| Value::Array(Vec::new()));
    let config_schema: Value =
        serde_json::from_str(&record.config_schema_json).unwrap_or_else(|_| json!({}));
    PluginView {
        id: record.id,
        name: record.name,
        version: record.version,
        kind: record.kind,
        runtime: record.runtime_type,
        enabled: record.enabled,
        priority: record.priority,
        description: record.description,
        capabilities,
        config_schema,
    }
}

fn config_view(record: PluginConfigRecord) -> PluginConfigView {
    let config: Value = serde_json::from_str(&record.config_json).unwrap_or(Value::Null);
    PluginConfigView {
        id: record.id,
        plugin_id: record.plugin_id,
        scope: record.scope,
        scope_id: record.scope_id,
        enabled: record.enabled,
        config,
    }
}

fn plugin_not_found(id: &str) -> ControlError {
    ControlError::not_found(format!("plugin '{id}' not found"))
}

fn config_not_found(id: &str) -> ControlError {
    ControlError::not_found(format!("plugin config '{id}' not found"))
}

async fn reload_plugin_runtime(state: &ApiControlState) -> ControlResult<()> {
    state.plugins.reload().await.map_err(ControlError::from)
}

async fn validate_scope_target(
    state: &ApiControlState,
    scope: Scope,
    scope_id: &str,
) -> ControlResult<()> {
    match scope {
        Scope::Global => Ok(()),
        Scope::Service => {
            if state.storage.get_service(scope_id).await?.is_some() {
                Ok(())
            } else {
                Err(not_found("service", scope_id))
            }
        }
        Scope::Route => {
            if state.storage.get_route(scope_id).await?.is_some() {
                Ok(())
            } else {
                Err(not_found("route", scope_id))
            }
        }
        Scope::Consumer => {
            if state.storage.get_consumer(scope_id).await?.is_some() {
                Ok(())
            } else {
                Err(not_found("consumer", scope_id))
            }
        }
    }
}

async fn validate_plugin_config(
    state: &ApiControlState,
    plugin: &PluginRecord,
    config: &Value,
) -> ControlResult<()> {
    state
        .plugins
        .registry()
        .validate(&plugin.name, config)
        .map_err(|error| ControlError::bad_request(error.message))
}

/// `GET /api/v1/plugins`.
pub async fn list_plugins(
    State(state): State<ApiControlState>,
) -> ControlResult<Json<Vec<PluginView>>> {
    let plugins = state.storage.list_plugins().await?;
    Ok(Json(plugins.into_iter().map(plugin_view).collect()))
}

/// `GET /api/v1/plugins/{id}`.
pub async fn get_plugin(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
) -> ControlResult<Json<PluginView>> {
    let plugin = state
        .storage
        .get_plugin(&id)
        .await?
        .ok_or_else(|| plugin_not_found(&id))?;
    Ok(Json(plugin_view(plugin)))
}

/// `PUT /api/v1/plugins/{id}`.
pub async fn update_plugin(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
    payload: Result<Json<UpdatePluginRequest>, JsonRejection>,
) -> ControlResult<Json<PluginView>> {
    let request = read_json(payload).await?;
    if let Some(priority) = request.priority
        && !(0..=1000).contains(&priority)
    {
        return Err(ControlError::bad_request(
            "priority must be between 0 and 1000",
        ));
    }
    let plugin = state
        .storage
        .update_plugin_flags(&id, request.enabled, request.priority)
        .await?
        .ok_or_else(|| plugin_not_found(&id))?;
    reload_plugin_runtime(&state).await?;
    Ok(Json(plugin_view(plugin)))
}

/// `GET /api/v1/plugins/{id}/configs`.
pub async fn list_plugin_configs(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
) -> ControlResult<Json<Vec<PluginConfigView>>> {
    if state.storage.get_plugin(&id).await?.is_none() {
        return Err(plugin_not_found(&id));
    }
    let configs = state.storage.list_plugin_configs(&id).await?;
    Ok(Json(configs.into_iter().map(config_view).collect()))
}

/// `POST /api/v1/plugins/{id}/configs`.
pub async fn create_plugin_config(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
    payload: Result<Json<CreatePluginConfigRequest>, JsonRejection>,
) -> ControlResult<(StatusCode, Json<PluginConfigView>)> {
    let request = read_json(payload).await?;
    let plugin = state
        .storage
        .get_plugin(&id)
        .await?
        .ok_or_else(|| plugin_not_found(&id))?;
    let scope = Scope::parse(&request.scope).ok_or_else(|| {
        ControlError::bad_request("scope must be one of global, service, route, consumer")
    })?;
    let scope_id = scope
        .normalize_id(request.scope_id.as_deref())
        .ok_or_else(|| ControlError::bad_request(format!("{scope} scope requires a scope_id")))?;
    validate_scope_target(&state, scope, &scope_id).await?;

    let config = request.config.unwrap_or_else(|| json!({}));
    validate_plugin_config(&state, &plugin, &config).await?;
    let config_json = serde_json::to_string(&config)
        .map_err(|error| ControlError::bad_request(error.to_string()))?;

    let record = state
        .storage
        .create_plugin_config(
            &id,
            scope,
            request.scope_id.as_deref(),
            request.enabled.unwrap_or(true),
            &config_json,
        )
        .await?;
    reload_plugin_runtime(&state).await?;
    Ok((StatusCode::CREATED, Json(config_view(record))))
}

/// `GET /api/v1/plugin-configs/{id}`.
pub async fn get_plugin_config(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
) -> ControlResult<Json<PluginConfigView>> {
    let record = state
        .storage
        .get_plugin_config(&id)
        .await?
        .ok_or_else(|| config_not_found(&id))?;
    Ok(Json(config_view(record)))
}

/// `PUT /api/v1/plugin-configs/{id}`.
pub async fn update_plugin_config(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
    payload: Result<Json<UpdatePluginConfigRequest>, JsonRejection>,
) -> ControlResult<Json<PluginConfigView>> {
    let request = read_json(payload).await?;
    let existing = state
        .storage
        .get_plugin_config(&id)
        .await?
        .ok_or_else(|| config_not_found(&id))?;

    let config_json = match &request.config {
        Some(config) => {
            let plugin = state
                .storage
                .get_plugin(&existing.plugin_id)
                .await?
                .ok_or_else(|| plugin_not_found(&existing.plugin_id))?;
            validate_plugin_config(&state, &plugin, config).await?;
            Some(
                serde_json::to_string(config)
                    .map_err(|error| ControlError::bad_request(error.to_string()))?,
            )
        }
        None => None,
    };

    let record = state
        .storage
        .update_plugin_config(&id, request.enabled, config_json.as_deref())
        .await?
        .ok_or_else(|| config_not_found(&id))?;
    reload_plugin_runtime(&state).await?;
    Ok(Json(config_view(record)))
}

/// `DELETE /api/v1/plugin-configs/{id}`.
pub async fn delete_plugin_config_route(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
) -> ControlResult<StatusCode> {
    if !state.storage.delete_plugin_config(&id).await? {
        return Err(config_not_found(&id));
    }
    reload_plugin_runtime(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/v1/plugins/reload`.
pub async fn reload_plugins(State(state): State<ApiControlState>) -> ControlResult<StatusCode> {
    reload_plugin_runtime(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/v1/api/metrics` on the authenticated control plane.
pub async fn metrics(State(state): State<ApiControlState>) -> Json<crate::MetricsSnapshot> {
    Json(state.metrics.snapshot())
}

#[derive(Debug, Default, Deserialize)]
pub struct ListFilter {
    pub service_id: Option<String>,
    pub upstream_id: Option<String>,
    pub consumer_id: Option<String>,
}

async fn read_json<T>(payload: Result<Json<T>, JsonRejection>) -> ControlResult<T> {
    payload
        .map(|Json(value)| value)
        .map_err(|rejection| ControlError::bad_request(rejection.body_text()))
}

async fn reload_runtime(state: &ApiControlState) -> ControlResult<SnapshotInfo> {
    state.runtime.reload().await.map_err(ControlError::from)
}

/// `POST /api/v1/api/reload`.
pub async fn reload(State(state): State<ApiControlState>) -> ControlResult<Json<SnapshotInfo>> {
    Ok(Json(reload_runtime(&state).await?))
}

fn checked_retries(retries: i64) -> ControlResult<()> {
    if (0..=MAX_RETRIES).contains(&retries) {
        Ok(())
    } else {
        Err(ControlError::bad_request(format!(
            "retries must be between 0 and {MAX_RETRIES}"
        )))
    }
}

fn not_found(entity: &str, id: &str) -> ControlError {
    ControlError::not_found(format!("{entity} '{id}' not found"))
}

async fn require_service(state: &ApiControlState, id: &str) -> ControlResult<()> {
    if state.storage.get_service(id).await?.is_none() {
        return Err(not_found("service", id));
    }
    Ok(())
}

async fn require_upstream(state: &ApiControlState, id: &str) -> ControlResult<()> {
    if state.storage.get_upstream(id).await?.is_none() {
        return Err(not_found("upstream", id));
    }
    Ok(())
}

async fn require_consumer(state: &ApiControlState, id: &str) -> ControlResult<()> {
    if state.storage.get_consumer(id).await?.is_none() {
        return Err(not_found("consumer", id));
    }
    Ok(())
}

// Services

pub async fn list_services(
    State(state): State<ApiControlState>,
) -> ControlResult<Json<Vec<Service>>> {
    Ok(Json(state.storage.list_services().await?))
}

pub async fn get_service(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
) -> ControlResult<Json<Service>> {
    state
        .storage
        .get_service(&id)
        .await?
        .map(Json)
        .ok_or_else(|| not_found("service", &id))
}

pub async fn create_service(
    State(state): State<ApiControlState>,
    payload: Result<Json<CreateService>, JsonRejection>,
) -> ControlResult<(StatusCode, Json<Service>)> {
    let input = read_json(payload).await?;
    input.validate()?;
    if let Some(retries) = input.retries {
        checked_retries(retries)?;
    }
    let service = state.storage.create_service(&input).await?;
    reload_runtime(&state).await?;
    Ok((StatusCode::CREATED, Json(service)))
}

pub async fn update_service(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
    payload: Result<Json<UpdateService>, JsonRejection>,
) -> ControlResult<Json<Service>> {
    let input = read_json(payload).await?;
    input.validate()?;
    if let Some(retries) = input.retries {
        checked_retries(retries)?;
    }
    let service = state.storage.update_service(&id, &input).await?;
    reload_runtime(&state).await?;
    Ok(Json(service))
}

pub async fn delete_service(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
) -> ControlResult<StatusCode> {
    state.storage.delete_service(&id).await?;
    reload_runtime(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}

// Routes

pub async fn list_routes(
    State(state): State<ApiControlState>,
    Query(filter): Query<ListFilter>,
) -> ControlResult<Json<Vec<Route>>> {
    Ok(Json(
        state
            .storage
            .list_routes(filter.service_id.as_deref())
            .await?,
    ))
}

pub async fn get_route(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
) -> ControlResult<Json<Route>> {
    state
        .storage
        .get_route(&id)
        .await?
        .map(Json)
        .ok_or_else(|| not_found("route", &id))
}

pub async fn create_route(
    State(state): State<ApiControlState>,
    payload: Result<Json<CreateRoute>, JsonRejection>,
) -> ControlResult<(StatusCode, Json<Route>)> {
    let input = read_json(payload).await?;
    input.validate()?;
    require_service(&state, &input.service_id).await?;
    let route = state.storage.create_route(&input).await?;
    reload_runtime(&state).await?;
    Ok((StatusCode::CREATED, Json(route)))
}

pub async fn update_route(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
    payload: Result<Json<UpdateRoute>, JsonRejection>,
) -> ControlResult<Json<Route>> {
    let input = read_json(payload).await?;
    input.validate()?;
    if let Some(service_id) = &input.service_id {
        require_service(&state, service_id).await?;
    }
    let route = state.storage.update_route(&id, &input).await?;
    reload_runtime(&state).await?;
    Ok(Json(route))
}

pub async fn delete_route(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
) -> ControlResult<StatusCode> {
    state.storage.delete_route(&id).await?;
    reload_runtime(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}

// Upstreams

pub async fn list_upstreams(
    State(state): State<ApiControlState>,
    Query(filter): Query<ListFilter>,
) -> ControlResult<Json<Vec<Upstream>>> {
    Ok(Json(
        state
            .storage
            .list_upstreams(filter.service_id.as_deref())
            .await?,
    ))
}

pub async fn get_upstream(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
) -> ControlResult<Json<Upstream>> {
    state
        .storage
        .get_upstream(&id)
        .await?
        .map(Json)
        .ok_or_else(|| not_found("upstream", &id))
}

pub async fn create_upstream(
    State(state): State<ApiControlState>,
    payload: Result<Json<CreateUpstream>, JsonRejection>,
) -> ControlResult<(StatusCode, Json<Upstream>)> {
    let input = read_json(payload).await?;
    input.validate()?;
    require_service(&state, &input.service_id).await?;
    let upstream = state.storage.create_upstream(&input).await?;
    reload_runtime(&state).await?;
    Ok((StatusCode::CREATED, Json(upstream)))
}

pub async fn update_upstream(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
    payload: Result<Json<UpdateUpstream>, JsonRejection>,
) -> ControlResult<Json<Upstream>> {
    let input = read_json(payload).await?;
    input.validate()?;
    if let Some(service_id) = &input.service_id {
        require_service(&state, service_id).await?;
    }
    let upstream = state.storage.update_upstream(&id, &input).await?;
    reload_runtime(&state).await?;
    Ok(Json(upstream))
}

pub async fn delete_upstream(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
) -> ControlResult<StatusCode> {
    state.storage.delete_upstream(&id).await?;
    reload_runtime(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}

// Targets

pub async fn list_targets(
    State(state): State<ApiControlState>,
    Query(filter): Query<ListFilter>,
) -> ControlResult<Json<Vec<Target>>> {
    Ok(Json(
        state
            .storage
            .list_targets(filter.upstream_id.as_deref())
            .await?,
    ))
}

pub async fn get_target(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
) -> ControlResult<Json<Target>> {
    state
        .storage
        .get_target(&id)
        .await?
        .map(Json)
        .ok_or_else(|| not_found("target", &id))
}

pub async fn create_target(
    State(state): State<ApiControlState>,
    payload: Result<Json<CreateTarget>, JsonRejection>,
) -> ControlResult<(StatusCode, Json<Target>)> {
    let input = read_json(payload).await?;
    input.validate()?;
    require_upstream(&state, &input.upstream_id).await?;
    let target = state.storage.create_target(&input).await?;
    reload_runtime(&state).await?;
    Ok((StatusCode::CREATED, Json(target)))
}

pub async fn update_target(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
    payload: Result<Json<UpdateTarget>, JsonRejection>,
) -> ControlResult<Json<Target>> {
    let input = read_json(payload).await?;
    input.validate()?;
    if let Some(upstream_id) = &input.upstream_id {
        require_upstream(&state, upstream_id).await?;
    }
    let target = state.storage.update_target(&id, &input).await?;
    reload_runtime(&state).await?;
    Ok(Json(target))
}

pub async fn delete_target(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
) -> ControlResult<StatusCode> {
    state.storage.delete_target(&id).await?;
    reload_runtime(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}

// Consumers

pub async fn list_consumers(
    State(state): State<ApiControlState>,
) -> ControlResult<Json<Vec<Consumer>>> {
    Ok(Json(state.storage.list_consumers().await?))
}

pub async fn get_consumer(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
) -> ControlResult<Json<Consumer>> {
    state
        .storage
        .get_consumer(&id)
        .await?
        .map(Json)
        .ok_or_else(|| not_found("consumer", &id))
}

pub async fn create_consumer(
    State(state): State<ApiControlState>,
    payload: Result<Json<CreateConsumer>, JsonRejection>,
) -> ControlResult<(StatusCode, Json<Consumer>)> {
    let input = read_json(payload).await?;
    input.validate()?;
    let consumer = state.storage.create_consumer(&input).await?;
    Ok((StatusCode::CREATED, Json(consumer)))
}

pub async fn update_consumer(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
    payload: Result<Json<UpdateConsumer>, JsonRejection>,
) -> ControlResult<Json<Consumer>> {
    let input = read_json(payload).await?;
    input.validate()?;
    let consumer = state.storage.update_consumer(&id, &input).await?;
    Ok(Json(consumer))
}

pub async fn delete_consumer(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
) -> ControlResult<StatusCode> {
    state.storage.delete_consumer(&id).await?;
    reload_runtime(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}

// Credentials

/// Response for a freshly minted credential. The plaintext key appears exactly
/// once here and is never stored or logged.
#[derive(Debug, Serialize)]
pub struct IssuedCredential {
    pub credential: ApiCredential,
    pub key: String,
}

pub async fn list_credentials(
    State(state): State<ApiControlState>,
    Query(filter): Query<ListFilter>,
) -> ControlResult<Json<Vec<ApiCredential>>> {
    Ok(Json(
        state
            .storage
            .list_credentials(filter.consumer_id.as_deref())
            .await?,
    ))
}

pub async fn get_credential(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
) -> ControlResult<Json<ApiCredential>> {
    state
        .storage
        .get_credential(&id)
        .await?
        .map(Json)
        .ok_or_else(|| not_found("credential", &id))
}

pub async fn create_credential(
    State(state): State<ApiControlState>,
    payload: Result<Json<CreateApiCredential>, JsonRejection>,
) -> ControlResult<(StatusCode, Json<IssuedCredential>)> {
    let input = read_json(payload).await?;
    input.validate()?;
    require_consumer(&state, &input.consumer_id).await?;

    let generated = generate_api_key();
    let credential = state
        .storage
        .create_credential(&input, &generated.prefix, &generated.hash)
        .await?;
    reload_runtime(&state).await?;

    Ok((
        StatusCode::CREATED,
        Json(IssuedCredential {
            credential,
            key: generated.plaintext,
        }),
    ))
}

pub async fn update_credential(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
    payload: Result<Json<UpdateApiCredential>, JsonRejection>,
) -> ControlResult<Json<ApiCredential>> {
    let input = read_json(payload).await?;
    input.validate()?;
    if let Some(consumer_id) = &input.consumer_id {
        require_consumer(&state, consumer_id).await?;
    }
    let credential = state.storage.update_credential(&id, &input).await?;
    reload_runtime(&state).await?;
    Ok(Json(credential))
}

pub async fn delete_credential(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
) -> ControlResult<StatusCode> {
    state.storage.delete_credential(&id).await?;
    reload_runtime(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}
