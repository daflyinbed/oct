//! History invariants shared by user and background entry points.
use crate::{
    application::services::ApplicationServices,
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

/// Close the previous tool batch before appending any synthetic user message.
/// Resume calls this after managed task backfills; send calls it before its
/// new user message. The caller owns the conversation's occupancy throughout.
pub async fn prepare_history(runs: &ApplicationServices, id: &str) -> Result<(), AppError> {
    ensure_no_pending_recovery(&runs.pool, id).await?;
    runs.assembly_services()
        .repair_dangling_tools(id, &[])
        .await?;
    // Notification storage is retried by the durable drainer. Its failure
    // must not block the user's turn, but a successful insert must always
    // follow all results of the interrupted tool batch.
    if let Err(error) =
        crate::application::notifications::catch_up_notifications(&runs.pool, id).await
    {
        tracing::warn!(conversation_id = id, %error, "failed to flush background task notifications");
    }
    Ok(())
}

pub async fn can_resume(pool: &sqlx::SqlitePool, id: &str) -> Result<bool, AppError> {
    let conversation = crate::db::conversations::get_conversation(pool, id).await?;
    if conversation
        .as_ref()
        .is_some_and(|c| c.origin_kind == "revert_backup")
    {
        return Ok(false);
    }
    if !msg_db::find_dangling_tool_calls(pool, id).await?.is_empty()
        || msg_db::last_message_is_user(pool, id).await?
    {
        return Ok(true);
    }
    if msg_db::last_message_role(pool, id).await?.as_deref() != Some("tool") {
        return Ok(false);
    }
    let run = run_db::latest_run_for_conversation(pool, id).await?;
    if let Some(run) = run {
        return Ok(matches!(
            run.status.as_str(),
            "lost" | "failed" | "cancelled"
        ));
    }
    // A fork of a closed tool round still needs its assistant continuation.
    // It owns no copied run; the inherited history is enough to resume it.
    Ok(conversation.is_some_and(|c| c.forked_at_revision.is_some() || c.history_epoch > 0))
}
