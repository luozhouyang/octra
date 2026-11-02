//! `/admin/routes` resources.

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use octra_api_core::{CreateRoute, Route, UpdateRoute};
use serde_json::{Value, json};

use crate::ApiControlState;

use super::error::{AdminError, AdminResult};
use super::list::{
    ListQuery, ListResponse, deserialize, list_page, list_response, normalize_fk, read_json_body,
    set_id, to_value, with_ref,
};
use super::{reload_all, require_service};

pub async fn list(
    State(state): State<ApiControlState>,
    Query(query): Query<ListQuery>,
) -> AdminResult<Json<ListResponse>> {
    let page = state
        .storage
        .api
        .list_routes_page(None, &list_page(&query))
        .await?;
    let next = page.next;
    let items = page
        .items
        .iter()
        .map(route_view)
        .collect::<AdminResult<Vec<_>>>()?;
    Ok(list_response("/admin/routes", items, next, &query))
}

pub async fn list_for_service(
    State(state): State<ApiControlState>,
    Path(service_id): Path<String>,
    Query(query): Query<ListQuery>,
) -> AdminResult<Json<ListResponse>> {
    require_service(&state, &service_id).await?;
    let page = state
        .storage
        .api
        .list_routes_page(Some(&service_id), &list_page(&query))
        .await?;
    let next = page.next;
    let items = page
        .items
        .iter()
        .map(route_view)
        .collect::<AdminResult<Vec<_>>>()?;
    Ok(list_response(
        &format!("/admin/services/{service_id}/routes"),
        items,
        next,
        &query,
    ))
}

pub async fn get(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
) -> AdminResult<Json<Value>> {
    let route = state
        .storage
        .api
        .get_route(&id)
        .await?
        .ok_or_else(|| AdminError::not_found(format!("route '{id}' not found")))?;
    Ok(Json(route_view(&route)?))
}

pub async fn create(
    State(state): State<ApiControlState>,
    payload: Result<Json<Value>, JsonRejection>,
) -> AdminResult<(StatusCode, Json<Value>)> {
    let mut body = read_json_body(payload)?;
    normalize_fk(&mut body, "service", "service_id");
    let input: CreateRoute = deserialize(body)?;
    input.validate()?;
    let route = state.storage.api.create_route(&input).await?;
    reload_all(&state).await?;
    Ok((StatusCode::CREATED, Json(route_view(&route)?)))
}

pub async fn create_for_service(
    State(state): State<ApiControlState>,
    Path(service_id): Path<String>,
    payload: Result<Json<Value>, JsonRejection>,
) -> AdminResult<(StatusCode, Json<Value>)> {
    require_service(&state, &service_id).await?;
    let mut body = read_json_body(payload)?;
    normalize_fk(&mut body, "service", "service_id");
    if let Some(object) = body.as_object_mut() {
        object.insert("service_id".into(), json!(service_id));
    }
    let input: CreateRoute = deserialize(body)?;
    input.validate()?;
    let route = state.storage.api.create_route(&input).await?;
    reload_all(&state).await?;
    Ok((StatusCode::CREATED, Json(route_view(&route)?)))
}

pub async fn update(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
    payload: Result<Json<Value>, JsonRejection>,
) -> AdminResult<Json<Value>> {
    let mut body = read_json_body(payload)?;
    normalize_fk(&mut body, "service", "service_id");
    let input: UpdateRoute = deserialize(body)?;
    input.validate()?;
    let route = state.storage.api.update_route(&id, &input).await?;
    reload_all(&state).await?;
    Ok(Json(route_view(&route)?))
}

pub async fn upsert(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
    payload: Result<Json<Value>, JsonRejection>,
) -> AdminResult<Json<Value>> {
    let mut body = read_json_body(payload)?;
    normalize_fk(&mut body, "service", "service_id");
    let route = if state.storage.api.get_route(&id).await?.is_some() {
        let input: UpdateRoute = deserialize(body)?;
        input.validate()?;
        state.storage.api.update_route(&id, &input).await?
    } else {
        set_id(&mut body, &id);
        let input: CreateRoute = deserialize(body)?;
        input.validate()?;
        state.storage.api.create_route(&input).await?
    };
    reload_all(&state).await?;
    Ok(Json(route_view(&route)?))
}

pub async fn delete(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
) -> AdminResult<StatusCode> {
    state.storage.api.delete_route(&id).await?;
    reload_all(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}

fn route_view(route: &Route) -> AdminResult<Value> {
    let mut value = to_value(route)?;
    with_ref(&mut value, "service", route.service_id.as_deref());
    Ok(value)
}
