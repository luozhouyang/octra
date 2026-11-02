//! `/admin/upstreams` and `/admin/targets` resources.

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use octra_api_core::{CreateTarget, CreateUpstream, Target, UpdateTarget, UpdateUpstream};
use serde_json::{Value, json};

use crate::ApiControlState;

use super::error::{AdminError, AdminResult};
use super::list::{
    ListQuery, ListResponse, deserialize, list_response, normalize_fk, read_json_body, to_value,
    with_ref,
};
use super::{reload_all, require_upstream};

pub async fn list(
    State(state): State<ApiControlState>,
    Query(query): Query<ListQuery>,
) -> AdminResult<Json<ListResponse>> {
    let upstreams = state.storage.api.list_upstreams().await?;
    let items = upstreams
        .iter()
        .map(to_value)
        .collect::<AdminResult<Vec<_>>>()?;
    Ok(list_response("/admin/upstreams", items, &query))
}

pub async fn get(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
) -> AdminResult<Json<Value>> {
    let upstream = state
        .storage
        .api
        .get_upstream(&id)
        .await?
        .ok_or_else(|| AdminError::not_found(format!("upstream '{id}' not found")))?;
    Ok(Json(to_value(&upstream)?))
}

pub async fn create(
    State(state): State<ApiControlState>,
    payload: Result<Json<Value>, JsonRejection>,
) -> AdminResult<(StatusCode, Json<Value>)> {
    let input: CreateUpstream = deserialize(read_json_body(payload)?)?;
    input.validate()?;
    let upstream = state.storage.api.create_upstream(&input).await?;
    reload_all(&state).await?;
    Ok((StatusCode::CREATED, Json(to_value(&upstream)?)))
}

pub async fn update(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
    payload: Result<Json<Value>, JsonRejection>,
) -> AdminResult<Json<Value>> {
    let input: UpdateUpstream = deserialize(read_json_body(payload)?)?;
    input.validate()?;
    let upstream = state.storage.api.update_upstream(&id, &input).await?;
    reload_all(&state).await?;
    Ok(Json(to_value(&upstream)?))
}

pub async fn delete(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
) -> AdminResult<StatusCode> {
    state.storage.api.delete_upstream(&id).await?;
    reload_all(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn list_all_targets(
    State(state): State<ApiControlState>,
    Query(query): Query<ListQuery>,
) -> AdminResult<Json<ListResponse>> {
    let targets = state.storage.api.list_targets(None).await?;
    let items = targets
        .iter()
        .map(target_view)
        .collect::<AdminResult<Vec<_>>>()?;
    Ok(list_response("/admin/targets", items, &query))
}

pub async fn list_targets(
    State(state): State<ApiControlState>,
    Path(upstream_id): Path<String>,
    Query(query): Query<ListQuery>,
) -> AdminResult<Json<ListResponse>> {
    require_upstream(&state, &upstream_id).await?;
    let targets = state.storage.api.list_targets(Some(&upstream_id)).await?;
    let items = targets
        .iter()
        .map(target_view)
        .collect::<AdminResult<Vec<_>>>()?;
    Ok(list_response(
        &format!("/admin/upstreams/{upstream_id}/targets"),
        items,
        &query,
    ))
}

pub async fn get_target(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
) -> AdminResult<Json<Value>> {
    let target = state
        .storage
        .api
        .get_target(&id)
        .await?
        .ok_or_else(|| AdminError::not_found(format!("target '{id}' not found")))?;
    Ok(Json(target_view(&target)?))
}

pub async fn create_any_target(
    State(state): State<ApiControlState>,
    payload: Result<Json<Value>, JsonRejection>,
) -> AdminResult<(StatusCode, Json<Value>)> {
    let mut body = read_json_body(payload)?;
    normalize_fk(&mut body, "upstream", "upstream_id");
    let input: CreateTarget = deserialize(body)?;
    input.validate()?;
    let target = state.storage.api.create_target(&input).await?;
    reload_all(&state).await?;
    Ok((StatusCode::CREATED, Json(target_view(&target)?)))
}

pub async fn create_target(
    State(state): State<ApiControlState>,
    Path(upstream_id): Path<String>,
    payload: Result<Json<Value>, JsonRejection>,
) -> AdminResult<(StatusCode, Json<Value>)> {
    require_upstream(&state, &upstream_id).await?;
    let mut body = read_json_body(payload)?;
    normalize_fk(&mut body, "upstream", "upstream_id");
    if let Some(object) = body.as_object_mut() {
        object.insert("upstream_id".into(), json!(upstream_id));
    }
    let input: CreateTarget = deserialize(body)?;
    input.validate()?;
    let target = state.storage.api.create_target(&input).await?;
    reload_all(&state).await?;
    Ok((StatusCode::CREATED, Json(target_view(&target)?)))
}

pub async fn update_target(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
    payload: Result<Json<Value>, JsonRejection>,
) -> AdminResult<Json<Value>> {
    let mut body = read_json_body(payload)?;
    normalize_fk(&mut body, "upstream", "upstream_id");
    let input: UpdateTarget = deserialize(body)?;
    input.validate()?;
    let target = state.storage.api.update_target(&id, &input).await?;
    reload_all(&state).await?;
    Ok(Json(target_view(&target)?))
}

pub async fn delete_target(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
) -> AdminResult<StatusCode> {
    state.storage.api.delete_target(&id).await?;
    reload_all(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}

fn target_view(target: &Target) -> AdminResult<Value> {
    let mut value = to_value(target)?;
    with_ref(&mut value, "upstream", Some(target.upstream_id.as_str()));
    Ok(value)
}
