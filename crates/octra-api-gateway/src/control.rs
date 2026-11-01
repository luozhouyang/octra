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
use serde::{Deserialize, Serialize};

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
        .with_state(state)
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
