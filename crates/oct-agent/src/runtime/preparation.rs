//! History invariants shared by user and background entry points.
use crate::{
    db::{messages as msg_db, runs as run_db},
    error::AppError,
};

pub async fn ensure_no_pending_recovery(pool: &sqlx::SqlitePool, id: &str) -> Result<(), AppError> {
    let dangling = msg_db::find_dangling_tool_calls(pool, id)
        .await
        .map_err(AppError::from)?;
    if dangling.is_empty() {
        return Ok(());
    }
    let managed = run_db::runs_for_parent_calls(pool, id, &dangling)
        .await
        .map_err(AppError::from)?;
    if managed.is_empty() {
        return Ok(());
    }
    Err(AppError::PendingRecovery(
        "This turn has unfinished subagent tasks waiting for recovery; use 恢复此轮 (resume) to continue it"
            .into(),
    ))
}

pub async fn prepare_history(pool: &sqlx::SqlitePool, id: &str) -> Result<(), AppError> {
    ensure_no_pending_recovery(pool, id).await?;
    msg_db::repair_dangling_tool_calls(pool, id).await?;
    Ok(())
}

pub async fn can_resume(pool: &sqlx::SqlitePool, id: &str) -> Result<bool, AppError> {
    if !msg_db::find_dangling_tool_calls(pool, id).await?.is_empty()
        || msg_db::last_message_is_user(pool, id).await?
    {
        return Ok(true);
    }
    let interrupted = run_db::latest_run_for_conversation(pool, id)
        .await?
        .is_some_and(|r| matches!(r.status.as_str(), "lost" | "failed" | "cancelled"));
    Ok(interrupted && msg_db::last_message_role(pool, id).await?.as_deref() == Some("tool"))
}
