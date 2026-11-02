//! List envelopes, cursor pagination, tag filters, and FK normalization shared
//! by Admin API resources.

use axum::Json;
use axum::extract::rejection::JsonRejection;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::error::{AdminError, AdminResult};

#[derive(Debug, Default, Deserialize)]
pub struct ListQuery {
    pub size: Option<usize>,
    pub offset: Option<String>,
    pub tags: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ListResponse {
    pub data: Vec<Value>,
    pub next: Option<String>,
    pub offset: Option<String>,
}

/// Applies the Kong tag filter, takes one cursor page, and builds the
/// `{data,next,offset}` envelope.
pub fn list_response(base: &str, mut items: Vec<Value>, query: &ListQuery) -> Json<ListResponse> {
    if let Some(raw) = &query.tags {
        items.retain(|item| matches_tags(item, raw));
    }

    let size = query.size.unwrap_or(100).clamp(1, 1000);
    let start = query
        .offset
        .as_deref()
        .and_then(decode_cursor)
        .and_then(|id| items.iter().position(|item| item["id"] == json!(id)))
        .map(|index| index + 1)
        .unwrap_or(0);
    let end = (start + size).min(items.len());
    let page: Vec<Value> = items[start..end].to_vec();
    let next = if end < items.len() {
        page.last()
            .and_then(|item| item["id"].as_str())
            .map(|id| format!("{base}?offset={}&size={size}", encode_cursor(id)))
    } else {
        None
    };

    Json(ListResponse {
        data: page,
        next,
        offset: query.offset.clone(),
    })
}

fn matches_tags(item: &Value, raw: &str) -> bool {
    let Some(tags) = item.get("tags").and_then(Value::as_array) else {
        return false;
    };
    let tags: Vec<&str> = tags.iter().filter_map(Value::as_str).collect();
    raw.split(',').all(|term| {
        term.split('/').any(|name| {
            let name = name.trim();
            !name.is_empty() && tags.contains(&name)
        })
    })
}

fn encode_cursor(id: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(id.as_bytes())
}

fn decode_cursor(cursor: &str) -> Option<String> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(cursor.as_bytes())
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
}

/// Accepts either a flat `target` id or a Kong-style `field: {"id": ...}`
/// (or `field: "id"`) and writes the result into `target`.
pub fn normalize_fk(body: &mut Value, field: &str, target: &str) {
    let Some(object) = body.as_object_mut() else {
        return;
    };
    let Some(raw) = object.remove(field) else {
        return;
    };
    let id = match raw {
        Value::Null => Value::Null,
        Value::String(id) => Value::String(id),
        Value::Object(map) => map
            .get("id")
            .cloned()
            .filter(|value| value.is_string())
            .unwrap_or(Value::Null),
        _ => Value::Null,
    };
    object.insert(target.to_string(), id);
}

/// Adds a Kong-style FK object to a serialized entity.
pub fn with_ref(value: &mut Value, field: &str, id: Option<&str>) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    object.insert(
        field.to_string(),
        id.map(|id| json!({ "id": id })).unwrap_or(Value::Null),
    );
}

pub fn to_value<T: Serialize>(value: &T) -> AdminResult<Value> {
    serde_json::to_value(value).map_err(|error| AdminError::internal(error.to_string()))
}

pub fn read_json_body(payload: Result<Json<Value>, JsonRejection>) -> AdminResult<Value> {
    payload
        .map(|Json(value)| value)
        .map_err(|rejection| AdminError::schema(rejection.body_text()))
}

pub fn deserialize<T: serde::de::DeserializeOwned>(body: Value) -> AdminResult<T> {
    serde_json::from_value(body).map_err(|error| AdminError::schema(error.to_string()))
}
