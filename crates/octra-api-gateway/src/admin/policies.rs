//! `/admin/policies` resources for consumer LLM access policies.
//!
//! Policies restrict which models and providers a consumer may reach. Every
//! mutation refreshes the API runtime and notifies the LLM identity/policy
//! snapshot so both runtimes observe the change immediately.

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use octra_api_core::{CreatePolicy, Policy, UpdatePolicy};
use serde_json::Value;

use crate::ApiControlState;

use super::error::{AdminError, AdminResult};
use super::list::{ListResponse, deserialize, read_json_body, set_id, to_value};
use super::reload_all;

pub async fn list(State(state): State<ApiControlState>) -> AdminResult<Json<ListResponse>> {
    let policies = state.storage.api.list_policies().await?;
    let items = policies
        .iter()
        .map(to_value)
        .collect::<AdminResult<Vec<_>>>()?;
    Ok(Json(ListResponse {
        data: items,
        next: None,
        offset: None,
    }))
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
    let mut input: CreatePolicy = deserialize(read_json_body(payload)?)?;
    validate_create(&input)?;
    if effective_scope(input.scope.as_deref(), input.consumer_id.as_deref()) == "global" {
        input.consumer_id = None;
    }
    let policy = state.storage.api.create_policy(&input).await?;
    reload_all(&state).await?;
    state.notify_reload_hook();
    Ok((StatusCode::CREATED, Json(to_value(&policy)?)))
}

pub async fn update(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
    payload: Result<Json<Value>, JsonRejection>,
) -> AdminResult<Json<Value>> {
    let existing = resolve(&state, &id).await?;
    let body = read_json_body(payload)?;
    let clear_consumer = body.get("consumer_id").is_some_and(Value::is_null);
    let mut input: UpdatePolicy = deserialize(body)?;
    normalize_update_scope(&mut input);
    validate_update(&input, &existing)?;
    let desired_scope = input.scope.as_deref().unwrap_or(&existing.scope);
    if clear_consumer && desired_scope == "consumer" {
        return Err(AdminError::schema(
            "consumer scope requires a non-empty consumer_id",
        ));
    }
    let clear = clear_consumer || desired_scope == "global";
    let mut policy = state
        .storage
        .api
        .update_policy(&existing.id, &input)
        .await?;
    if clear {
        state
            .storage
            .api
            .clear_policy_consumer(&existing.id)
            .await?;
        policy = resolve(&state, &existing.id).await?;
    }
    reload_all(&state).await?;
    state.notify_reload_hook();
    Ok(Json(to_value(&policy)?))
}

pub async fn upsert(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
    payload: Result<Json<Value>, JsonRejection>,
) -> AdminResult<Json<Value>> {
    let mut body = read_json_body(payload)?;
    let clear_consumer = body.get("consumer_id").is_some_and(Value::is_null);
    let policy = match state.storage.api.get_policy(&id).await? {
        Some(existing) => {
            let mut input: UpdatePolicy = deserialize(body)?;
            normalize_update_scope(&mut input);
            validate_update(&input, &existing)?;
            let desired_scope = input.scope.as_deref().unwrap_or(&existing.scope);
            if clear_consumer && desired_scope == "consumer" {
                return Err(AdminError::schema(
                    "consumer scope requires a non-empty consumer_id",
                ));
            }
            let clear = clear_consumer || desired_scope == "global";
            let mut policy = state
                .storage
                .api
                .update_policy(&existing.id, &input)
                .await?;
            if clear {
                state
                    .storage
                    .api
                    .clear_policy_consumer(&existing.id)
                    .await?;
                policy = resolve(&state, &existing.id).await?;
            }
            policy
        }
        None => {
            set_id(&mut body, &id);
            let mut input: CreatePolicy = deserialize(body)?;
            validate_create(&input)?;
            if effective_scope(input.scope.as_deref(), input.consumer_id.as_deref()) == "global" {
                input.consumer_id = None;
            }
            state.storage.api.create_policy(&input).await?
        }
    };
    reload_all(&state).await?;
    state.notify_reload_hook();
    Ok(Json(to_value(&policy)?))
}

pub async fn delete(
    State(state): State<ApiControlState>,
    Path(id): Path<String>,
) -> AdminResult<StatusCode> {
    state.storage.api.delete_policy(&id).await?;
    reload_all(&state).await?;
    state.notify_reload_hook();
    Ok(StatusCode::NO_CONTENT)
}

async fn resolve(state: &ApiControlState, id: &str) -> AdminResult<Policy> {
    state
        .storage
        .api
        .get_policy(id)
        .await?
        .ok_or_else(|| AdminError::not_found(format!("policy '{id}' not found")))
}

/// If a consumer id is supplied without an explicit scope, the policy becomes
/// consumer-scoped. This keeps the stored scope in sync with the runtime,
/// which scopes policies by the presence of `consumer_id`.
fn normalize_update_scope(input: &mut UpdatePolicy) {
    if input.scope.is_none() && input.consumer_id.is_some() {
        input.scope = Some("consumer".to_string());
    }
}

fn validate_create(input: &CreatePolicy) -> AdminResult<()> {
    validate_name(&input.name)?;
    if let Some(scope) = &input.scope {
        validate_scope(scope)?;
    }
    validate_document(input.document.as_ref())?;
    if effective_scope(input.scope.as_deref(), input.consumer_id.as_deref()) == "consumer"
        && input
            .consumer_id
            .as_deref()
            .is_none_or(|id| id.trim().is_empty())
    {
        return Err(AdminError::schema(
            "consumer scope requires a non-empty consumer_id",
        ));
    }
    Ok(())
}

fn validate_update(input: &UpdatePolicy, existing: &Policy) -> AdminResult<()> {
    if let Some(name) = &input.name {
        validate_name(name)?;
    }
    if let Some(scope) = &input.scope {
        validate_scope(scope)?;
    }
    validate_document(input.document.as_ref())?;
    if input.scope.as_deref().unwrap_or(&existing.scope) == "consumer" {
        let consumer_id = input
            .consumer_id
            .as_deref()
            .or(existing.consumer_id.as_deref());
        if consumer_id.is_none_or(|id| id.trim().is_empty()) {
            return Err(AdminError::schema(
                "consumer scope requires a non-empty consumer_id",
            ));
        }
    }
    Ok(())
}

fn validate_name(name: &str) -> AdminResult<()> {
    if name.trim().is_empty() {
        return Err(AdminError::schema("name must not be empty"));
    }
    Ok(())
}

fn validate_scope(scope: &str) -> AdminResult<()> {
    match scope {
        "global" | "consumer" => Ok(()),
        _ => Err(AdminError::schema("scope must be 'global' or 'consumer'")),
    }
}

fn validate_document(document: Option<&Value>) -> AdminResult<()> {
    if let Some(document) = document
        && !document.is_object()
    {
        return Err(AdminError::schema("document must be a JSON object"));
    }
    Ok(())
}

/// Mirrors the storage default: an explicit scope wins, otherwise a
/// `consumer_id` selects consumer scope and its absence selects global scope.
fn effective_scope<'a>(scope: Option<&'a str>, consumer_id: Option<&str>) -> &'a str {
    scope.unwrap_or(if consumer_id.is_some() {
        "consumer"
    } else {
        "global"
    })
}
