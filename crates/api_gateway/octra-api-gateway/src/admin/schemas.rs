//! `/admin/schemas/plugins` endpoints.

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::ApiControlState;

use super::error::{AdminError, AdminResult};
use super::list::{deserialize, read_json_body};

pub async fn list(State(state): State<ApiControlState>) -> Json<Value> {
    let mut schemas = serde_json::Map::new();
    for manifest in state.plugins.manifests() {
        schemas.insert(
            manifest.name,
            manifest.config_schema.unwrap_or_else(|| json!({})),
        );
    }
    Json(Value::Object(schemas))
}

pub async fn get(
    State(state): State<ApiControlState>,
    Path(name): Path<String>,
) -> AdminResult<Json<Value>> {
    let factory = state
        .plugins
        .factory(&name)
        .ok_or_else(|| AdminError::not_found(format!("plugin '{name}' not found")))?;
    let schema = factory
        .manifest()
        .config_schema
        .unwrap_or_else(|| json!({}));
    Ok(Json(schema))
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ValidateRequest {
    name: String,
    config: Value,
}

pub async fn validate(
    State(state): State<ApiControlState>,
    payload: Result<Json<Value>, JsonRejection>,
) -> AdminResult<Json<Value>> {
    let input: ValidateRequest = deserialize(read_json_body(payload)?)?;
    if input.name.trim().is_empty() {
        return Err(AdminError::schema("name is required"));
    }
    if state.plugins.factory(&input.name).is_none() {
        return Err(AdminError::not_found(format!(
            "plugin '{}' not found",
            input.name
        )));
    }
    match state.plugins.validate(&input.name, &input.config) {
        Ok(()) => Ok(Json(json!({ "valid": true, "errors": [] }))),
        Err(error) => Ok(Json(json!({
            "valid": false,
            "errors": [error.message],
        }))),
    }
}
