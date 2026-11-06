//! Control-plane API for the event outbox and plugin jobs.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use octra_api_plugin_types::validate_topic_pattern;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::EventBus;

#[derive(Clone)]
pub struct EventControlState {
    bus: Arc<EventBus>,
}

impl EventControlState {
    pub fn new(bus: Arc<EventBus>) -> Self {
        Self { bus }
    }
}

pub fn control_router(state: EventControlState) -> Router {
    Router::new()
        .route("/admin/events", get(list_events).post(create_event))
        .route("/admin/events/{id}", get(get_event))
        .route("/admin/jobs", get(list_jobs))
        .route("/admin/jobs/{id}/run", post(run_job))
        .with_state(state)
}

type ApiResult<T> = Result<T, (StatusCode, Json<Value>)>;

fn internal(error: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": error.to_string() })),
    )
}

fn bad_request(error: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": error.to_string() })),
    )
}

fn not_found(message: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": message.to_string() })),
    )
}

#[derive(Debug, Deserialize)]
struct EventQuery {
    #[serde(default)]
    limit: Option<u32>,
    #[serde(default)]
    status: Option<String>,
}

async fn list_events(
    State(state): State<EventControlState>,
    Query(query): Query<EventQuery>,
) -> ApiResult<Json<Vec<Value>>> {
    let events = state
        .bus
        .storage()
        .list_events(query.limit.unwrap_or(100), query.status.as_deref())
        .await
        .map_err(internal)?;
    Ok(Json(events.iter().map(event_view).collect()))
}

async fn get_event(
    State(state): State<EventControlState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let event = state
        .bus
        .storage()
        .get_event(&id)
        .await
        .map_err(internal)?
        .ok_or_else(|| not_found(format!("event '{id}' not found")))?;
    let deliveries = state
        .bus
        .storage()
        .list_event_deliveries(&id)
        .await
        .map_err(internal)?;
    Ok(Json(json!({
        "event": event_view(&event),
        "deliveries": deliveries,
    })))
}

#[derive(Debug, Deserialize)]
struct CreateEvent {
    topic: String,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    payload: Value,
}

async fn create_event(
    State(state): State<EventControlState>,
    Json(body): Json<CreateEvent>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    validate_topic_pattern(&body.topic).map_err(bad_request)?;
    if body.topic.contains('*') {
        return Err(bad_request("cannot publish to a wildcard topic"));
    }
    let event = state
        .bus
        .publish_now(
            &body.topic,
            body.source.as_deref().unwrap_or("control"),
            body.payload,
        )
        .await
        .map_err(internal)?;
    Ok((StatusCode::ACCEPTED, Json(event_view(&event))))
}

async fn list_jobs(State(state): State<EventControlState>) -> ApiResult<Json<Value>> {
    let jobs = state.bus.storage().list_jobs().await.map_err(internal)?;
    Ok(Json(json!({ "data": jobs })))
}

async fn run_job(
    State(state): State<EventControlState>,
    Path(id): Path<String>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let job = state
        .bus
        .storage()
        .get_job(&id)
        .await
        .map_err(internal)?
        .ok_or_else(|| not_found(format!("job '{id}' not found")))?;
    if !job.enabled {
        return Err(not_found(format!("job '{id}' is disabled")));
    }
    state.bus.trigger_job(&id).await.map_err(internal)?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "triggered": true, "id": id })),
    ))
}

fn event_view(event: &octra_storage::EventRecord) -> Value {
    let payload = serde_json::from_str(&event.payload_json).unwrap_or(Value::Null);
    json!({
        "id": event.id,
        "topic": event.topic,
        "source": event.source,
        "status": event.status,
        "attempts": event.attempts,
        "max_attempts": event.max_attempts,
        "next_attempt_at": event.next_attempt_at,
        "last_error": event.last_error,
        "created_at": event.created_at,
        "completed_at": event.completed_at,
        "payload": payload,
    })
}
