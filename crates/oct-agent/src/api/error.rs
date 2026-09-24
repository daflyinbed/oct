use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use oct_llm_provider::core::ModelError;
use serde::Serialize;
use utoipa::ToSchema;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("{0}")]
    NotFound(String),

    #[error("{0}")]
    BadRequest(String),

    #[error("{0}")]
    Conflict(String),

    /// 409: the conversation's session mutex is already held by an active
    /// run or a manual compaction (send / resume / compact share the entry).
    /// A distinct code so clients can tell it from [`AppError::NothingToResume`]
    /// without parsing the message.
    #[error("{0}")]
    SessionBusy(String),

    /// 409: a resume was requested but the conversation has no interrupted
    /// turn to continue.
    #[error("{0}")]
    NothingToResume(String),

    /// The request itself was fine, but the upstream service it proxied to
    /// (e.g. an MCP server) failed, errored or timed out.
    #[error("{0}")]
    BadGateway(String),

    #[error("{0}")]
    Internal(#[from] anyhow::Error),
}

impl From<ModelError> for AppError {
    fn from(e: ModelError) -> Self {
        AppError::Internal(anyhow::anyhow!("{e}"))
    }
}

pub type ApiResult<T> = std::result::Result<T, AppError>;

/// The error body of EVERY non-2xx API response. `code` is the stable
/// machine-readable cause ("not_found", "bad_request", "conflict",
/// "already_running", "nothing_to_resume", "bad_gateway", "internal") —
/// clients branch on it and show `error` verbatim.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ErrorBody {
    pub code: String,
    pub error: String,
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, code, message) = match &self {
            AppError::NotFound(msg) => (StatusCode::NOT_FOUND, "not_found", msg.clone()),
            AppError::BadRequest(msg) => (StatusCode::BAD_REQUEST, "bad_request", msg.clone()),
            AppError::Conflict(msg) => (StatusCode::CONFLICT, "conflict", msg.clone()),
            AppError::SessionBusy(msg) => {
                (StatusCode::CONFLICT, "already_running", msg.clone())
            }
            AppError::NothingToResume(msg) => {
                (StatusCode::CONFLICT, "nothing_to_resume", msg.clone())
            }
            AppError::BadGateway(msg) => (StatusCode::BAD_GATEWAY, "bad_gateway", msg.clone()),
            AppError::Internal(e) => {
                tracing::error!(error = %e, "Internal server error");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string())
            }
        };
        (
            status,
            Json(ErrorBody {
                code: code.to_string(),
                error: message,
            }),
        )
            .into_response()
    }
}
