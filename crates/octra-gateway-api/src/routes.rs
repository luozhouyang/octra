use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::AppState;
use crate::error::ApiError;

pub async fn health(State(state): State<AppState>) -> Response {
    let storage_ok = state.storage.ping().await.is_ok();
    let snapshot = state.runtime.snapshot().await;

    let status = if storage_ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    let body = json!({
        "status": if storage_ok { "ok" } else { "degraded" },
        "version": octra_core::VERSION,
        "uptime_secs": state.started_at.elapsed().as_secs(),
        "storage": if storage_ok { "ok" } else { "error" },
        "snapshot": snapshot.info,
    });

    (status, Json(body)).into_response()
}

pub async fn get_config(State(state): State<AppState>) -> Json<octra_config::OctraConfig> {
    Json((*state.config).clone())
}

pub async fn reload(
    State(state): State<AppState>,
) -> Result<Json<octra_core::SnapshotInfo>, ApiError> {
    let info = state.runtime.reload().await.map_err(ApiError::internal)?;

    tracing::info!(
        services = info.counts.services,
        routes = info.counts.routes,
        "runtime snapshot reloaded"
    );

    Ok(Json(info))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Instant;

    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::*;
    use crate::router;

    async fn test_state() -> AppState {
        let storage = Arc::new(octra_storage::Storage::connect_in_memory().await.unwrap());
        let runtime = Arc::new(
            octra_gateway_runtime::GatewayRuntime::bootstrap(storage.clone())
                .await
                .unwrap(),
        );

        AppState {
            runtime,
            storage,
            config: Arc::new(octra_config::OctraConfig::default()),
            token: Arc::new("test-token".to_string()),
            started_at: Instant::now(),
        }
    }

    #[tokio::test]
    async fn rejects_requests_without_token() {
        let app = router(test_state().await);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/system/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn health_reports_ok_with_token() {
        let app = router(test_state().await);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/system/health")
                    .header(header::AUTHORIZATION, "Bearer test-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["status"], "ok");
        assert_eq!(value["storage"], "ok");
    }
}
