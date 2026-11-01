//! Authenticated local control plane for Octra.

pub mod auth;
pub mod error;
pub mod routes;
pub mod state;

pub use state::AppState;

use axum::Router;
use axum::http::{HeaderValue, Method, header};
use axum::routing::{get, post};
use tower_http::cors::{AllowOrigin, CorsLayer};

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/v1/system/health", get(routes::health))
        .route("/api/v1/system/config", get(routes::get_config))
        .route("/api/v1/system/reload", post(routes::reload))
        .route("/api/v1/services", get(routes::list_services))
        .route("/api/v1/routes", get(routes::list_routes))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::require_bearer,
        ))
        .layer(cors_layer())
        .with_state(state)
}

/// The control plane stays loopback-only; CORS merely lets a local browser
/// development build talk to it. Requests still require the bearer token.
fn cors_layer() -> CorsLayer {
    CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(|origin: &HeaderValue, _| {
            origin.to_str().is_ok_and(is_loopback_origin)
        }))
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE])
}

fn is_loopback_origin(origin: &str) -> bool {
    const LOOPBACK: [&str; 4] = [
        "http://127.0.0.1",
        "http://localhost",
        "https://127.0.0.1",
        "https://localhost",
    ];

    LOOPBACK.iter().any(|prefix| {
        origin
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(':'))
    })
}

#[cfg(test)]
mod tests {
    use super::is_loopback_origin;

    #[test]
    fn cors_allows_only_local_origins() {
        assert!(is_loopback_origin("http://localhost:1420"));
        assert!(is_loopback_origin("http://127.0.0.1:8081"));
        assert!(!is_loopback_origin("http://localhost.evil.com"));
        assert!(!is_loopback_origin("https://example.com"));
    }
}
