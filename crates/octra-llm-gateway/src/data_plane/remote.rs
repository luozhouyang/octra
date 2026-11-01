//! Request extraction and JSON rendering for the data plane.
//!
//! The request body is read manually (to enforce the configured size limit),
//! so handlers take [`Incoming`], a lightweight extractor holding the request
//! parts. Responses are wrapped in [`LlmResponse`] so handler signatures do
//! not depend on which response helper is used internally.

use axum::body::Body;
use axum::extract::{FromRequest, FromRequestParts, Request, State};
use axum::http::header;
use axum::http::request::Parts;
use axum::http::{HeaderMap, Method, Uri};
use axum::response::{IntoResponse, Response};
use serde_json::Value;

use crate::runtime::LlmDataState;

/// Extracted request metadata. The body is taken separately from the handler
/// using [`axum::extract::Request`], which this type intentionally avoids.
pub struct Incoming {
    pub method: Method,
    pub uri: Uri,
    pub headers: HeaderMap,
}

impl FromRequestParts<LlmDataState> for Incoming {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        _state: &LlmDataState,
    ) -> Result<Self, Self::Rejection> {
        Ok(Self {
            method: parts.method.clone(),
            uri: parts.uri.clone(),
            headers: parts.headers.clone(),
        })
    }
}

/// Buffered request body, read before the handler runs under the configured
/// size limit. A failing read rejects with the protocol error response.
pub struct RemoteBody(pub Result<bytes::Bytes, LlmResponse>);

impl FromRequest<LlmDataState> for RemoteBody {
    type Rejection = LlmResponse;

    async fn from_request(req: Request, state: &LlmDataState) -> Result<Self, Self::Rejection> {
        let limit = state.config.gateway.max_body_bytes.max(1);
        match axum::body::to_bytes(req.into_body(), limit).await {
            Ok(bytes) => Ok(Self(Ok(bytes))),
            Err(_) => Err(LlmResponse(toolarge())),
        }
    }
}

fn toolarge() -> Response {
    let error = octra_llm_core::GatewayError::new(
        413,
        octra_llm_core::ErrorKind::PayloadTooLarge,
        "request body is too large",
    );
    crate::error::protocol_response(octra_llm_core::Protocol::Openai, &error)
}

/// Response wrapper for data plane handlers.
pub struct LlmResponse(pub Response);

impl IntoResponse for LlmResponse {
    fn into_response(self) -> Response {
        self.0
    }
}

/// Serializes a JSON value as a response.
pub fn json_response(body: Value) -> Response {
    let payload = body.to_string();
    Response::builder()
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(payload))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

#[allow(dead_code)]
fn _touch(_: Incoming, _: State<LlmDataState>) {}
