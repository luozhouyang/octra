//! Operational endpoints under `/admin`: metrics, logs, audit and reload.

use axum::Json;
use axum::extract::{Query, State};
use axum::response::IntoResponse;
use octra_core::SnapshotInfo;
use octra_storage::{AuditLog, RequestLog};
use serde::Deserialize;

use crate::ApiControlState;

use super::error::{AdminError, AdminResult};

pub async fn metrics(State(state): State<ApiControlState>) -> Json<crate::MetricsSnapshot> {
    Json(state.metrics.snapshot())
}

pub async fn prometheus_metrics(State(state): State<ApiControlState>) -> impl IntoResponse {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        state.metrics.prometheus(),
    )
}

#[derive(Debug, Default, Deserialize)]
pub struct LogQuery {
    pub limit: Option<u32>,
    pub request_id: Option<String>,
    pub route_id: Option<String>,
}

pub async fn logs(
    State(state): State<ApiControlState>,
    Query(query): Query<LogQuery>,
) -> AdminResult<Json<Vec<RequestLog>>> {
    Ok(Json(
        state
            .storage
            .logs
            .list_request_logs(
                query.limit.unwrap_or(100),
                query.request_id.as_deref(),
                query.route_id.as_deref(),
            )
            .await?,
    ))
}

#[derive(Debug, Default, Deserialize)]
pub struct AuditQuery {
    pub limit: Option<u32>,
}

pub async fn audit_logs(
    State(state): State<ApiControlState>,
    Query(query): Query<AuditQuery>,
) -> AdminResult<Json<Vec<AuditLog>>> {
    Ok(Json(
        state
            .storage
            .logs
            .list_audit_logs(query.limit.unwrap_or(100))
            .await?,
    ))
}

pub async fn reload(State(state): State<ApiControlState>) -> AdminResult<Json<SnapshotInfo>> {
    let info = state.runtime.reload().await.map_err(AdminError::from)?;
    let plugins = state
        .storage
        .api
        .list_plugin_instances()
        .await
        .map_err(AdminError::from)?;
    state.plugins.reload(plugins).map_err(AdminError::from)?;
    Ok(Json(info))
}
