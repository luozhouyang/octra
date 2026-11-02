//! `/admin/consumer_groups` resources and membership endpoints.

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use octra_api_core::{ConsumerGroup, CreateConsumerGroup, UpdateConsumerGroup};
use serde_json::Value;

use crate::ApiControlState;

use super::error::{AdminError, AdminResult};
use super::list::{
    ListQuery, ListResponse, deserialize, list_response, normalize_fk, read_json_body, to_value,
};
use super::{reload_all, require_consumer};

pub async fn list(
    State(state): State<ApiControlState>,
    Query(query): Query<ListQuery>,
) -> AdminResult<Json<ListResponse>> {
    let groups = state.storage.api.list_consumer_groups().await?;
    let items = groups
        .iter()
        .map(to_value)
        .collect::<AdminResult<Vec<_>>>()?;
    Ok(list_response("/admin/consumer_groups", items, &query))
}

pub async fn get(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
) -> AdminResult<Json<Value>> {
    Ok(Json(to_value(&resolve(&state, &id).await?)?))
}

pub async fn create(
    State(state): State<ApiControlState>,
    payload: Result<Json<Value>, JsonRejection>,
) -> AdminResult<(StatusCode, Json<Value>)> {
    let input: CreateConsumerGroup = deserialize(read_json_body(payload)?)?;
    input.validate()?;
    let group = state.storage.api.create_consumer_group(&input).await?;
    reload_all(&state).await?;
    Ok((StatusCode::CREATED, Json(to_value(&group)?)))
}

pub async fn update(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
    payload: Result<Json<Value>, JsonRejection>,
) -> AdminResult<Json<Value>> {
    let group = resolve(&state, &id).await?;
    let input: UpdateConsumerGroup = deserialize(read_json_body(payload)?)?;
    input.validate()?;
    let group = state
        .storage
        .api
        .update_consumer_group(&group.id, &input)
        .await?;
    reload_all(&state).await?;
    Ok(Json(to_value(&group)?))
}

pub async fn delete(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
) -> AdminResult<StatusCode> {
    let group = resolve(&state, &id).await?;
    state.storage.api.delete_consumer_group(&group.id).await?;
    reload_all(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn list_members(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
    Query(query): Query<ListQuery>,
) -> AdminResult<Json<ListResponse>> {
    let group = resolve(&state, &id).await?;
    let consumers = state
        .storage
        .api
        .list_consumer_group_members(&group.id)
        .await?;
    let items = consumers
        .iter()
        .map(to_value)
        .collect::<AdminResult<Vec<_>>>()?;
    Ok(list_response(
        &format!("/admin/consumer_groups/{}/consumers", group.id),
        items,
        &query,
    ))
}

pub async fn list_for_consumer(
    State(state): State<ApiControlState>,
    Path(consumer_id): Path<String>,
    Query(query): Query<ListQuery>,
) -> AdminResult<Json<ListResponse>> {
    require_consumer(&state, &consumer_id).await?;
    let groups = state
        .storage
        .api
        .list_consumer_groups_for_consumer(&consumer_id)
        .await?;
    let items = groups
        .iter()
        .map(to_value)
        .collect::<AdminResult<Vec<_>>>()?;
    Ok(list_response(
        &format!("/admin/consumers/{consumer_id}/consumer_groups"),
        items,
        &query,
    ))
}

pub async fn add_member(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
    payload: Result<Json<Value>, JsonRejection>,
) -> AdminResult<(StatusCode, Json<Value>)> {
    let group = resolve(&state, &id).await?;
    let mut body = read_json_body(payload)?;
    normalize_fk(&mut body, "consumer", "consumer_id");
    let consumer_id = body
        .get("consumer_id")
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| AdminError::schema("consumer_id is required"))?
        .to_string();
    let member = state
        .storage
        .api
        .add_consumer_group_member(&group.id, &consumer_id)
        .await?;
    reload_all(&state).await?;
    Ok((StatusCode::CREATED, Json(to_value(&member)?)))
}

pub async fn remove_member(
    State(state): State<ApiControlState>,
    Path((id, consumer_id)): Path<(String, String)>,
) -> AdminResult<StatusCode> {
    let group = resolve(&state, &id).await?;
    state
        .storage
        .api
        .remove_consumer_group_member(&group.id, &consumer_id)
        .await?;
    reload_all(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn resolve(state: &ApiControlState, id_or_name: &str) -> AdminResult<ConsumerGroup> {
    if let Some(group) = state.storage.api.get_consumer_group(id_or_name).await? {
        return Ok(group);
    }
    state
        .storage
        .api
        .get_consumer_group_by_name(id_or_name)
        .await?
        .ok_or_else(|| AdminError::not_found(format!("consumer group '{id_or_name}' not found")))
}
