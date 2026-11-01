//! Error mapping for the control plane.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use octra_core::Error;

#[derive(Debug)]
pub struct ControlError {
    pub status: StatusCode,
    pub message: String,
}

impl ControlError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, message)
    }
}

impl From<Error> for ControlError {
    fn from(error: Error) -> Self {
        match error {
            Error::Config(message) => Self::bad_request(message),
            Error::NotFound(message) => Self::not_found(message),
            Error::Storage(message) if message.starts_with("conflict:") => {
                Self::new(StatusCode::CONFLICT, message)
            }
            Error::Storage(message) if message.starts_with("not_found:") => {
                Self::not_found(message)
            }
            Error::Storage(message) => {
                tracing::error!(error = %message, "api gateway storage error");
                Self::internal("storage error")
            }
            Error::Unauthorized => Self::new(StatusCode::UNAUTHORIZED, "unauthorized"),
            Error::Runtime(message) => {
                tracing::error!(error = %message, "api gateway runtime error");
                Self::internal("runtime error")
            }
            Error::Other(error) => {
                tracing::error!(error = %error, "api gateway error");
                Self::internal("internal error")
            }
        }
    }
}

impl IntoResponse for ControlError {
    fn into_response(self) -> Response {
        let code = match self.status {
            StatusCode::BAD_REQUEST => "bad_request",
            StatusCode::UNAUTHORIZED => "unauthorized",
            StatusCode::NOT_FOUND => "not_found",
            StatusCode::CONFLICT => "conflict",
            _ => "internal_error",
        };
        let body = Json(serde_json::json!({
            "error": { "code": code, "message": self.message }
        }));
        (self.status, body).into_response()
    }
}

pub type ControlResult<T> = Result<T, ControlError>;
