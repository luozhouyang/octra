//! Mapping between gateway errors, octra-core errors and axum responses.

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use octra_core::{Error, Result};
use octra_llm_core::{ErrorKind, GatewayError, Protocol};
use octra_llm_protocol::encode_error;
use octra_storage::LlmStorageError;

/// Renders an error in the ingress protocol's shape.
pub fn protocol_response(protocol: Protocol, error: &GatewayError) -> Response {
    let status = StatusCode::from_u16(error.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut response = (status, axum::Json(encode_error(protocol, error))).into_response();
    if error.kind == ErrorKind::Unauthorized
        && let Ok(value) = HeaderValue::from_str("Bearer")
    {
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, value);
    }
    response
}

pub fn to_core(error: GatewayError) -> Error {
    Error::Runtime(error.message)
}

pub fn from_storage(error: LlmStorageError) -> GatewayError {
    match error {
        LlmStorageError::Backend(message) => {
            tracing::error!(%message, "llm storage error");
            GatewayError::internal("storage error")
        }
        LlmStorageError::Conflict(_) | LlmStorageError::Missing(_) => {
            GatewayError::internal("storage conflict")
        }
    }
}

pub fn from_core(error: Error) -> GatewayError {
    match error {
        Error::Config(message) => GatewayError::config(message),
        other => {
            tracing::error!(error = %other, "llm runtime error");
            GatewayError::internal("internal error")
        }
    }
}

pub type LlmResult<T> = std::result::Result<T, GatewayError>;

#[allow(dead_code)]
fn _assert_result_alias(_: Result<()>) {}
