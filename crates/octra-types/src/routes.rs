use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
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

/// Proves that the process listening on the control port knows the control
/// token, without asking the caller to send the token first.
///
/// The desktop host sends a random nonce; the daemon answers with an HMAC over
/// the nonce. A port squatter cannot produce the proof, so the host never
/// sends the bearer token to it.
pub async fn identity(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let nonce = headers
        .get("x-octra-nonce")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|nonce| !nonce.is_empty() && nonce.len() <= 128);
    let Some(nonce) = nonce else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "missing or invalid x-octra-nonce"})),
        )
            .into_response();
    };

    let proof = {
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        let mut mac = Hmac::<Sha256>::new_from_slice(state.token.as_bytes())
            .expect("HMAC accepts keys of any length");
        mac.update(b"octra-control-identity-v1:");
        mac.update(nonce.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    };

    (
        StatusCode::OK,
        Json(json!({
            "name": "octra",
            "version": octra_core::VERSION,
            "nonce": nonce,
            "proof": proof,
        })),
    )
        .into_response()
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
            octra_runtime::GatewayRuntime::bootstrap(storage.clone())
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

    fn proof(token: &str, nonce: &str) -> String {
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        let mut mac = Hmac::<Sha256>::new_from_slice(token.as_bytes()).expect("HMAC key");
        mac.update(b"octra-control-identity-v1:");
        mac.update(nonce.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    #[tokio::test]
    async fn identity_is_public_and_proves_the_token() {
        let app = router(test_state().await);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/system/identity")
                    .header("x-octra-nonce", "nonce-1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["nonce"], "nonce-1");
        assert_eq!(value["proof"], proof("test-token", "nonce-1"));
    }

    #[tokio::test]
    async fn identity_requires_a_nonce() {
        let app = router(test_state().await);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/system/identity")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}
