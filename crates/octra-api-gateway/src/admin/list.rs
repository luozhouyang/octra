//! List envelopes, cursor pagination, tag filters, and FK normalization shared
//! by Admin API resources.

use axum::Json;
use axum::extract::rejection::JsonRejection;
use base64::Engine as _;
use octra_storage::api::{ListPage, Page, PageCursor};
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

pub fn page_size(query: &ListQuery) -> usize {
    query.size.unwrap_or(100).clamp(1, 1000)
}

/// Converts the wire query into a storage page request. The offset cursor is
/// `base64url("<created_at>:<id>")`.
pub fn list_page(query: &ListQuery) -> ListPage {
    ListPage {
        limit: page_size(query),
        after: query.offset.as_deref().and_then(decode_cursor),
        tags: query
            .tags
            .as_deref()
            .map(|raw| {
                raw.split(',')
                    .map(|term| {
                        term.split('/')
                            .map(str::trim)
                            .filter(|name| !name.is_empty())
                            .map(str::to_string)
                            .collect::<Vec<_>>()
                    })
                    .filter(|term| !term.is_empty())
                    .collect()
            })
            .unwrap_or_default(),
    }
}

pub fn list_response(
    base: &str,
    items: Vec<Value>,
    next: Option<PageCursor>,
    query: &ListQuery,
) -> Json<ListResponse> {
    let size = page_size(query);
    let next = next.map(|cursor| {
        format!(
            "{base}?offset={}&size={size}",
            encode_cursor(&cursor.created_at, &cursor.id)
        )
    });
    Json(ListResponse {
        data: items,
        next,
        offset: query.offset.clone(),
    })
}

/// Maps a storage page into serialized entities plus the next cursor.
pub fn page_values<T: Serialize>(page: Page<T>) -> AdminResult<(Vec<Value>, Option<PageCursor>)> {
    let items = page
        .items
        .iter()
        .map(|item| {
            serde_json::to_value(item).map_err(|error| AdminError::internal(error.to_string()))
        })
        .collect::<AdminResult<Vec<_>>>()?;
    Ok((items, page.next))
}

fn encode_cursor(created_at: &i64, id: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!("{created_at}:{id}"))
}

fn decode_cursor(cursor: &str) -> Option<PageCursor> {
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(cursor.as_bytes())
        .ok()?;
    let decoded = String::from_utf8(bytes).ok()?;
    let (created_at, id) = decoded.split_once(':')?;
    Some(PageCursor {
        created_at: created_at.parse().ok()?,
        id: id.to_string(),
    })
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

/// Forces the path id into a create payload for PUT upserts.
pub fn set_id(body: &mut Value, id: &str) {
    if let Some(object) = body.as_object_mut() {
        object.insert("id".to_string(), json!(id));
    }
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
