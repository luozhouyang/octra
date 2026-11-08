//! `/admin/secrets/rotate` and `/admin/secrets/retire`.

use axum::Json;
use axum::extract::State;
use serde_json::{Value, json};

use super::AdminResult;
use crate::ApiControlState;

/// Rotates the master key and re-encrypts every stored secret. The control
/// bearer token is enforced by the daemon around the control router.
pub async fn rotate(State(state): State<ApiControlState>) -> AdminResult<Json<Value>> {
    let report = state.storage.rotate_secrets().await?;
    Ok(Json(json!({ "rotated": report.rotated })))
}

/// Retires master keys no longer referenced by any stored secret while
/// keeping the active key. The control bearer token is enforced by the daemon
/// around the control router.
pub async fn retire(State(state): State<ApiControlState>) -> AdminResult<Json<Value>> {
    let report = state.storage.retire_secrets().await?;
    Ok(Json(json!({
        "retired": report.retired,
        "remaining": report.remaining,
    })))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use octra_runtime::GatewayRuntime;
    use octra_storage::Storage;

    use super::*;

    #[tokio::test]
    async fn retire_reports_the_retired_and_remaining_counts() {
        let storage = Arc::new(Storage::connect_in_memory().await.unwrap());
        let runtime = Arc::new(GatewayRuntime::bootstrap(storage.clone()).await.unwrap());
        let state = ApiControlState::new(storage.clone(), runtime);

        storage.rotate_secrets().await.unwrap();
        let Json(value) = retire(State(state)).await.unwrap();
        assert_eq!(value["retired"], 1);
        assert_eq!(value["remaining"], 1);
    }

    #[tokio::test]
    async fn retire_is_idempotent() {
        let storage = Arc::new(Storage::connect_in_memory().await.unwrap());
        let runtime = Arc::new(GatewayRuntime::bootstrap(storage.clone()).await.unwrap());
        let state = ApiControlState::new(storage, runtime);

        let Json(first) = retire(State(state.clone())).await.unwrap();
        assert_eq!(first["retired"], 0);
        assert_eq!(first["remaining"], 1);
        let Json(second) = retire(State(state)).await.unwrap();
        assert_eq!(second, first);
    }
}
