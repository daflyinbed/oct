//! The subagent API surface (PR2): the task list of a root
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
use super::sse::sse_event;
use crate::db::{conversations as conv_db, runs as run_db};

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
    /// The configuration actually sealed for this task; absent on legacy runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_config: Option<super::agent_settings::WorkerSnapshot>,
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

    let mut rows = run_db::list_subagent_runs(&state.pool, &id).await?;
    if conv_db::get_conversation(&state.pool, &id)
        .await?
        .is_some_and(|c| c.history_epoch > 0)
    {
        let calls = crate::db::history::visible_call_ids(&state.pool, &id).await?;
        rows.retain(|r| {
            r.parent_tool_call_id
                .as_ref()
                .is_some_and(|call| calls.contains(call))
        });
    }
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
                agent_config: row
                    .agent_config_json
                    .as_deref()
                    .and_then(|s| serde_json::from_str(s).ok()),
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
    if let Some(parent) = conversation.parent_conversation_id.as_deref()
        && conv_db::get_conversation(&state.pool, parent)
            .await?
            .is_some_and(|c| c.history_epoch > 0)
    {
        let calls = crate::db::history::visible_call_ids(&state.pool, parent).await?;
        let run = run_db::latest_run_for_conversation(&state.pool, agent_id).await?;
        if !run
            .and_then(|r| r.parent_tool_call_id)
            .is_some_and(|call| calls.contains(&call))
        {
            return Err(AppError::NotFound(
                "Subagent is no longer in the active conversation history".into(),
            ));
        }
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
) -> ApiResult<Response> {
    require_subagent_conversation(&state, &agent_id).await?;

    // Use the same transactional snapshot and revision/epoch headers as roots.
    super::chat::get_messages(State(state), Path(agent_id)).await
}

#[utoipa::path(
    get,
    path = "/subagents/{agentId}/events",
    params(("agentId" = String, Path, description = "Subagent (child conversation) ID")),
    responses(
        (status = 200, description = "SSE stream of the ACTIVE child run: the execution log including ready run_meta metadata, followed by live events"),
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

    // Metadata and replay come from the same execution hub; recovery never
    // reuses the original delegation as the event-log boundary.
    let stream = hub.subscribe().map(|e| Ok::<_, Infallible>(sse_event(&e)));

    Ok(Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response())
}

#[utoipa::path(
    post,
    path = "/subagents/{agentId}/cancel",
    params(("agentId" = String, Path, description = "Subagent (child conversation) ID")),
    responses(
        (status = 200, description = "Cancellation requested (active or starting run), or an unowned recoverable run persisted as cancelled"),
        (status = 404, description = "No subagent, no run, or nothing cancellable")
    ),
    tag = "subagents"
)]
pub async fn cancel_subagent(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> ApiResult<StatusCode> {
    require_subagent_conversation(&state, &agent_id).await?;

    // The manager distinguishes live/startup owners from unowned DB rows,
    // and serializes persisted cancellation against registration/recovery.
    if state
        .runs
        .delegation_service()
        .cancel_run(&agent_id)
        .await?
    {
        return Ok(StatusCode::OK);
    }

    Err(AppError::NotFound(
        "No active or recoverable run for this subagent".into(),
    ))
}
