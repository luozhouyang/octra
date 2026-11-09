//! Control-plane API for the event outbox and plugin jobs.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use octra_api_plugin_types::validate_topic_pattern;
use octra_storage::events::EventStats;
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
        .route("/admin/events/stats", get(event_stats))
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

async fn event_stats(State(state): State<EventControlState>) -> ApiResult<Json<EventStats>> {
    let stats = state
        .bus
        .storage()
        .event_stats(Utc::now().timestamp())
        .await
        .map_err(internal)?;
    Ok(Json(stats))
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use octra_api_plugin_types::{EventSubscriber, PluginServiceSource, ScheduledJob};
    use octra_storage::{NewEvent, Storage};
    use serde_json::Value;
    use tower::ServiceExt;

    use super::*;

    struct EmptySource;

    impl PluginServiceSource for EmptySource {
        fn subscribers(&self) -> Vec<Arc<dyn EventSubscriber>> {
            Vec::new()
        }

        fn jobs(&self) -> Vec<Arc<dyn ScheduledJob>> {
            Vec::new()
        }
    }

    #[tokio::test]
    async fn stats_route_is_not_captured_by_the_event_id_route() {
        let storage = Arc::new(Storage::connect_in_memory().await.unwrap());
        let event = storage
            .insert_event(&NewEvent {
                topic: "gateway.request".to_string(),
                source: "test".to_string(),
                payload_json: "{}".to_string(),
                max_attempts: 5,
            })
            .await
            .unwrap();
        storage
            .ensure_event_delivery(
                &event.id,
                "sub-1",
                "plugin",
                "instance",
                5,
                event.created_at,
            )
            .await
            .unwrap();
        storage
            .mark_event_delivery_retry(&event.id, "sub-1", event.created_at + 30, "boom")
            .await
            .unwrap();

        let bus = Arc::new(EventBus::new(storage.clone(), Arc::new(EmptySource)));
        let app = control_router(EventControlState::new(bus));

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/admin/events/stats")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let stats: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(stats["pending_events"], 1);
        assert_eq!(stats["delivered_events"], 0);
        assert_eq!(stats["failed_events"], 0);
        assert_eq!(stats["pending_deliveries"], 1);
        assert_eq!(stats["failed_deliveries"], 0);
        assert_eq!(stats["retrying_deliveries"], 1);
        assert_eq!(stats["total_attempts"], 1);
        assert!(stats["oldest_pending_age_secs"].as_i64().unwrap() >= 0);
    }
}
