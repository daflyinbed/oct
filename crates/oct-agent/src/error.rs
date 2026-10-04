//! Application errors; HTTP status mapping lives in api::error.
use oct_llm_provider::core::ModelError;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("{0}")]
    NotFound(String),

    #[error("{0}")]
    BadRequest(String),

    #[error("{0}")]
    Conflict(String),

    #[error("{0}")]
    InvalidCut(String),
    #[error("{0}")]
    StaleRevision(String),
    #[error("{0}")]
    OperationConflict(String),
    #[error("{0}")]
    ReadOnlyConversation(String),
    #[error("{0}")]
    BackgroundPending(String),

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

    /// 409: the conversation is a managed subagent session — send / resume /
    /// compact must go through the parent conversation (or the manager),
    /// never start a run behind its back.
    #[error("{0}")]
    ManagedConversation(String),

    /// 409: the conversation's last turn has unfinished (crashed) subagent
    /// tasks awaiting recovery; a plain send or compact would destroy the
    /// pending wait. Resume (恢复此轮) instead.
    #[error("{0}")]
    PendingRecovery(String),

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

impl From<sqlx::Error> for AppError {
    fn from(error: sqlx::Error) -> Self {
        Self::Internal(error.into())
    }
}

pub type AppResult<T> = std::result::Result<T, AppError>;
