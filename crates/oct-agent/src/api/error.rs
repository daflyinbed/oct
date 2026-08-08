use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use oct_llm_provider::core::ModelError;
use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("{0}")]
    NotFound(String),

    #[error("{0}")]
    BadRequest(String),

    #[error("{0}")]
    Conflict(String),

    #[error("{0}")]
    Internal(#[from] anyhow::Error),
}

impl From<ModelError> for AppError {
    fn from(e: ModelError) -> Self {
        AppError::Internal(anyhow::anyhow!("{e}"))
    }
}

pub type ApiResult<T> = std::result::Result<T, AppError>;

#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match &self {
            AppError::NotFound(msg) => (StatusCode::NOT_FOUND, msg.clone()),
            AppError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg.clone()),
            AppError::Conflict(msg) => (StatusCode::CONFLICT, msg.clone()),
            AppError::Internal(e) => {
                tracing::error!(error = %e, "Internal server error");
                (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
            }
        };
        (status, Json(ErrorBody { error: message })).into_response()
    }
}
