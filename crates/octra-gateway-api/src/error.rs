use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    pub fn internal(error: impl std::fmt::Display) -> Self {
        tracing::error!(error = %error, "control api request failed");
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "internal error".to_string(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = Json(serde_json::json!({ "error": self.message }));
        (self.status, body).into_response()
    }
}
