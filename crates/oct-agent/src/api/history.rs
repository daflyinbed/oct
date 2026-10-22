//! History transport DTOs and translation to application commands.
use super::{AppState, error::ApiResult};
use crate::application::history::HistoryApplication;
use crate::attachment_types::AttachmentDto;
use crate::db::{conversations::Conversation, messages::StoredMessage};
use crate::history::{self as domain, HistoryEntry, policy};
use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use utoipa::ToSchema;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ForkCut {
    Head,
    Message { message_id: String, side: ForkSide },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ForkSide {
    Before,
    After,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ForkRequest {
    pub operation_id: String,
    pub expected_revision: i64,
    pub cut: ForkCut,
    pub title: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ForkResponse {
    pub conversation: Conversation,
    pub context_revision: i64,
    pub cut: ForkCut,
    pub draft: Option<String>,
    /// Sent source attachments to copy into the destination's staging queue.
    #[serde(default)]
    pub draft_attachments: Vec<AttachmentDto>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct RevertRequest {
    pub operation_id: String,
    pub expected_revision: i64,
    pub cut: ForkCut,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RevertResponse {
    pub conversation: Conversation,
    pub context_revision: i64,
    pub backup_conversation_id: String,
    pub backup: Conversation,
    pub cut: ForkCut,
    pub draft: Option<String>,
    #[serde(default)]
    pub draft_attachments: Vec<AttachmentDto>,
}

#[utoipa::path(
    post, path = "/conversations/{id}/revert",
    params(("id" = String, Path, description = "Conversation ID")),
    request_body = RevertRequest,
    responses(
        (status = 200, description = "History reverted with a read-only backup", body = RevertResponse),
        (status = 400, description = "Invalid cut or no change", body = super::error::ErrorBody),
        (status = 404, description = "Conversation or cut not found", body = super::error::ErrorBody),
        (status = 409, description = "Busy, pending work, read-only, stale revision or operation conflict", body = super::error::ErrorBody)
    ), tag = "conversations"
)]
pub async fn revert_conversation(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<RevertRequest>,
) -> ApiResult<Json<RevertResponse>> {
    let response = HistoryApplication::new(state.runs)
        .revert(&id, request.into())
        .await?;
    Ok(Json(response.into()))
}

#[utoipa::path(
    post,
    path = "/conversations/{id}/fork",
    params(("id" = String, Path, description = "Source conversation ID")),
    request_body = ForkRequest,
    responses(
        (status = 201, description = "Fork created (or replayed)", body = ForkResponse),
        (status = 400, description = "Invalid cut or request", body = super::error::ErrorBody),
        (status = 404, description = "Source or cut not found", body = super::error::ErrorBody),
        (status = 409, description = "Busy, stale revision or operation conflict", body = super::error::ErrorBody)
    ),
    tag = "conversations"
)]
pub async fn fork_conversation(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<ForkRequest>,
) -> ApiResult<(StatusCode, Json<ForkResponse>)> {
    let response = HistoryApplication::new(state.runs)
        .fork(&id, request.into())
        .await?;
    Ok((StatusCode::CREATED, Json(response.into())))
}

impl From<ForkCut> for domain::HistoryCut {
    fn from(cut: ForkCut) -> Self {
        match cut {
            ForkCut::Head => Self::Head,
            ForkCut::Message { message_id, side } => Self::Message {
                message_id,
                side: side.into(),
            },
        }
    }
}
impl From<domain::HistoryCut> for ForkCut {
    fn from(cut: domain::HistoryCut) -> Self {
        match cut {
            domain::HistoryCut::Head => Self::Head,
            domain::HistoryCut::Message { message_id, side } => Self::Message {
                message_id,
                side: side.into(),
            },
        }
    }
}
impl From<ForkSide> for domain::HistorySide {
    fn from(side: ForkSide) -> Self {
        match side {
            ForkSide::Before => Self::Before,
            ForkSide::After => Self::After,
        }
    }
}
impl From<domain::HistorySide> for ForkSide {
    fn from(side: domain::HistorySide) -> Self {
        match side {
            domain::HistorySide::Before => Self::Before,
            domain::HistorySide::After => Self::After,
        }
    }
}
impl From<ForkRequest> for domain::ForkRequest {
    fn from(request: ForkRequest) -> Self {
        Self {
            operation_id: request.operation_id,
            expected_revision: request.expected_revision,
            cut: request.cut.into(),
            title: request.title,
        }
    }
}
impl From<RevertRequest> for domain::RevertRequest {
    fn from(request: RevertRequest) -> Self {
        Self {
            operation_id: request.operation_id,
            expected_revision: request.expected_revision,
            cut: request.cut.into(),
        }
    }
}
impl From<crate::db::history::ForkResponse> for ForkResponse {
    fn from(result: crate::db::history::ForkResponse) -> Self {
        Self {
            conversation: result.conversation,
            context_revision: result.context_revision,
            cut: result.cut.into(),
            draft: result.draft,
            draft_attachments: result.draft_attachments,
        }
    }
}
impl From<crate::db::history::RevertResponse> for RevertResponse {
    fn from(result: crate::db::history::RevertResponse) -> Self {
        Self {
            conversation: result.conversation,
            context_revision: result.context_revision,
            backup_conversation_id: result.backup_conversation_id,
            backup: result.backup,
            cut: result.cut.into(),
            draft: result.draft,
            draft_attachments: result.draft_attachments,
        }
    }
}

/// Presentation adapters consume the same pure rules used by transaction plans.
pub(crate) fn message_cut(row: &StoredMessage) -> Option<ForkCut> {
    policy::message_cut(&HistoryEntry::from(row)).map(Into::into)
}
pub(crate) fn tool_cuts(rows: &[StoredMessage]) -> HashMap<String, ForkCut> {
    policy::tool_cuts(&rows.iter().map(HistoryEntry::from).collect::<Vec<_>>())
        .into_iter()
        .map(|(id, cut)| (id, cut.into()))
        .collect()
}
