use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use utoipa::ToSchema;

use super::AppState;
use crate::db::conversations as db;

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateConversationRequest {
    pub title: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateConversationTitleRequest {
    pub title: String,
}

/// List all conversations.
#[utoipa::path(
    get,
    path = "/conversations",
    responses(
        (status = 200, description = "List of conversations", body = Vec<db::Conversation>)
    ),
    tag = "conversations"
)]
pub async fn list_conversations(
    State(state): State<AppState>,
) -> Result<Json<Vec<db::Conversation>>, StatusCode> {
    let conversations = db::list_conversations(&state.pool)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(conversations))
}

/// Create a new conversation.
#[utoipa::path(
    post,
    path = "/conversations",
    request_body = CreateConversationRequest,
    responses(
        (status = 201, description = "Created conversation", body = db::Conversation)
    ),
    tag = "conversations"
)]
pub async fn create_conversation(
    State(state): State<AppState>,
    Json(req): Json<CreateConversationRequest>,
) -> Result<(StatusCode, Json<db::Conversation>), StatusCode> {
    let provider_spec = state
        .provider_spec
        .read()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .clone();

    let conversation = db::create_conversation(
        &state.pool,
        &state.working_dir.to_string_lossy(),
        &provider_spec,
        req.title.as_deref(),
    )
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok((StatusCode::CREATED, Json(conversation)))
}

/// Get a specific conversation.
#[utoipa::path(
    get,
    path = "/conversations/{id}",
    params(("id" = String, Path, description = "Conversation ID")),
    responses(
        (status = 200, description = "Conversation found", body = db::Conversation),
        (status = 404, description = "Conversation not found")
    ),
    tag = "conversations"
)]
pub async fn get_conversation(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<db::Conversation>, StatusCode> {
    let conversation = db::get_conversation(&state.pool, &id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(conversation))
}

/// Delete a conversation.
#[utoipa::path(
    delete,
    path = "/conversations/{id}",
    params(("id" = String, Path, description = "Conversation ID")),
    responses(
        (status = 204, description = "Conversation deleted"),
        (status = 404, description = "Conversation not found")
    ),
    tag = "conversations"
)]
pub async fn delete_conversation(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, StatusCode> {
    let deleted = db::delete_conversation(&state.pool, &id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    if deleted {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

/// Update a conversation's title.
#[utoipa::path(
    patch,
    path = "/conversations/{id}",
    params(("id" = String, Path, description = "Conversation ID")),
    request_body = UpdateConversationTitleRequest,
    responses(
        (status = 200, description = "Title updated"),
        (status = 404, description = "Conversation not found")
    ),
    tag = "conversations"
)]
pub async fn update_conversation_title(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateConversationTitleRequest>,
) -> Result<StatusCode, StatusCode> {
    let updated = db::update_conversation_title(&state.pool, &id, &req.title)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    if updated {
        Ok(StatusCode::OK)
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}
