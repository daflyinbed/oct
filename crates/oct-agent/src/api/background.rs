//! Background bash task REST surface (ADR 0003): the list the UI polls
//! while tasks run, bounded output reads for the full-log viewer, and the
//! HTTP cancel behind the UI stop button. The model-facing equivalents are
//! the `task_output` / `task_stop` tools; these endpoints serve the
//! frontend only.

use axum::Json;
use axum::extract::{Path, Query, State};
use serde::Deserialize;
use serde::Serialize;
use utoipa::ToSchema;

use super::AppState;
use super::error::{ApiResult, AppError};
use crate::db::background as task_db;
use crate::db::background::BackgroundTask;
use crate::runtime::background::read_output_chunk;

/// One background task row as the API serves it. For a `running` row the
/// `output_bytes` field is refreshed from the live output file (the row
/// only carries the final size once settled).
#[derive(Debug, Serialize, ToSchema)]
pub struct BackgroundTaskDto {
    pub id: String,
    pub conversation_id: String,
    pub command: String,
    pub working_dir: Option<String>,
    /// `running` | `completed` | `failed` | `cancelled` | `lost`
    pub status: String,
    pub exit_code: Option<i64>,
    pub output_path: String,
    pub output_bytes: Option<i64>,
    pub error: Option<String>,
    pub created_at: String,
    pub finished_at: Option<String>,
}

impl BackgroundTaskDto {
    async fn from_row(task: BackgroundTask) -> Self {
        let output_bytes = if task.status == "running" {
            tokio::fs::metadata(&task.output_path)
                .await
                .ok()
                .map(|m| i64::try_from(m.len()).unwrap_or(i64::MAX))
                .or(task.output_bytes)
        } else {
            task.output_bytes
        };
        Self {
            id: task.id,
            conversation_id: task.conversation_id,
            command: task.command,
            working_dir: task.working_dir,
            status: task.status,
            exit_code: task.exit_code,
            output_path: task.output_path,
            output_bytes,
            error: task.error,
            created_at: task.created_at.format("%Y-%m-%dT%H:%M:%S").to_string(),
            finished_at: task
                .finished_at
                .map(|t| t.format("%Y-%m-%dT%H:%M:%S").to_string()),
        }
    }
}

/// Query params for the output read: `offset` pages from the start
/// (default: the tail), `limit` bounds the slice.
#[derive(Debug, Deserialize, ToSchema)]
pub struct TaskOutputQuery {
    /// Byte offset to read from; omit for the tail.
    pub offset: Option<u64>,
    /// Maximum bytes to return (default 64 KiB, cap 1 MiB).
    pub limit: Option<u64>,
}

/// One bounded slice of a task's output file.
#[derive(Debug, Serialize, ToSchema)]
pub struct TaskOutputChunkDto {
    /// Lossy-UTF-8 text of the slice.
    pub content: String,
    /// Absolute byte offset the content starts at.
    pub start: u64,
    /// Offset the next page starts at, when more output follows.
    pub next_offset: Option<u64>,
    /// Current total size of the output file.
    pub total_bytes: u64,
}

/// Every background task of a conversation, running first then oldest
/// first — the list the bottom-bar indicator and the task cards poll.
#[utoipa::path(
    get,
    path = "/conversations/{id}/background-tasks",
    params(("id" = String, Path, description = "Conversation ID")),
    responses(
        (status = 200, description = "Background tasks of the conversation", body = Vec<BackgroundTaskDto>),
        (status = 404, description = "Conversation not found")
    ),
    tag = "background-tasks"
)]
pub async fn list_background_tasks(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<BackgroundTaskDto>>> {
    crate::db::conversations::get_conversation(&state.pool, &id)
        .await?
        .ok_or_else(|| AppError::NotFound("Conversation not found".into()))?;

    let tasks = task_db::list_tasks_for_conversation(&state.pool, &id).await?;
    let mut dtos = Vec::with_capacity(tasks.len());
    for task in tasks {
        dtos.push(BackgroundTaskDto::from_row(task).await);
    }
    Ok(Json(dtos))
}

/// Read a bounded slice of one task's output file.
#[utoipa::path(
    get,
    path = "/background-tasks/{id}/output",
    params(
        ("id" = String, Path, description = "Background task ID"),
        ("offset" = Option<u64>, Query, description = "Byte offset to read from; omit for the tail"),
        ("limit" = Option<u64>, Query, description = "Maximum bytes to return (default 64 KiB, cap 1 MiB)"),
    ),
    responses(
        (status = 200, description = "One bounded slice of the output", body = TaskOutputChunkDto),
        (status = 404, description = "Task not found")
    ),
    tag = "background-tasks"
)]
pub async fn read_background_task_output(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<TaskOutputQuery>,
) -> ApiResult<Json<TaskOutputChunkDto>> {
    let task = task_db::get_task(&state.pool, &id)
        .await?
        .ok_or_else(|| AppError::NotFound("Background task not found".into()))?;

    let limit = query.limit.unwrap_or(64 * 1024).clamp(1, 1024 * 1024);
    let chunk = read_output_chunk(std::path::Path::new(&task.output_path), query.offset, limit)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("failed to read task output: {e}")))?;

    Ok(Json(TaskOutputChunkDto {
        content: chunk.content,
        start: chunk.start,
        next_offset: chunk.next_offset,
        total_bytes: chunk.total_bytes,
    }))
}

/// Stop a background task (the UI stop button; the model's equivalent is
/// the `task_stop` tool). Active tasks are killed gracefully-then-forced;
/// `lost` rows are persisted as cancelled.
#[utoipa::path(
    post,
    path = "/background-tasks/{id}/cancel",
    params(("id" = String, Path, description = "Background task ID")),
    responses(
        (status = 200, description = "The settled task row", body = BackgroundTaskDto),
        (status = 404, description = "Task not found")
    ),
    tag = "background-tasks"
)]
pub async fn cancel_background_task(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<BackgroundTaskDto>> {
    // Scope check before any cancel: an id from another conversation is a
    // plain 404 from here.
    let row = task_db::get_task(&state.pool, &id)
        .await?
        .ok_or_else(|| AppError::NotFound("Background task not found".into()))?;

    let settled = if row.status == "running" {
        state.runs.background.cancel_and_wait(&id).await
    } else if row.status == "lost" {
        task_db::cancel_lost_task(&state.pool, &id).await?;
        task_db::get_task(&state.pool, &id).await?
    } else {
        Some(row)
    };

    let task = settled.ok_or_else(|| AppError::NotFound("Background task not found".into()))?;
    Ok(Json(BackgroundTaskDto::from_row(task).await))
}
