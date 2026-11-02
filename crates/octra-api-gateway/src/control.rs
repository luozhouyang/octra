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
    Acl, BasicauthCredential, Consumer, CreateAcl, CreateBasicauthCredential, CreateConsumer,
    CreateJwtSecret, CreateKeyauthCredential, CreatePluginInstance, CreateRoute, CreateService,
    CreateTarget, CreateUpstream, JwtSecret, KeyauthCredential, PluginInstance, Route, Service,
    Target, UpdateAcl, UpdateBasicauthCredential, UpdateConsumer, UpdateJwtSecret,
    UpdateKeyauthCredential, UpdatePluginInstance, UpdateRoute, UpdateService, UpdateTarget,
    UpdateUpstream, Upstream,
};
use octra_core::SnapshotInfo;
use octra_plugin_api::PluginManifest;
use octra_storage::{AuditLog, RequestLog};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::ApiControlState;
use crate::error::{ControlError, ControlResult};
use crate::key::generate_api_key;

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
        .route(
            "/api/v1/basic-auth",
            get(list_basicauth).post(create_basicauth),
        )
        .route(
            "/api/v1/basic-auth/{id}",
            get(get_basicauth)
                .put(update_basicauth)
                .delete(delete_basicauth),
        )
        .route("/api/v1/jwt", get(list_jwt).post(create_jwt))
        .route(
            "/api/v1/jwt/{id}",
            get(get_jwt).put(update_jwt).delete(delete_jwt),
        )
        .route("/api/v1/acls", get(list_acls).post(create_acl))
        .route(
            "/api/v1/acls/{id}",
            get(get_acl).put(update_acl).delete(delete_acl),
        )
        .route("/api/v1/api/reload", post(reload))
        .route("/api/v1/api/metrics", get(metrics))
        .route("/api/v1/api/metrics/prometheus", get(prometheus_metrics))
        .route("/api/v1/system/logs", get(system_logs))
        .route("/api/v1/audit-logs", get(audit_logs))
        .route("/api/v1/plugins", get(list_plugins))
        .route("/api/v1/plugins/reload", post(reload_plugins))
        .route("/api/v1/plugins/{name}", get(get_plugin))
        .route(
            "/api/v1/plugins/{name}/instances",
            get(list_plugin_instances).post(create_plugin_instance),
        )
        .route(
            "/api/v1/plugin-instances/{id}",
            get(get_plugin_instance)
                .put(update_plugin_instance)
                .delete(delete_plugin_instance),
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
    pub priority: i64,
    pub description: String,
    pub capabilities: Value,
    pub config_schema: Value,
}

fn plugin_view(manifest: PluginManifest) -> PluginView {
    PluginView {
        id: format!("plg_{}", manifest.name),
        name: manifest.name,
        version: manifest.version,
        kind: manifest.kind.as_str().to_string(),
        runtime: manifest.runtime.as_str().to_string(),
        priority: i64::from(manifest.priority),
        description: manifest.description,
        capabilities: serde_json::to_value(manifest.capabilities).unwrap_or(Value::Array(vec![])),
        config_schema: manifest.config_schema.unwrap_or_else(|| json!({})),
    }
}

async fn reload_plugin_runtime(state: &ApiControlState) -> ControlResult<()> {
    state.plugins.reload().await.map_err(ControlError::from)
}

async fn validate_plugin_config(
    state: &ApiControlState,
    name: &str,
    config: &Value,
) -> ControlResult<()> {
    state
        .plugins
        .registry()
        .validate(name, config)
        .map_err(|error| ControlError::bad_request(error.message))
}

/// `GET /api/v1/plugins`.
pub async fn list_plugins(
    State(state): State<ApiControlState>,
) -> ControlResult<Json<Vec<PluginView>>> {
    Ok(Json(
        state
            .plugins
            .registry()
            .manifests()
            .into_iter()
            .map(plugin_view)
            .collect(),
    ))
}

/// `GET /api/v1/plugins/{name}`.
pub async fn get_plugin(
    State(state): State<ApiControlState>,
    Path(name): Path<String>,
) -> ControlResult<Json<PluginView>> {
    state
        .plugins
        .registry()
        .factory(&name)
        .map(|factory| Json(plugin_view(factory.manifest())))
        .ok_or_else(|| ControlError::not_found(format!("plugin '{name}' not found")))
}

/// `GET /api/v1/plugins/{name}/instances`.
pub async fn list_plugin_instances(
    State(state): State<ApiControlState>,
    Path(name): Path<String>,
) -> ControlResult<Json<Vec<PluginInstance>>> {
    let instances: Vec<PluginInstance> = state
        .storage
        .list_plugin_instances()
        .await?
        .into_iter()
        .filter(|instance| instance.name == name)
        .collect();
    Ok(Json(instances))
}

/// `POST /api/v1/plugins/{name}/instances`.
pub async fn create_plugin_instance(
    State(state): State<ApiControlState>,
    Path(name): Path<String>,
    payload: Result<Json<CreatePluginInstance>, JsonRejection>,
) -> ControlResult<(StatusCode, Json<PluginInstance>)> {
    let mut input = read_json(payload).await?;
    input.name = name.clone();
    input.validate()?;
    if state.plugins.registry().factory(&name).is_none() {
        return Err(ControlError::not_found(format!(
            "plugin '{name}' not found"
        )));
    }
    let config = input.config.clone().unwrap_or_else(|| json!({}));
    validate_plugin_config(&state, &name, &config).await?;
    let instance = state.storage.create_plugin_instance(&input).await?;
    reload_plugin_runtime(&state).await?;
    Ok((StatusCode::CREATED, Json(instance)))
}

/// `GET /api/v1/plugin-instances/{id}`.
pub async fn get_plugin_instance(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
) -> ControlResult<Json<PluginInstance>> {
    state
        .storage
        .get_plugin_instance(&id)
        .await?
        .map(Json)
        .ok_or_else(|| ControlError::not_found(format!("plugin instance '{id}' not found")))
}

/// `PUT /api/v1/plugin-instances/{id}`.
pub async fn update_plugin_instance(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
    payload: Result<Json<UpdatePluginInstance>, JsonRejection>,
) -> ControlResult<Json<PluginInstance>> {
    let input = read_json(payload).await?;
    input.validate()?;
    let existing = state
        .storage
        .get_plugin_instance(&id)
        .await?
        .ok_or_else(|| ControlError::not_found(format!("plugin instance '{id}' not found")))?;
    if let Some(config) = &input.config {
        validate_plugin_config(&state, &existing.name, config).await?;
    }
    let instance = state.storage.update_plugin_instance(&id, &input).await?;
    reload_plugin_runtime(&state).await?;
    Ok(Json(instance))
}

/// `DELETE /api/v1/plugin-instances/{id}`.
pub async fn delete_plugin_instance(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
) -> ControlResult<StatusCode> {
    state.storage.delete_plugin_instance(&id).await?;
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

/// `GET /api/v1/api/metrics/prometheus`.
pub async fn prometheus_metrics(
    State(state): State<ApiControlState>,
) -> impl axum::response::IntoResponse {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        state.metrics.prometheus(),
    )
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
    reload_plugin_runtime(&state).await?;
    Ok(Json(reload_runtime(&state).await?))
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
    if let Some(service_id) = &input.service_id {
        require_service(&state, service_id).await?;
    }
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
    if let Some(Some(service_id)) = &input.service_id {
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
) -> ControlResult<Json<Vec<Upstream>>> {
    Ok(Json(state.storage.list_upstreams().await?))
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
    reload_runtime(&state).await?;
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
    reload_runtime(&state).await?;
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

// Key-auth credentials

pub async fn list_credentials(
    State(state): State<ApiControlState>,
    Query(filter): Query<ListFilter>,
) -> ControlResult<Json<Vec<KeyauthCredential>>> {
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
) -> ControlResult<Json<KeyauthCredential>> {
    state
        .storage
        .get_credential(&id)
        .await?
        .map(Json)
        .ok_or_else(|| not_found("credential", &id))
}

pub async fn create_credential(
    State(state): State<ApiControlState>,
    payload: Result<Json<CreateKeyauthCredential>, JsonRejection>,
) -> ControlResult<(StatusCode, Json<KeyauthCredential>)> {
    let mut input = read_json(payload).await?;
    input.validate()?;
    require_consumer(&state, &input.consumer_id).await?;
    if input.key.is_none() {
        input.key = Some(generate_api_key().plaintext);
    }
    let credential = state.storage.create_credential(&input).await?;
    reload_runtime(&state).await?;
    Ok((StatusCode::CREATED, Json(credential)))
}

pub async fn update_credential(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
    payload: Result<Json<UpdateKeyauthCredential>, JsonRejection>,
) -> ControlResult<Json<KeyauthCredential>> {
    let input = read_json(payload).await?;
    input.validate()?;
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

// Basic-auth credentials

pub async fn list_basicauth(
    State(state): State<ApiControlState>,
) -> ControlResult<Json<Vec<BasicauthCredential>>> {
    Ok(Json(state.storage.list_basicauth().await?))
}

pub async fn get_basicauth(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
) -> ControlResult<Json<BasicauthCredential>> {
    state
        .storage
        .get_basicauth(&id)
        .await?
        .map(Json)
        .ok_or_else(|| not_found("credential", &id))
}

pub async fn create_basicauth(
    State(state): State<ApiControlState>,
    payload: Result<Json<CreateBasicauthCredential>, JsonRejection>,
) -> ControlResult<(StatusCode, Json<BasicauthCredential>)> {
    let mut input = read_json(payload).await?;
    input.validate()?;
    require_consumer(&state, &input.consumer_id).await?;
    input.password = hash_password(input.password).await?;
    let credential = state.storage.create_basicauth(&input).await?;
    reload_runtime(&state).await?;
    Ok((StatusCode::CREATED, Json(credential)))
}

pub async fn update_basicauth(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
    payload: Result<Json<UpdateBasicauthCredential>, JsonRejection>,
) -> ControlResult<Json<BasicauthCredential>> {
    let mut input = read_json(payload).await?;
    input.validate()?;
    if let Some(password) = input.password.take() {
        input.password = Some(hash_password(password).await?);
    }
    let credential = state.storage.update_basicauth(&id, &input).await?;
    reload_runtime(&state).await?;
    Ok(Json(credential))
}

pub async fn delete_basicauth(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
) -> ControlResult<StatusCode> {
    state.storage.delete_basicauth(&id).await?;
    reload_runtime(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn hash_password(password: String) -> ControlResult<String> {
    tokio::task::spawn_blocking(move || bcrypt::hash(password, bcrypt::DEFAULT_COST))
        .await
        .map_err(|error| ControlError::bad_request(format!("cannot hash password: {error}")))?
        .map_err(|error| ControlError::bad_request(format!("cannot hash password: {error}")))
}

// JWT secrets

pub async fn list_jwt(State(state): State<ApiControlState>) -> ControlResult<Json<Vec<JwtSecret>>> {
    Ok(Json(state.storage.list_jwt().await?))
}

pub async fn get_jwt(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
) -> ControlResult<Json<JwtSecret>> {
    state
        .storage
        .get_jwt(&id)
        .await?
        .map(Json)
        .ok_or_else(|| not_found("credential", &id))
}

pub async fn create_jwt(
    State(state): State<ApiControlState>,
    payload: Result<Json<CreateJwtSecret>, JsonRejection>,
) -> ControlResult<(StatusCode, Json<JwtSecret>)> {
    let input = read_json(payload).await?;
    input.validate()?;
    require_consumer(&state, &input.consumer_id).await?;
    let credential = state.storage.create_jwt(&input).await?;
    reload_runtime(&state).await?;
    Ok((StatusCode::CREATED, Json(credential)))
}

pub async fn update_jwt(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
    payload: Result<Json<UpdateJwtSecret>, JsonRejection>,
) -> ControlResult<Json<JwtSecret>> {
    let input = read_json(payload).await?;
    input.validate()?;
    let credential = state.storage.update_jwt(&id, &input).await?;
    reload_runtime(&state).await?;
    Ok(Json(credential))
}

pub async fn delete_jwt(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
) -> ControlResult<StatusCode> {
    state.storage.delete_jwt(&id).await?;
    reload_runtime(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ACL entries

pub async fn list_acls(
    State(state): State<ApiControlState>,
    Query(filter): Query<ListFilter>,
) -> ControlResult<Json<Vec<Acl>>> {
    Ok(Json(
        state
            .storage
            .list_acls(filter.consumer_id.as_deref())
            .await?,
    ))
}

pub async fn get_acl(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
) -> ControlResult<Json<Acl>> {
    state
        .storage
        .get_acl(&id)
        .await?
        .map(Json)
        .ok_or_else(|| not_found("acl", &id))
}

pub async fn create_acl(
    State(state): State<ApiControlState>,
    payload: Result<Json<CreateAcl>, JsonRejection>,
) -> ControlResult<(StatusCode, Json<Acl>)> {
    let input = read_json(payload).await?;
    input.validate()?;
    require_consumer(&state, &input.consumer_id).await?;
    let acl = state.storage.create_acl(&input).await?;
    reload_runtime(&state).await?;
    Ok((StatusCode::CREATED, Json(acl)))
}

pub async fn update_acl(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
    payload: Result<Json<UpdateAcl>, JsonRejection>,
) -> ControlResult<Json<Acl>> {
    let input = read_json(payload).await?;
    input.validate()?;
    let acl = state.storage.update_acl(&id, &input).await?;
    reload_runtime(&state).await?;
    Ok(Json(acl))
}

pub async fn delete_acl(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
) -> ControlResult<StatusCode> {
    state.storage.delete_acl(&id).await?;
    reload_runtime(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Default, Deserialize)]
pub struct RequestLogQuery {
    pub limit: Option<u32>,
    pub request_id: Option<String>,
    pub route_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct AuditLogQuery {
    pub limit: Option<u32>,
}

/// `GET /api/v1/system/logs`.
pub async fn system_logs(
    State(state): State<ApiControlState>,
    Query(query): Query<RequestLogQuery>,
) -> ControlResult<Json<Vec<RequestLog>>> {
    let logs = state
        .storage
        .list_request_logs(
            query.limit.unwrap_or(100),
            query.request_id.as_deref(),
            query.route_id.as_deref(),
        )
        .await?;
    Ok(Json(logs))
}

/// `GET /api/v1/audit-logs`.
pub async fn audit_logs(
    State(state): State<ApiControlState>,
    Query(query): Query<AuditLogQuery>,
) -> ControlResult<Json<Vec<AuditLog>>> {
    let logs = state
        .storage
        .list_audit_logs(query.limit.unwrap_or(100))
        .await?;
    Ok(Json(logs))
}
