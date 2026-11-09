//! `/admin/status`.

use axum::Json;
use axum::extract::State;
use serde_json::{Value, json};

use crate::ApiControlState;

pub async fn status(State(state): State<ApiControlState>) -> Json<Value> {
    let reachable = state.storage.ping().await.is_ok();
    Json(json!({
        "server": {
            "version": env!("CARGO_PKG_VERSION"),
        },
        "database": {
            "reachable": reachable,
        },
        "configuration": {
            "database": "sqlite",
            "plugins": state.plugins.manifests().len(),
        },
    }))
}
