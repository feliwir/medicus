//! Errors as HTTP responses.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use medicus_core::{Error, ErrorKind};
use serde_json::json;

/// An error answered as an HTTP status with a JSON body.
#[derive(Debug)]
pub(crate) struct WebError {
    status: StatusCode,
    message: String,
}

impl WebError {
    pub(crate) fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    pub(crate) fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    #[cfg(test)]
    pub(crate) fn status(&self) -> StatusCode {
        self.status
    }

    pub(crate) fn not_acceptable(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_ACCEPTABLE, message)
    }
}

impl From<Error> for WebError {
    fn from(e: Error) -> Self {
        let status = match e.kind {
            ErrorKind::InvalidArgument => StatusCode::BAD_REQUEST,
            ErrorKind::NotFound => StatusCode::NOT_FOUND,
            ErrorKind::AlreadyExists => StatusCode::CONFLICT,
            ErrorKind::Rejected | ErrorKind::PermissionDenied => StatusCode::FORBIDDEN,
            ErrorKind::ResourceExhausted => StatusCode::INSUFFICIENT_STORAGE,
            ErrorKind::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            ErrorKind::Unsupported => StatusCode::NOT_IMPLEMENTED,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self::new(status, e.message)
    }
}

impl IntoResponse for WebError {
    fn into_response(self) -> Response {
        if self.status.is_server_error() {
            tracing::error!(status = %self.status, message = %self.message, "DICOMweb request failed");
        }
        (self.status, Json(json!({ "message": self.message }))).into_response()
    }
}

pub(crate) type WebResult<T> = Result<T, WebError>;
