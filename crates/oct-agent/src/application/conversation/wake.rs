use std::time::Duration;

use oct_llm_provider::core::{Message, Role};
use tokio_util::sync::CancellationToken;

use crate::db::background as task_db;
use crate::error::{AppError, AppResult};
use crate::runtime::background::build_notification_content;
use crate::runtime::{RootRunRequest, RunService};

use super::{launch_root, preparation};

/// Poll period while the drainer waits for the conversation to go idle.
const DELIVERY_IDLE_RETRY: Duration = Duration::from_secs(1);

/// Deliver EVERY pending notification of the conversation as one batched
/// synthetic user message and start the wake run over it. Takes the
/// occupancy first (retrying while busy) so the message insert and the
/// run start are serialized against send/resume/compact exactly like a
/// user turn.
pub(super) async fn wake(service: &RunService, conversation_id: &str) -> AppResult<()> {
    // The conversation must still exist (deletion cascades the task
    // rows away — a stale pending list simply yields nothing below).
    let conversation = crate::db::conversations::get_conversation(&service.pool, conversation_id)
        .await?
        .ok_or_else(|| AppError::NotFound("conversation is gone".into()))?;
    if conversation.kind != "chat" {
        // Background tasks are gated to root chat runs; defensive.
        return Err(AppError::Internal(anyhow::anyhow!(
            "not a chat conversation"
        )));
    }

    // Wait for the conversation to go idle, then hold the occupancy for
    // the whole insert + run start.
    let cancel = CancellationToken::new();
    let guard = loop {
        match service.occupy_with(conversation_id, cancel.clone()) {
            Ok(guard) => break guard,
            Err(crate::error::AppError::SessionBusy(_)) => {
                tokio::time::sleep(DELIVERY_IDLE_RETRY).await;
            }
            Err(e) => return Err(e),
        }
    };

    // Re-read under the guard: tasks settled while we were waiting join
    // this batch (the loop's next query would catch them too, but one
    // message per wake keeps the model turns cheap).
    let pending = task_db::list_pending_notifications(&service.pool, conversation_id).await?;
    let previous = task_db::pending_wake(&service.pool, conversation_id).await?;
    if pending.is_empty() && previous.is_none() {
        return Ok(());
    }
    preparation::ensure_no_pending_recovery(&service.pool, conversation_id).await?;
    service.repair_dangling_tools(conversation_id, &[]).await?;
    let start_message_id = if pending.is_empty() {
        previous.expect("checked above")
    } else {
        let (content, details) = build_notification_content(&pending).await;
        let message = Message::text(Role::User, &content);
        let task_ids: Vec<String> = pending.iter().map(|t| t.id.clone()).collect();
        task_db::insert_notification_marking_tasks(
            &service.pool,
            conversation_id,
            &message,
            Some(&details),
            &task_ids,
        )
        .await?
    };

    // The wake run reuses the conversation's last provider/model — the
    // same rule resume follows. Resolution failures retain the durable
    // wake intent; the drainer retries without duplicating the message.
    let Some((provider_id, model_id)) =
        crate::db::messages::get_last_provider_spec(&service.pool, conversation_id).await?
    else {
        tracing::warn!(
            conversation_id,
            "background notification delivered without a wake run: no provider recorded"
        );
        return Err(AppError::Internal(anyhow::anyhow!(
            "no provider recorded for pending wake"
        )));
    };
    let spec = format!("{provider_id}:{model_id}");
    let (model, provider_id, model_id) = match service.resolve_model(&spec).await {
        Ok(resolved) => resolved,
        Err(e) => {
            tracing::warn!(
                conversation_id,
                "background notification delivered without a wake run: {e}"
            );
            return Err(e);
        }
    };
    let context_window = service
        .resolve_context_window(model.as_ref(), &provider_id, &model_id)
        .await;

    let project = crate::db::projects::get_project(&service.pool, &conversation.project_id)
        .await?
        .ok_or_else(|| AppError::NotFound("project is gone".into()))?;

    let view = crate::db::messages::load_llm_view(&service.pool, conversation_id).await?;

    // Same pre-run connection sync a user turn performs (best effort).
    if !service
        .mcp
        .ensure_connections_cancelled(&service.pool, Some(&project.id), &cancel)
        .await
    {
        task_db::cancel_preparing_wake(&service.pool, conversation_id, &start_message_id).await?;
        return Ok(());
    }

    let started = launch_root(
        service,
        RootRunRequest {
            guard,
            conversation_id: conversation_id.to_string(),
            project,
            model,
            provider_id,
            model_id,
            view,
            context_window,
            // The notification itself is the activity; the idle
            // microcompact trigger is meaningless here.
            idle: None,
            // 后台唤醒不是用户回合：不携带思考档位。
            reasoning: None,
            attach_task_tool: true,
        },
    )
    .await?;
    let run_id = started.run_id;
    drop(started.events);
    // Starting the run is only admission, not successful delivery. Wait
    // for durable settlement so model failures use the drainer's retry
    // delay while the wake intent remains available for the next run.
    loop {
        let Some(run) = crate::db::runs::get_run(&service.pool, &run_id).await? else {
            return Ok(()); // Conversation deletion cascaded the run.
        };
        match run.status.as_str() {
            "running" => tokio::time::sleep(DELIVERY_IDLE_RETRY).await,
            "failed" => {
                return Err(AppError::Internal(anyhow::anyhow!(
                    "wake run {run_id} failed"
                )));
            }
            _ => return Ok(()),
        }
    }
}
