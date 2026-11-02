//! `/admin/consumers` resources.

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use octra_api_core::{CreateConsumer, UpdateConsumer};
use serde_json::Value;

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
        .list_consumers_page(&list_page(&query))
        .await?;
    let (items, next) = page_values(page)?;
    Ok(list_response("/admin/consumers", items, next, &query))
}

pub async fn get(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
) -> AdminResult<Json<Value>> {
    let consumer = state
        .storage
        .api
        .get_consumer(&id)
        .await?
        .ok_or_else(|| AdminError::not_found(format!("consumer '{id}' not found")))?;
    Ok(Json(to_value(&consumer)?))
}

pub async fn create(
    State(state): State<ApiControlState>,
    payload: Result<Json<Value>, JsonRejection>,
) -> AdminResult<(StatusCode, Json<Value>)> {
    let input: CreateConsumer = deserialize(read_json_body(payload)?)?;
    input.validate()?;
    let consumer = state.storage.api.create_consumer(&input).await?;
    reload_all(&state).await?;
    Ok((StatusCode::CREATED, Json(to_value(&consumer)?)))
}

pub async fn update(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
    payload: Result<Json<Value>, JsonRejection>,
) -> AdminResult<Json<Value>> {
    let input: UpdateConsumer = deserialize(read_json_body(payload)?)?;
    input.validate()?;
    let consumer = state.storage.api.update_consumer(&id, &input).await?;
    reload_all(&state).await?;
    Ok(Json(to_value(&consumer)?))
}

pub async fn upsert(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
    payload: Result<Json<Value>, JsonRejection>,
) -> AdminResult<Json<Value>> {
    let mut body = read_json_body(payload)?;
    let consumer = if state.storage.api.get_consumer(&id).await?.is_some() {
        let input: UpdateConsumer = deserialize(body)?;
        input.validate()?;
        state.storage.api.update_consumer(&id, &input).await?
    } else {
        set_id(&mut body, &id);
        let input: CreateConsumer = deserialize(body)?;
        input.validate()?;
        state.storage.api.create_consumer(&input).await?
    };
    reload_all(&state).await?;
    Ok(Json(to_value(&consumer)?))
}

pub async fn delete(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
) -> AdminResult<StatusCode> {
    state.storage.api.delete_consumer(&id).await?;
    reload_all(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}
