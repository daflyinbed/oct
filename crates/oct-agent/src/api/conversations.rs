use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;

use super::AppState;
use super::error::{ApiResult, AppError};
use crate::db::{conversations as db, projects as project_db};

#[utoipa::path(
    get,
    path = "/projects/{projectId}/conversations",
    params(("projectId" = String, Path, description = "Project ID")),
    responses(
        (status = 200, description = "List of conversations", body = Vec<db::Conversation>),
        (status = 404, description = "Project not found")
    ),
    tag = "conversations"
)]
pub async fn list_conversations(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
) -> ApiResult<Json<Vec<db::Conversation>>> {
    if project_db::get_project(&state.pool, &project_id)
        .await?
        .is_none()
    {
        return Err(AppError::NotFound("Project not found".into()));
    }

    let conversations = db::list_conversations_by_project(&state.pool, &project_id).await?;
    Ok(Json(conversations))
}

#[utoipa::path(
    post,
    path = "/projects/{projectId}/conversations",
    params(("projectId" = String, Path, description = "Project ID")),
    request_body = db::CreateConversationRequest,
    responses(
        (status = 201, description = "Created conversation", body = db::Conversation),
        (status = 404, description = "Project not found")
    ),
    tag = "conversations"
)]
pub async fn create_conversation(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
    Json(req): Json<db::CreateConversationRequest>,
) -> ApiResult<(StatusCode, Json<db::Conversation>)> {
    if project_db::get_project(&state.pool, &project_id)
        .await?
        .is_none()
    {
        return Err(AppError::NotFound("Project not found".into()));
    }

    let conversation =
        db::create_conversation(&state.pool, &project_id, req.title.as_deref()).await?;

    Ok((StatusCode::CREATED, Json(conversation)))
}

#[utoipa::path(
    get,
    path = "/projects/{projectId}/conversations/{id}",
    params(
        ("projectId" = String, Path, description = "Project ID"),
        ("id" = String, Path, description = "Conversation ID"),
    ),
    responses(
        (status = 200, description = "Conversation found", body = db::Conversation),
        (status = 404, description = "Conversation not found")
    ),
    tag = "conversations"
)]
pub async fn get_conversation(
    State(state): State<AppState>,
    Path((_project_id, id)): Path<(String, String)>,
) -> ApiResult<Json<db::Conversation>> {
    let conversation = db::get_conversation(&state.pool, &id)
        .await?
        .ok_or_else(|| AppError::NotFound("Conversation not found".into()))?;
    Ok(Json(conversation))
}

#[utoipa::path(
    delete,
    path = "/projects/{projectId}/conversations/{id}",
    params(
        ("projectId" = String, Path, description = "Project ID"),
        ("id" = String, Path, description = "Conversation ID"),
    ),
    responses(
        (status = 204, description = "Conversation deleted"),
        (status = 404, description = "Conversation not found")
    ),
    tag = "conversations"
)]
pub async fn delete_conversation(
    State(state): State<AppState>,
    Path((_project_id, id)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    // A root conversation's task tree must not outlive it (ADR 0002 §6.2):
    // stop every ACTIVE subagent first and wait for their settlement
    // (bounded), so the cascading row deletion below never races a live
    // child's persistence. Background bash tasks get the same treatment
    // (ADR 0003) — plus their output directory, which the DB cascade
    // cannot reach.
    super::subagents::cancel_children_of_root(&state, &id).await?;
    state
        .runs
        .background
        .cancel_tasks_for_conversation(&id)
        .await;

    let deleted = db::delete_conversation(&state.pool, &id).await?;
    if deleted {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound("Conversation not found".into()))
    }
}

#[utoipa::path(
    patch,
    path = "/projects/{projectId}/conversations/{id}",
    params(
        ("projectId" = String, Path, description = "Project ID"),
        ("id" = String, Path, description = "Conversation ID"),
    ),
    request_body = db::UpdateConversationRequest,
    responses(
        (status = 200, description = "Updated (title and/or permission mode)"),
        (status = 400, description = "No field to update, or an unknown permission mode"),
        (status = 404, description = "Conversation not found")
    ),
    tag = "conversations"
)]
pub async fn update_conversation(
    State(state): State<AppState>,
    Path((_project_id, id)): Path<(String, String)>,
    Json(req): Json<db::UpdateConversationRequest>,
) -> ApiResult<StatusCode> {
    // Either field may be updated, both at once included. A mode change is
    // allowed while a run is active BY DESIGN (ADR 0004 §1.5: per-call
    // immediate effect) — the live gates of the conversation are flipped
    // below so the very next tool call of the running turn sees it.
    let mut updated = false;
    let mut found = true;

    if let Some(title) = req.title.as_deref() {
        if db::update_conversation_title(&state.pool, &id, title).await? {
            updated = true;
        } else {
            found = false;
        }
    }

    if let Some(mode_str) = req.permission_mode.as_deref() {
        let mode = crate::permissions::evaluator::PermissionMode::from_str_opt(mode_str)
            .ok_or_else(|| {
                AppError::BadRequest(format!(
                    "Invalid permission_mode: '{mode_str}' (expected one of: default, accept_edits, bypass)"
                ))
            })?;
        if db::update_conversation_permission_mode(&state.pool, &id, mode.as_str()).await? {
            updated = true;
            state.runs.set_conversation_permission_mode(&id, mode);
        } else {
            found = false;
        }
    }

    if !updated {
        if found {
            return Err(AppError::BadRequest(
                "Nothing to update: provide title and/or permission_mode".into(),
            ));
        }
        return Err(AppError::NotFound("Conversation not found".into()));
    }
    Ok(StatusCode::OK)
}
