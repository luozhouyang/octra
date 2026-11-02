//! `/admin/services` resources.

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use octra_api_core::{CreateService, UpdateService};

use crate::ApiControlState;

use super::error::{AdminError, AdminResult};
use super::list::{ListQuery, ListResponse, deserialize, list_response, read_json_body, to_value};
use super::reload_all;

pub async fn list(
    State(state): State<ApiControlState>,
    Query(query): Query<ListQuery>,
) -> AdminResult<Json<ListResponse>> {
    let services = state.storage.api.list_services().await?;
    let items = services
        .iter()
        .map(to_value)
        .collect::<AdminResult<Vec<_>>>()?;
    Ok(list_response("/admin/services", items, &query))
}

pub async fn get(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
) -> AdminResult<Json<serde_json::Value>> {
    let service = state
        .storage
        .api
        .get_service(&id)
        .await?
        .ok_or_else(|| AdminError::not_found(format!("service '{id}' not found")))?;
    Ok(Json(to_value(&service)?))
}

pub async fn create(
    State(state): State<ApiControlState>,
    payload: Result<Json<serde_json::Value>, JsonRejection>,
) -> AdminResult<(StatusCode, Json<serde_json::Value>)> {
    let input: CreateService = deserialize(read_json_body(payload)?)?;
    input.validate()?;
    let service = state.storage.api.create_service(&input).await?;
    reload_all(&state).await?;
    Ok((StatusCode::CREATED, Json(to_value(&service)?)))
}

pub async fn update(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
    payload: Result<Json<serde_json::Value>, JsonRejection>,
) -> AdminResult<Json<serde_json::Value>> {
    let input: UpdateService = deserialize(read_json_body(payload)?)?;
    input.validate()?;
    let service = state.storage.api.update_service(&id, &input).await?;
    reload_all(&state).await?;
    Ok(Json(to_value(&service)?))
}

pub async fn delete(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
) -> AdminResult<StatusCode> {
    state.storage.api.delete_service(&id).await?;
    reload_all(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}
