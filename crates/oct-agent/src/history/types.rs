//! Values used by history rules and durable operation receipts. No SQL or HTTP.
use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::attachment_types::AttachmentDto;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HistoryCut {
    Head,
    Message {
        message_id: String,
        side: HistorySide,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistorySide {
    Before,
    After,
}

/// The serde shapes are also the persisted request receipt format.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForkRequest {
    pub operation_id: String,
    pub expected_revision: i64,
    pub cut: HistoryCut,
    pub title: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RevertRequest {
    pub operation_id: String,
    pub expected_revision: i64,
    pub cut: HistoryCut,
}

/// Only immutable content needed by cut validation. Provider metadata, usage
/// and creation times remain on the existing content row selected by its id.
#[derive(Debug, Clone)]
pub struct HistoryContent {
    pub id: String,
    pub role: String,
    pub parts_json: String,
    pub details_json: Option<String>,
    pub supersedes_message_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct HistoryEntry {
    pub content: HistoryContent,
    pub appended_at_revision: i64,
    pub swapped_at_revision: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct HistorySnapshot {
    pub revision: i64,
    pub entries: Vec<HistoryEntry>,
    /// Only originals needed by the selected cut are loaded before planning,
    /// in the same transaction as memberships. Missing/invalid originals are
    /// policy errors only when the selected cut actually needs to restore them.
    pub originals: HashMap<String, HistoryContent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryMembership {
    pub message_id: String,
    pub appended_at_revision: i64,
    pub swapped_at_revision: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct HistoryPlan {
    pub prefix: Vec<HistoryMembership>,
    pub cut_revision: i64,
    pub draft: Option<String>,
    pub draft_attachments: Vec<AttachmentDto>,
    pub target: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum HistoryPolicyError {
    #[error("{0}")]
    InvalidCut(String),
    #[error("Fork message does not belong to this conversation")]
    MessageNotFound,
    #[error(transparent)]
    MalformedContent(#[from] anyhow::Error),
}
