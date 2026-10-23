use std::sync::Arc;

use crate::application::services::ApplicationServices;
use crate::db::{conversations as conv_db, messages as msg_db, projects as project_db};
use crate::error::{AppError, AppResult};

use super::recovery::{RecoveryJob, orchestrate};
use super::{ResumeCommand, RunSubscription, conversation_idle, ensure_not_managed, preparation};

pub(super) async fn resume(
    runs: &Arc<ApplicationServices>,
    command: ResumeCommand,
) -> AppResult<RunSubscription> {
    let id = command.conversation_id;
    // Same occupancy guard as send_message: 409 on a live run, release on
    // every error exit below via drop. The token is the orchestrator's:
    // children restarted during recovery derive child tokens from it, and
    // the parent run started at the end reuses the same session handle — so
    // the cancel endpoint covers both phases.
    let cancel = tokio_util::sync::CancellationToken::new();
    let guard = runs.occupancy().occupy_with(&id, cancel.clone())?;

    let conversation = conv_db::get_conversation(&runs.pool, &id)
        .await
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::NotFound("Conversation not found".into()))?;

    // Managed subagent sessions recover through their parent conversation.
    ensure_not_managed(&conversation)?;
    conversation.ensure_writable()?;

    let project = project_db::get_project(&runs.pool, &conversation.project_id)
        .await
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::NotFound("Project not found".into()))?;

    // Read-only resumability probe — EXTENDED beyond dangling calls
    // (恢复资格看 run 状态): a dangling ToolCall OR a trailing unanswered user message
    // both mean the last turn never completed, and so does a `lost` latest
    // run whose last row is a tool result (every tool result landed, but the
    // next model request never completed). Nothing is persisted yet — an
    // empty/answered conversation must 409 regardless of provider state.
    let dangling = msg_db::find_dangling_tool_calls(&runs.pool, &id)
        .await
        .map_err(AppError::from)?;
    if !preparation::can_resume(&runs.pool, &id).await? {
        return Err(AppError::NothingToResume("Nothing to resume".into()));
    }

    // The resumed turn reuses the provider/model the conversation last ran
    // with; there is no request body to carry a fresh spec. Resolved before
    // ANY write (also before the orchestrator spawns): persisting
    // placeholder rows and then failing to resolve would leave the
    // conversation permanently unresumable.
    let provider_spec = match msg_db::get_last_provider_spec(&runs.pool, &id).await? {
        Some((provider_id, model_id)) => format!("{provider_id}:{model_id}"),
        None => {
            return Err(AppError::BadRequest(
                "Conversation has no recorded provider/model to resume with".into(),
            ));
        }
    };

    let (chat_model, provider_id, model_id) = runs
        .assembly_services()
        .models
        .resolve_model(&provider_spec)
        .await?;
    let context_window = runs
        .assembly_services()
        .models
        .resolve_context_window(chat_model.as_ref(), &provider_id, &model_id)
        .await;

    // Restore the interrupted turn's reasoning level from the trailing user
    // message's details — the resumed request must not silently drop thinking
    // mid-conversation. Malformed stored values degrade to None (no level).
    let reasoning = msg_db::get_last_user_details(&runs.pool, &id)
        .await
        .map_err(AppError::from)?
        .and_then(|details| details.get("reasoning").cloned())
        .and_then(|value| serde_json::from_value(value).ok());

    // Idle captured before any repair writes below (see send_message).
    let idle = conversation_idle(runs, &id).await;

    // Subscribe to the (already occupied) conversation's hub and return the
    // SSE response IMMEDIATELY: keep-alive covers the recovery wait, and
    // children publish `subagent_updated` onto this same hub so the UI is
    // live. The orchestrator does every write (repair, backfill, child
    // restarts), flushes background notifications after all tool results,
    // and finally starts the parent loop over the guard it holds.
    let handle = runs
        .sessions
        .get(&id)
        .map(|entry| entry.value().clone())
        .expect("the occupancy guard guarantees the session entry");
    let hub = handle.hub.clone();
    let events = handle.hub.subscribe();

    tokio::spawn(orchestrate(RecoveryJob {
        runs: runs.clone(),
        hub,
        cancel,
        guard,
        conversation_id: id,
        project,
        model: chat_model,
        provider_id,
        model_id,
        context_window,
        idle,
        reasoning,
        dangling,
    }));

    Ok(RunSubscription {
        events,
        run_id: None,
    })
}
