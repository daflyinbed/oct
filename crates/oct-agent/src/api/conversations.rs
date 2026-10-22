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
    let _admission = state.runs.lifecycle.admission.read().await;
    state.runs.lifecycle.check("project", &project_id)?;
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
    let _admission = state.runs.lifecycle.admission.read().await;
    state.runs.lifecycle.check("project", &project_id)?;
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
    Path((project_id, id)): Path<(String, String)>,
) -> ApiResult<Json<db::Conversation>> {
    let conversation = db::get_conversation(&state.pool, &id)
        .await?
        .ok_or_else(|| AppError::NotFound("Conversation not found".into()))?;
    if conversation.project_id != project_id {
        return Err(AppError::NotFound(
            "Conversation not found in project".into(),
        ));
    }
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
    Path((project_id, id)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    let conversation = db::get_conversation(&state.pool, &id)
        .await?
        .filter(|c| c.project_id == project_id)
        .ok_or_else(|| AppError::NotFound("Conversation not found in project".into()))?;
    if conversation.kind != "chat" {
        return Err(AppError::ManagedConversation(
            "Delete managed conversations through their parent".into(),
        ));
    }
    let deleted = state.runs.delete_conversation(&id).await?;
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
    Path((project_id, id)): Path<(String, String)>,
    Json(req): Json<db::UpdateConversationRequest>,
) -> ApiResult<StatusCode> {
    let conversation = db::get_conversation(&state.pool, &id)
        .await?
        .filter(|c| c.project_id == project_id)
        .ok_or_else(|| AppError::NotFound("Conversation not found in project".into()))?;
    if req.permission_mode.is_some() {
        conversation.ensure_writable()?;
    }
    let mode = req
        .permission_mode
        .as_deref()
        .map(|value| {
            crate::permissions::evaluator::PermissionMode::from_str_opt(value)
                .ok_or_else(|| AppError::BadRequest(format!("Invalid permission_mode: '{value}'")))
        })
        .transpose()?;
    if req.title.is_none() && mode.is_none() {
        return Err(AppError::BadRequest("Nothing to update".into()));
    }
    db::update_conversation(
        &state.pool,
        &id,
        req.title.as_deref(),
        mode.map(|m| m.as_str()),
    )
    .await?;
    if let Some(mode) = mode {
        state.runs.set_conversation_permission_mode(&id, mode);
    }
    Ok(StatusCode::OK)
}
