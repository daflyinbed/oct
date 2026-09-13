use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;

use super::AppState;
use super::error::{AppError, ApiResult};
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
        (status = 200, description = "Title updated"),
        (status = 404, description = "Conversation not found")
    ),
    tag = "conversations"
)]
pub async fn update_conversation(
    State(state): State<AppState>,
    Path((_project_id, id)): Path<(String, String)>,
    Json(req): Json<db::UpdateConversationRequest>,
) -> ApiResult<StatusCode> {
    let title = req
        .title
        .as_deref()
        .ok_or_else(|| AppError::BadRequest("title is required".into()))?;

    let updated = db::update_conversation_title(&state.pool, &id, title).await?;
    if updated {
        Ok(StatusCode::OK)
    } else {
        Err(AppError::NotFound("Conversation not found".into()))
    }
}
