//! The subagent API surface (ADR 0002 §7 / PR2): the task list of a root
//! conversation, read-only history access to a managed child conversation,
//! its live event stream, and per-task cancellation. The field names are
//! the FIXED frontend contract — the TaskCard and the bottom-bar list
//! render straight from them.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::sse::{KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use chrono::NaiveDateTime;
use futures_util::StreamExt;
use serde::Serialize;
use std::convert::Infallible;
use utoipa::ToSchema;

use super::AppState;
use super::dto;
use super::error::{ApiResult, AppError};
use crate::agent::AgentEvent;
use crate::db::{conversations as conv_db, messages as msg_db, runs as run_db};
use crate::runtime::sse_event;
use crate::subagent::await_done;

/// One row of the subagent task list: the persisted run row merged with the
/// live in-memory activity. Field names are the fixed frontend contract.
#[derive(Debug, Serialize, ToSchema)]
pub struct SubagentTaskDto {
    pub run_id: String,
    /// The child conversation id (== agent id in v1).
    pub agent_id: String,
    pub description: String,
    /// `running` | `completed` | `failed` | `cancelled` | `lost` (from the
    /// DB row; `lost` = awaiting recovery).
    pub status: String,
    pub execution_epoch: i64,
    pub parent_call_id: Option<String>,
    pub provider_id: Option<String>,
    pub model_id: Option<String>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub created_at: NaiveDateTime,
    pub finished_at: Option<NaiveDateTime>,
    pub final_message_id: Option<String>,
    /// The child conversation's title (== the task description).
    pub title: String,
    /// Whether an in-memory execution instance is live right now.
    pub running: bool,
}

#[utoipa::path(
    get,
    path = "/conversations/{id}/subagents",
    params(("id" = String, Path, description = "Root conversation ID")),
    responses(
        (status = 200, description = "Subagent tasks of the conversation's task tree, running first then oldest first", body = Vec<SubagentTaskDto>),
        (status = 404, description = "Conversation not found")
    ),
    tag = "subagents"
)]
pub async fn list_subagents(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<SubagentTaskDto>>> {
    conv_db::get_conversation(&state.pool, &id)
        .await?
        .ok_or_else(|| AppError::NotFound("Conversation not found".into()))?;

    let rows = run_db::list_subagent_runs(&state.pool, &id).await?;
    let tasks = rows
        .into_iter()
        .map(|row| {
            let running = state.runs.subagents.is_active(&row.conversation_id);
            SubagentTaskDto {
                run_id: row.id,
                agent_id: row.conversation_id,
                description: row.description.clone().unwrap_or_default(),
                status: row.status,
                execution_epoch: row.execution_epoch,
                parent_call_id: row.parent_tool_call_id,
                provider_id: row.provider_id,
                model_id: row.model_id,
                input_tokens: row.input_tokens,
                output_tokens: row.output_tokens,
                created_at: row.started_at,
                finished_at: row.finished_at,
                final_message_id: row.final_message_id,
                title: row.title,
                running,
            }
        })
        .collect();
    Ok(Json(tasks))
}

/// Load a conversation and require it to be a managed subagent session.
async fn require_subagent_conversation(
    state: &AppState,
    agent_id: &str,
) -> Result<conv_db::Conversation, AppError> {
    let conversation = conv_db::get_conversation(&state.pool, agent_id)
        .await?
        .ok_or_else(|| AppError::NotFound("Subagent not found".into()))?;
    if conversation.kind != "subagent" {
        return Err(AppError::NotFound(
            "Conversation is not a subagent session".into(),
        ));
    }
    Ok(conversation)
}

#[utoipa::path(
    get,
    path = "/subagents/{agentId}/messages",
    params(("agentId" = String, Path, description = "Subagent (child conversation) ID")),
    responses(
        (status = 200, description = "The subagent conversation's full history (same typed contract as the chat endpoint)", body = Vec<dto::MessageDto>),
        (status = 404, description = "No subagent conversation with this id")
    ),
    tag = "subagents"
)]
pub async fn get_subagent_messages(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> ApiResult<Json<Vec<dto::MessageDto>>> {
    require_subagent_conversation(&state, &agent_id).await?;

    let messages = msg_db::list_messages(&state.pool, &agent_id).await?;
    Ok(Json(messages.iter().map(dto::MessageDto::from_stored).collect()))
}

#[utoipa::path(
    get,
    path = "/subagents/{agentId}/events",
    params(("agentId" = String, Path, description = "Subagent (child conversation) ID")),
    responses(
        (status = 200, description = "SSE stream of the ACTIVE child run: a synthetic run_meta event (the delegation message boundary) followed by the run's full event log replayed from its start, then live events"),
        (status = 404, description = "No active run for this subagent (it ended or was lost — read the history instead)")
    ),
    tag = "subagents"
)]
pub async fn stream_subagent_events(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> ApiResult<Response> {
    require_subagent_conversation(&state, &agent_id).await?;

    // The child run lookup goes through the live map: only an ACTIVE run has
    // a hub to subscribe to (a terminal child's info lives in the DB and the
    // panel falls back to the history endpoint).
    let child = state
        .runs
        .subagents
        .child_by_agent(&agent_id)
        .ok_or_else(|| AppError::NotFound("No active run for this subagent".into()))?;
    let hub = child
        .hub
        .upgrade()
        .ok_or_else(|| AppError::NotFound("No active run for this subagent".into()))?;

    // Same per-subscriber run_meta contract as the chat events endpoint: the
    // delegation message is the boundary where a reconnecting panel truncates
    // its DB-loaded history. Never published to the hub itself.
    let start_message_id = state
        .sessions
        .get(&agent_id)
        .and_then(|entry| entry.value().start_message_id.clone());
    let meta =
        start_message_id.map(|start_message_id| AgentEvent::RunMeta { start_message_id });

    let stream = futures_util::stream::iter(meta)
        .chain(hub.subscribe())
        .map(|e| Ok::<_, Infallible>(sse_event(&e)));

    Ok(Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response())
}

#[utoipa::path(
    post,
    path = "/subagents/{agentId}/cancel",
    params(("agentId" = String, Path, description = "Subagent (child conversation) ID")),
    responses(
        (status = 200, description = "Cancellation requested (active run), or a lost run persisted as cancelled"),
        (status = 404, description = "No subagent, no run, or nothing cancellable")
    ),
    tag = "subagents"
)]
pub async fn cancel_subagent(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> ApiResult<StatusCode> {
    require_subagent_conversation(&state, &agent_id).await?;

    // Active run: cancel its token; the supervisor settles it as cancelled.
    if state.runs.subagents.cancel(&agent_id).is_some() {
        return Ok(StatusCode::OK);
    }

    // NOT active, but the latest run is `lost` (a crashed execution instance
    // with no live memory handle): persist it as cancelled so a later resume
    // never restarts it (ADR 0002 §6.3).
    if let Some(run) = run_db::latest_run_for_conversation(&state.pool, &agent_id).await?
        && run.status == "lost"
    {
        run_db::finish_run(
            &state.pool,
            &run.id,
            run_db::RunFinalStatus::Cancelled,
            run.final_message_id.as_deref(),
            None,
            run.input_tokens,
            run.output_tokens,
            // Any epoch: the row is `lost`, so no live instance can race us.
            None,
        )
        .await?;
        return Ok(StatusCode::OK);
    }

    Err(AppError::NotFound(
        "No active or recoverable run for this subagent".into(),
    ))
}

/// Used by the conversation-delete flow: cancel every ACTIVE child of the
/// root conversation and wait for their settlement (bounded — a stuck child
/// must not hang the DELETE; the timeout is logged, never silent).
pub(crate) async fn cancel_children_of_root(
    state: &AppState,
    root_conversation_id: &str,
) -> Result<(), AppError> {
    let children = state
        .runs
        .subagents
        .active_children_of_root(&state.pool, root_conversation_id)
        .await
        .map_err(AppError::from)?;
    for child in &children {
        child.cancel.cancel();
    }
    for child in &children {
        if !await_done(child, std::time::Duration::from_secs(10)).await {
            tracing::warn!(
                run_id = %child.run_id,
                "timed out waiting for an active subagent to settle during conversation deletion"
            );
        }
    }
    Ok(())
}
