use std::sync::Arc;

use crate::agent::{AgentEvent, title};
use crate::db::{conversations as conv_db, messages as msg_db, projects as project_db};
use crate::error::{AppError, AppResult};
use crate::runtime::{RootRunRequest, RunService};

use super::{
    RunSubscription, SendCommand, conversation_idle, ensure_not_managed, launch_root, preparation,
    validate_reasoning,
};

pub(super) async fn send(runs: &Arc<RunService>, req: SendCommand) -> AppResult<RunSubscription> {
    let id = req.conversation_id.clone();
    // The occupancy guard takes the session entry up front (concurrent
    // starts get their 409 here) and releases it on drop — every `?` below
    // frees the conversation, no manual cleanup needed.
    let guard = runs.occupy(&id)?;

    let conversation = conv_db::get_conversation(&runs.pool, &id)
        .await
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::NotFound("Conversation not found".into()))?;

    // Managed subagent sessions are driven by their parent's `task` calls —
    // never by a direct send.
    ensure_not_managed(&conversation)?;
    conversation.ensure_writable()?;

    if let Some(expected) = req.expected_revision {
        let revision = sqlx::query_scalar!(
            "SELECT context_revision FROM conversations WHERE id = $1",
            id
        )
        .fetch_one(&runs.pool)
        .await
        .map_err(anyhow::Error::from)?;
        if revision != expected {
            return Err(AppError::StaleRevision(
                "会话历史已变化，请刷新后重新发送。".into(),
            ));
        }
    }

    let project = project_db::get_project(&runs.pool, &conversation.project_id)
        .await
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::NotFound("Project not found".into()))?;

    let (chat_model, provider_id, model_id) = runs.resolve_model(&req.provider_spec).await?;
    if let Some(reasoning) = &req.reasoning {
        validate_reasoning(runs, &provider_id, &model_id, reasoning).await?;
    }
    let context_window = runs
        .resolve_context_window(chat_model.as_ref(), &provider_id, &model_id)
        .await;

    // A turn with unfinished (crashed) subagent tasks must be recovered, not
    // appended to: the repair below would otherwise turn the pending task
    // calls into error placeholders.
    preparation::ensure_no_pending_recovery(&runs.pool, &id).await?;

    // Idle for the microcompact trigger: captured BEFORE the repair and the
    // new user message below — anything later would always look "just
    // active". Clock skew (negative delta) simply means "not idle".
    let idle = conversation_idle(runs, &id).await;

    // Poison guard: a previous run interrupted mid-tool (e.g. backend crash)
    // leaves assistant ToolCalls without tool messages. Shared preparation
    // repairs them BEFORE background notifications or the new user message
    // are appended, keeping the tool batch consecutive for the provider.
    preparation::prepare_history(runs, &id).await?;

    // One application operation owns the publication lease, frozen snapshots
    // and atomic ticket/message/ownership commit. Filesystem IO finishes before
    // the SQLite write transaction, and every failed claim remains retryable.
    let prepared = runs
        .attachments
        .prepare_message(&id, &req.content, &req.attachment_ids, context_window)
        .await
        .map_err(AppError::from)?;
    // Receipt and reasoning are durable request metadata, independent of the
    // typed user body/attachments assembled by the attachment service.
    let mut details = serde_json::json!({});
    if let Some(request_id) = &req.client_request_id {
        details["client_request_id"] = serde_json::json!(request_id);
    }
    if let Some(reasoning) = &req.reasoning {
        details["reasoning"] = serde_json::to_value(reasoning).map_err(anyhow::Error::from)?;
    }
    prepared
        .commit(&provider_id, &model_id, details)
        .await
        .map_err(AppError::from)?;

    // Runtime captures the durable replay base after preparation and before
    // publishing this execution's content events.

    let view = msg_db::load_llm_view(&runs.pool, &id)
        .await
        .map_err(AppError::from)?;

    // One-shot title generation: an independent background task firing
    // alongside the run (never inside it). Guards: the conversation still
    // carries its default title, and the user message just persisted is its
    // first — a failed attempt is not retried on later turns. Forks start
    // with a default「（分支）」placeholder over copied history, so for them
    // the one-shot fires on the first message sent INSIDE the fork (the row
    // count past the fork point) — that is the moment the branch actually
    // diverges and a regenerated title can say something the source's
    // cannot. Spawned only after every fallible step has succeeded, so an
    // aborted send never burns the one-shot on a message that was never
    // stored. The weak hub reference keeps a slow title call from delaying
    // SSE EOF past the run's end.
    let fork_first_own_message = conversation.origin_kind == "fork"
        && matches!(
            msg_db::count_user_messages_after(
                &runs.pool,
                &id,
                conversation.forked_at_revision.unwrap_or(0),
            )
            .await,
            Ok(1),
        );
    if conversation.title_source == "default"
        && (matches!(msg_db::count_user_messages(&runs.pool, &id).await, Ok(1))
            || fork_first_own_message)
    {
        let hub = runs
            .sessions
            .get(&id)
            .map(|entry| Arc::downgrade(&entry.value().hub));
        if let Some(hub) = hub {
            title::spawn(title::TitleJob {
                runs: runs.clone(),
                project_id: project.id.clone(),
                parent_provider_id: provider_id.clone(),
                parent_model_id: model_id.clone(),
                pool: runs.pool.clone(),
                conversation_id: id.clone(),
                first_user_content: req.content.clone(),
                hub,
            });
        }
    }

    // Bring the MCP connections in sync with the enabled mcp_servers rows
    // before the run's toolset is assembled, from this project's
    // perspective (best-effort: a server that fails to spawn is recorded as
    // Failed and simply contributes no tools).
    let handle = runs
        .sessions
        .get(&id)
        .map(|entry| entry.value().clone())
        .expect("the occupancy guard guarantees the session entry");
    if !runs
        .mcp
        .ensure_connections_cancelled(&runs.pool, Some(&project.id), &handle.cancel)
        .await
    {
        // The user message is durable and remains resumable. Publish on the
        // existing hub too, so reconnect subscribers observe cancellation
        // before this preparation guard releases occupancy.
        handle.hub.publish(AgentEvent::Cancelled);
        return Ok(RunSubscription {
            events: handle.hub.subscribe(),
            run_id: None,
        });
    }

    // Shared tail of the chat endpoints: hand the occupancy guard to the run
    // service — it is CONSUMED by the supervised run, so occupancy lasts
    // until the supervisor settles the terminal state (which keeps GET /run
    // reporting `running` and GET /events able to replay until then) — and
    // return the SSE stream of the run's events.
    let started = launch_root(
        runs,
        RootRunRequest {
            guard,
            conversation_id: id,
            project,
            model: chat_model,
            provider_id,
            model_id,
            view,
            context_window,
            idle,
            reasoning: req.reasoning.clone(),
            // Chat conversations get the delegation tool (their workers then
            // never do — depth <= 1, enforced again at registration).
            attach_task_tool: conversation.kind == "chat",
        },
    )
    .await?;
    Ok(started.into())
}
