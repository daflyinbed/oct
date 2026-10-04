use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use utoipa::ToSchema;

pub use crate::error::{AppError, AppResult as ApiResult};

/// The error body of EVERY non-2xx API response. `code` is the stable
/// machine-readable cause ("not_found", "bad_request", "conflict",
/// "already_running", "nothing_to_resume", "managed_conversation",
/// "pending_recovery", "bad_gateway", "internal") — clients branch on it
/// and show `error` verbatim.
/// Forks additionally use `invalid_cut`, `stale_revision` and
/// `operation_conflict` to distinguish invalid boundaries from stale views.
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
            AppError::InvalidCut(msg) => (StatusCode::BAD_REQUEST, "invalid_cut", msg.clone()),
            AppError::StaleRevision(msg) => (StatusCode::CONFLICT, "stale_revision", msg.clone()),
            AppError::OperationConflict(msg) => {
                (StatusCode::CONFLICT, "operation_conflict", msg.clone())
            }
            AppError::ReadOnlyConversation(msg) => {
                (StatusCode::CONFLICT, "read_only_conversation", msg.clone())
            }
            AppError::BackgroundPending(msg) => {
                (StatusCode::CONFLICT, "background_pending", msg.clone())
            }
            AppError::SessionBusy(msg) => (StatusCode::CONFLICT, "already_running", msg.clone()),
            AppError::NothingToResume(msg) => {
                (StatusCode::CONFLICT, "nothing_to_resume", msg.clone())
            }
            AppError::ManagedConversation(msg) => {
                (StatusCode::CONFLICT, "managed_conversation", msg.clone())
            }
            AppError::PendingRecovery(msg) => {
                (StatusCode::CONFLICT, "pending_recovery", msg.clone())
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
