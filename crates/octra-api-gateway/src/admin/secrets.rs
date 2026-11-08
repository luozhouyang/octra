//! `/admin/secrets/rotate`.

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
