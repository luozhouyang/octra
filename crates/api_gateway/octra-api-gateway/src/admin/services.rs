//! `/admin/services` resources.

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use octra_api_core::{CreateService, UpdateService};

use crate::ApiControlState;

use super::error::{AdminError, AdminResult};
use super::list::{
    ListQuery, ListResponse, deserialize, list_page, list_response, page_values, read_json_body,
    set_id, to_value,
};
use super::reload_all;

pub async fn list(
    State(state): State<ApiControlState>,
    Query(query): Query<ListQuery>,
) -> AdminResult<Json<ListResponse>> {
    let page = state
        .storage
        .api
        .list_services_page(&list_page(&query))
        .await?;
    let (items, next) = page_values(page)?;
    Ok(list_response("/admin/services", items, next, &query))
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

pub async fn upsert(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
    payload: Result<Json<serde_json::Value>, JsonRejection>,
) -> AdminResult<Json<serde_json::Value>> {
    let mut body = read_json_body(payload)?;
    let service = if state.storage.api.get_service(&id).await?.is_some() {
        let input: UpdateService = deserialize(body)?;
        input.validate()?;
        state.storage.api.update_service(&id, &input).await?
    } else {
        set_id(&mut body, &id);
        let input: CreateService = deserialize(body)?;
        input.validate()?;
        state.storage.api.create_service(&input).await?
    };
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
