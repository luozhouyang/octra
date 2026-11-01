//! Control-plane audit middleware.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::Method;
use axum::middleware::Next;
use axum::response::Response;
use octra_storage::Storage;

#[derive(Clone)]
pub struct AuditState {
    pub storage: Arc<Storage>,
}

/// Records every successful non-read control request as an audit row.
pub async fn record_control_audit(
    State(state): State<AuditState>,
    request: Request,
    next: Next,
) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let response = next.run(request).await;

    if matches!(method, Method::GET | Method::HEAD | Method::OPTIONS)
        || !response.status().is_success()
    {
        return response;
    }

    let storage = state.storage.clone();
    let action = method.as_str().to_ascii_lowercase();
    let status = response.status().as_u16();
    let detail = serde_json::json!({ "method": method.as_str(), "path": path, "status": status });
    tokio::spawn(async move {
        if let Err(error) = storage
            .insert_audit_log(
                Some("admin"),
                &format!("control.{action}"),
                Some(&path),
                &detail,
            )
            .await
        {
            tracing::warn!(%error, "cannot persist control audit log");
        }
    });

    response
}
