//! In-process upstreams for the API gateway.
//!
//! An internal upstream is an HTTP service served by this process instead of a
//! network target. The LLM gateway uses this to sit behind an API gateway route
//! without exposing a second listener.

use std::net::IpAddr;

use async_trait::async_trait;
use axum::extract::Request;
use axum::response::Response;

/// Caller metadata forwarded to an in-process upstream.
#[derive(Debug, Clone)]
pub struct InternalUpstreamContext {
    pub client_ip: Option<IpAddr>,
    pub request_id: String,
}

/// Serves a request entirely in-process. Implementations return a normal HTTP
/// response so the API gateway can keep running its response phases.
#[async_trait]
pub trait InternalUpstream: Send + Sync {
    async fn call(&self, request: Request, context: InternalUpstreamContext) -> Response;
}
