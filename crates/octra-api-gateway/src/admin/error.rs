//! Kong-compatible error responses for the Admin API.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use octra_core::Error;
use serde_json::{Value, json};

#[derive(Debug)]
pub struct AdminError {
    pub status: StatusCode,
    pub name: &'static str,
    pub code: i32,
    pub message: String,
    pub fields: Option<Value>,
}

impl AdminError {
    pub fn schema(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            name: "schema violation",
            code: 2,
            message: message.into(),
            fields: None,
        }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            name: "not found",
            code: 404,
            message: message.into(),
            fields: None,
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            name: "internal error",
            code: 1,
            message: message.into(),
            fields: None,
        }
    }

    fn conflict(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            name: "unique constraint violation",
            code: 5,
            message: message.into(),
            fields: None,
        }
    }
}

impl From<Error> for AdminError {
    fn from(error: Error) -> Self {
        match error {
            Error::Config(message) => Self::schema(message),
            Error::NotFound(message) => Self::not_found(message),
            Error::Storage(message) if message.starts_with("conflict:") => Self::conflict(message),
            Error::Storage(message) if message.starts_with("not_found:") => {
                Self::not_found(message)
            }
            Error::Storage(message) => Self::internal(message),
            Error::Runtime(message) => Self::internal(message),
            Error::Unauthorized => Self {
                status: StatusCode::UNAUTHORIZED,
                name: "unauthorized",
                code: 401,
                message: "Unauthorized".into(),
                fields: None,
            },
            Error::Other(error) => Self::internal(error.to_string()),
        }
    }
}

impl IntoResponse for AdminError {
    fn into_response(self) -> Response {
        let mut body = json!({
            "name": self.name,
            "code": self.code,
            "message": self.message,
        });
        if let Some(fields) = self.fields
            && let Some(object) = body.as_object_mut()
        {
            object.insert("fields".into(), fields);
        }
        (self.status, Json(body)).into_response()
    }
}

pub type AdminResult<T> = Result<T, AdminError>;
