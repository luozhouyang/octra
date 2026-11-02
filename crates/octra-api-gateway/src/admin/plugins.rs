//! `/admin/plugins` resources.

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use octra_api_core::{CreatePluginInstance, PluginInstance, UpdatePluginInstance};
use octra_api_plugin_types::ApiGatewayPluginFactory;
use serde_json::{Value, json};

use crate::ApiControlState;

use super::error::{AdminError, AdminResult};
use super::list::{
    ListQuery, ListResponse, deserialize, list_response, normalize_fk, read_json_body, to_value,
    with_ref,
};
use super::{
    reload_all, require_consumer, require_route, require_service, resolve_consumer_group_id,
};

pub async fn list(
    State(state): State<ApiControlState>,
    Query(query): Query<ListQuery>,
) -> AdminResult<Json<ListResponse>> {
    let plugins = state.storage.api.list_plugin_instances().await?;
    let items = plugins
        .iter()
        .map(plugin_view)
        .collect::<AdminResult<Vec<_>>>()?;
    Ok(list_response("/admin/plugins", items, &query))
}

pub async fn enabled(State(state): State<ApiControlState>) -> Json<Value> {
    let mut names: Vec<String> = state
        .plugins
        .registry()
        .manifests()
        .into_iter()
        .map(|manifest| manifest.name)
        .collect();
    names.sort();
    Json(json!({ "enabled_plugins": names }))
}

pub async fn get(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
) -> AdminResult<Json<Value>> {
    let plugin = state
        .storage
        .api
        .get_plugin_instance(&id)
        .await?
        .ok_or_else(|| AdminError::not_found(format!("plugin '{id}' not found")))?;
    Ok(Json(plugin_view(&plugin)?))
}

pub async fn list_for_service(
    State(state): State<ApiControlState>,
    Path(service_id): Path<String>,
    Query(query): Query<ListQuery>,
) -> AdminResult<Json<ListResponse>> {
    require_service(&state, &service_id).await?;
    let items = filtered(&state, |plugin| {
        plugin.service_id.as_deref() == Some(service_id.as_str())
    })
    .await?;
    Ok(list_response(
        &format!("/admin/services/{service_id}/plugins"),
        items,
        &query,
    ))
}

pub async fn list_for_route(
    State(state): State<ApiControlState>,
    Path(route_id): Path<String>,
    Query(query): Query<ListQuery>,
) -> AdminResult<Json<ListResponse>> {
    require_route(&state, &route_id).await?;
    let items = filtered(&state, |plugin| {
        plugin.route_id.as_deref() == Some(route_id.as_str())
    })
    .await?;
    Ok(list_response(
        &format!("/admin/routes/{route_id}/plugins"),
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
    let items = filtered(&state, |plugin| {
        plugin.consumer_id.as_deref() == Some(consumer_id.as_str())
    })
    .await?;
    Ok(list_response(
        &format!("/admin/consumers/{consumer_id}/plugins"),
        items,
        &query,
    ))
}

pub async fn list_for_consumer_group(
    State(state): State<ApiControlState>,
    Path(group_id): Path<String>,
    Query(query): Query<ListQuery>,
) -> AdminResult<Json<ListResponse>> {
    let group_id = resolve_consumer_group_id(&state, &group_id).await?;
    let items = filtered(&state, |plugin| {
        plugin.consumer_group_id.as_deref() == Some(group_id.as_str())
    })
    .await?;
    Ok(list_response(
        &format!("/admin/consumer_groups/{group_id}/plugins"),
        items,
        &query,
    ))
}

pub async fn create(
    State(state): State<ApiControlState>,
    payload: Result<Json<Value>, JsonRejection>,
) -> AdminResult<(StatusCode, Json<Value>)> {
    let body = normalized(read_json_body(payload)?);
    let plugin = create_instance(&state, body).await?;
    Ok((StatusCode::CREATED, Json(plugin_view(&plugin)?)))
}

pub async fn create_for_service(
    State(state): State<ApiControlState>,
    Path(service_id): Path<String>,
    payload: Result<Json<Value>, JsonRejection>,
) -> AdminResult<(StatusCode, Json<Value>)> {
    require_service(&state, &service_id).await?;
    let plugin = create_scoped(&state, "service_id", service_id, payload).await?;
    Ok((StatusCode::CREATED, Json(plugin_view(&plugin)?)))
}

pub async fn create_for_route(
    State(state): State<ApiControlState>,
    Path(route_id): Path<String>,
    payload: Result<Json<Value>, JsonRejection>,
) -> AdminResult<(StatusCode, Json<Value>)> {
    require_route(&state, &route_id).await?;
    let plugin = create_scoped(&state, "route_id", route_id, payload).await?;
    Ok((StatusCode::CREATED, Json(plugin_view(&plugin)?)))
}

pub async fn create_for_consumer(
    State(state): State<ApiControlState>,
    Path(consumer_id): Path<String>,
    payload: Result<Json<Value>, JsonRejection>,
) -> AdminResult<(StatusCode, Json<Value>)> {
    require_consumer(&state, &consumer_id).await?;
    let plugin = create_scoped(&state, "consumer_id", consumer_id, payload).await?;
    Ok((StatusCode::CREATED, Json(plugin_view(&plugin)?)))
}

pub async fn create_for_consumer_group(
    State(state): State<ApiControlState>,
    Path(group_id): Path<String>,
    payload: Result<Json<Value>, JsonRejection>,
) -> AdminResult<(StatusCode, Json<Value>)> {
    let group_id = resolve_consumer_group_id(&state, &group_id).await?;
    let plugin = create_scoped(&state, "consumer_group_id", group_id, payload).await?;
    Ok((StatusCode::CREATED, Json(plugin_view(&plugin)?)))
}

pub async fn update(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
    payload: Result<Json<Value>, JsonRejection>,
) -> AdminResult<Json<Value>> {
    let body = normalized(read_json_body(payload)?);
    let input: UpdatePluginInstance = deserialize(body)?;
    input.validate()?;
    let existing = state
        .storage
        .api
        .get_plugin_instance(&id)
        .await?
        .ok_or_else(|| AdminError::not_found(format!("plugin '{id}' not found")))?;

    let consumer_id = input
        .consumer_id
        .clone()
        .unwrap_or_else(|| existing.consumer_id.clone());
    let consumer_group_id = input
        .consumer_group_id
        .clone()
        .unwrap_or_else(|| existing.consumer_group_id.clone());
    let factory = factory(&state, &existing.name)?;
    if consumer_id.is_some() && consumer_group_id.is_some() {
        return Err(AdminError::schema(
            "plugin cannot be scoped to both consumer and consumer_group",
        ));
    }
    if factory.auth() && (consumer_id.is_some() || consumer_group_id.is_some()) {
        return Err(AdminError::schema(
            "authentication plugins cannot be scoped to consumers or consumer groups",
        ));
    }
    if let Some(config) = &input.config {
        validate_config(&state, &existing.name, config)?;
    }

    let plugin = state
        .storage
        .api
        .update_plugin_instance(&id, &input)
        .await?;
    reload_all(&state).await?;
    Ok(Json(plugin_view(&plugin)?))
}

pub async fn delete(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
) -> AdminResult<StatusCode> {
    state.storage.api.delete_plugin_instance(&id).await?;
    reload_all(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn create_scoped(
    state: &ApiControlState,
    field: &str,
    id: String,
    payload: Result<Json<Value>, JsonRejection>,
) -> AdminResult<PluginInstance> {
    let mut body = normalized(read_json_body(payload)?);
    if let Some(object) = body.as_object_mut() {
        object.insert(field.to_string(), json!(id));
    }
    create_instance(state, body).await
}

async fn create_instance(state: &ApiControlState, body: Value) -> AdminResult<PluginInstance> {
    let input: CreatePluginInstance = deserialize(body)?;
    input.validate()?;
    let factory = factory(state, &input.name)?;
    if factory.auth() && (input.consumer_id.is_some() || input.consumer_group_id.is_some()) {
        return Err(AdminError::schema(
            "authentication plugins cannot be scoped to consumers or consumer groups",
        ));
    }
    let config = input.config.clone().unwrap_or_else(|| json!({}));
    validate_config(state, &input.name, &config)?;
    let plugin = state.storage.api.create_plugin_instance(&input).await?;
    reload_all(state).await?;
    Ok(plugin)
}

async fn filtered(
    state: &ApiControlState,
    keep: impl Fn(&PluginInstance) -> bool,
) -> AdminResult<Vec<Value>> {
    let plugins = state.storage.api.list_plugin_instances().await?;
    plugins
        .iter()
        .filter(|plugin| keep(plugin))
        .map(plugin_view)
        .collect()
}

fn normalized(mut body: Value) -> Value {
    normalize_fk(&mut body, "service", "service_id");
    normalize_fk(&mut body, "route", "route_id");
    normalize_fk(&mut body, "consumer", "consumer_id");
    normalize_fk(&mut body, "consumer_group", "consumer_group_id");
    body
}

fn factory(
    state: &ApiControlState,
    name: &str,
) -> AdminResult<std::sync::Arc<dyn ApiGatewayPluginFactory>> {
    state
        .plugins
        .registry()
        .factory(name)
        .ok_or_else(|| AdminError::not_found(format!("plugin '{name}' not found")))
}

fn validate_config(state: &ApiControlState, name: &str, config: &Value) -> AdminResult<()> {
    state
        .plugins
        .registry()
        .validate(name, config)
        .map_err(|error| AdminError::schema(error.message))
}

fn plugin_view(plugin: &PluginInstance) -> AdminResult<Value> {
    let mut value = to_value(plugin)?;
    with_ref(&mut value, "service", plugin.service_id.as_deref());
    with_ref(&mut value, "route", plugin.route_id.as_deref());
    with_ref(&mut value, "consumer", plugin.consumer_id.as_deref());
    with_ref(
        &mut value,
        "consumer_group",
        plugin.consumer_group_id.as_deref(),
    );
    Ok(value)
}
