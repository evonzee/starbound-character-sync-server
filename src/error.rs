use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;

use crate::db::{FileMeta, Lease};

#[derive(Debug)]
pub enum ApiError {
    Unauthorized,
    BadRequest(String),
    NotFound,
    /// Mutating request carried neither `If-Match` nor `If-None-Match`.
    PreconditionRequired,
    /// `If-Match` / `If-None-Match` didn't match the server's current head.
    PreconditionFailed(Option<FileMeta>),
    LeaseHeld(Lease),
    TooLarge,
    Internal(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            ApiError::Unauthorized => {
                (StatusCode::UNAUTHORIZED, Json(json!({"error": "unauthorized"}))).into_response()
            }
            ApiError::BadRequest(m) => {
                (StatusCode::BAD_REQUEST, Json(json!({"error": m}))).into_response()
            }
            ApiError::NotFound => {
                (StatusCode::NOT_FOUND, Json(json!({"error": "not found"}))).into_response()
            }
            ApiError::PreconditionRequired => (
                StatusCode::PRECONDITION_REQUIRED,
                Json(json!({"error": "send If-Match: \"<version>\" or If-None-Match: *"})),
            )
                .into_response(),
            ApiError::PreconditionFailed(current) => (
                StatusCode::PRECONDITION_FAILED,
                Json(json!({"error": "version conflict", "current": current})),
            )
                .into_response(),
            ApiError::LeaseHeld(holder) => (
                StatusCode::CONFLICT,
                Json(json!({"error": "lease held by another machine", "holder": holder})),
            )
                .into_response(),
            ApiError::TooLarge => (
                StatusCode::PAYLOAD_TOO_LARGE,
                Json(json!({"error": "payload too large"})),
            )
                .into_response(),
            ApiError::Internal(m) => {
                tracing::error!("internal error: {m}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": "internal error"})),
                )
                    .into_response()
            }
        }
    }
}

impl From<rusqlite::Error> for ApiError {
    fn from(e: rusqlite::Error) -> Self {
        ApiError::Internal(format!("db: {e}"))
    }
}
impl From<std::io::Error> for ApiError {
    fn from(e: std::io::Error) -> Self {
        ApiError::Internal(format!("io: {e}"))
    }
}
