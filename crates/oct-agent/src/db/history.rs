//! Forks share immutable content, but own their timeline and runtime state.
use std::collections::{HashMap, HashSet};

use oct_llm_provider::core::ContentPart;
use serde::{Deserialize, Serialize};
use sqlx::{SqliteConnection, SqlitePool};
use utoipa::ToSchema;

use super::{
    conversations::Conversation,
    messages::{self, StoredMessage},
};
use crate::{
    context::details_kind,
    error::{AppError, AppResult},
};

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
}

pub async fn replay_revert(
    pool: &SqlitePool,
    source: &str,
    request: &RevertRequest,
) -> AppResult<Option<RevertResponse>> {
    let mut connection = pool.acquire().await.map_err(anyhow::Error::from)?;
    replay_revert_in_tx(&mut connection, source, request).await
}

async fn replay_revert_in_tx(
    connection: &mut SqliteConnection,
    source: &str,
    request: &RevertRequest,
) -> AppResult<Option<RevertResponse>> {
    let row = sqlx::query!("SELECT source_conversation_id, backup_conversation_id, request_json, response_json FROM conversation_revert_operations WHERE id = $1", request.operation_id)
        .fetch_optional(connection).await.map_err(anyhow::Error::from)?;
    let Some(row) = row else { return Ok(None) };
    let previous: RevertRequest =
        serde_json::from_str(&row.request_json).map_err(anyhow::Error::from)?;
    if row.source_conversation_id != source || previous != *request {
        return Err(AppError::OperationConflict(
            "operation_id was already used for another revert request".into(),
        ));
    }
    if row.backup_conversation_id.is_none() {
        return Err(AppError::OperationConflict(
            "The backup created by this operation has been deleted".into(),
        ));
    }
    Ok(Some(
        serde_json::from_str(&row.response_json).map_err(anyhow::Error::from)?,
    ))
}

/// Caller holds lifecycle admission and source occupancy. All writes, including
/// the full backup and operation receipt, commit or roll back together.
pub async fn revert(
    pool: &SqlitePool,
    source: &Conversation,
    request: &RevertRequest,
) -> AppResult<RevertResponse> {
    // Retrying the entire transaction revalidates the revision and cut.
    for attempt in 0..4 {
        match revert_once(pool, source, request).await {
            Err(AppError::Internal(error))
                if attempt < 3
                    && error
                        .downcast_ref::<sqlx::Error>()
                        .is_some_and(super::is_busy) =>
            {
                tokio::time::sleep(std::time::Duration::from_millis(25 << attempt)).await;
            }
            result => return result,
        }
    }
    unreachable!()
}

async fn revert_once(
    pool: &SqlitePool,
    source: &Conversation,
    request: &RevertRequest,
) -> AppResult<RevertResponse> {
    let mut tx = pool
        .begin_with("BEGIN IMMEDIATE")
        .await
        .map_err(anyhow::Error::from)?;
    if let Some(response) = replay_revert_in_tx(&mut tx, &source.id, request).await? {
        return Ok(response);
    }
    let source = super::conversations::get_conversation_in_tx(&mut tx, &source.id)
        .await
        .map_err(anyhow::Error::from)?
        .ok_or_else(|| AppError::NotFound("Conversation not found".into()))?;
    source.ensure_writable()?;
    let revision = sqlx::query_scalar!(
        "SELECT context_revision FROM conversations WHERE id = $1",
        source.id
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(anyhow::Error::from)?;
    if revision != request.expected_revision {
        return Err(AppError::StaleRevision(
            "Conversation history changed; refresh before reverting".into(),
        ));
    }
    // No catch-up/repair here: pending work must settle on the existing timeline.
    let pending = sqlx::query_scalar!("SELECT EXISTS(SELECT 1 FROM background_tasks WHERE conversation_id = $1 AND (status = 'running' OR notified = 0)) OR EXISTS(SELECT 1 FROM wake_intents w LEFT JOIN agent_runs r ON r.id = w.run_id WHERE w.conversation_id = $1 AND (r.status IS NULL OR r.status NOT IN ('completed', 'cancelled'))) AS \"pending!: bool\"", source.id)
        .fetch_one(&mut *tx).await.map_err(anyhow::Error::from)?;
    if pending {
        return Err(AppError::BackgroundPending(
            "请先完成或停止后台任务，并处理待投递通知或唤醒轮次，再回退对话。".into(),
        ));
    }
    if matches!(request.cut, ForkCut::Head) {
        return Err(invalid("Revert requires a historical message cut"));
    }
    let rows = messages::list_messages_in_tx(&mut tx, &source.id).await?;
    let normalized = normalize(&mut tx, &rows, revision, &request.cut).await?;
    if normalized.prefix.len() == rows.len()
        && normalized
            .prefix
            .iter()
            .zip(&rows)
            .all(|(a, b)| a.id == b.id)
    {
        return Err(invalid(
            "The selected cut does not change conversation history",
        ));
    }
    let title = format!(
        "{}（回退前 · {}）",
        source.title,
        chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC")
    );
    let mut backup =
        super::conversations::create_conversation_in_tx(&mut tx, &source.project_id, Some(&title))
            .await
            .map_err(anyhow::Error::from)?;
    sqlx::query!("UPDATE conversations SET origin_kind = 'revert_backup', permission_mode = $1, context_revision = $2, backup_of_conversation_id = $3, backup_at_revision = $2 WHERE id = $4", source.permission_mode, revision, source.id, backup.id)
        .execute(&mut *tx).await.map_err(anyhow::Error::from)?;
    backup.origin_kind = "revert_backup".into();
    backup.permission_mode = source.permission_mode.clone();
    backup.backup_of_conversation_id = Some(source.id.clone());
    backup.backup_at_revision = Some(revision);
    copy_membership(&mut tx, &backup.id, &rows).await?;
    sqlx::query!(
        "DELETE FROM conversation_messages WHERE conversation_id = $1",
        source.id
    )
    .execute(&mut *tx)
    .await
    .map_err(anyhow::Error::from)?;
    copy_membership(&mut tx, &source.id, &normalized.prefix).await?;
    sqlx::query!(
        "DELETE FROM wake_intents WHERE conversation_id = $1",
        source.id
    )
    .execute(&mut *tx)
    .await
    .map_err(anyhow::Error::from)?;
    let now = chrono::Utc::now().naive_utc();
    sqlx::query!("UPDATE conversations SET context_revision = context_revision + 1, history_epoch = history_epoch + 1, updated_at = $1 WHERE id = $2", now, source.id)
        .execute(&mut *tx).await.map_err(anyhow::Error::from)?;
    let mut conversation = source;
    conversation.history_epoch += 1;
    conversation.updated_at = now;
    let response = RevertResponse {
        conversation,
        context_revision: revision + 1,
        backup_conversation_id: backup.id.clone(),
        backup,
        cut: request.cut.clone(),
        draft: normalized.draft,
    };
    let request_json = serde_json::to_string(request).map_err(anyhow::Error::from)?;
    let response_json = serde_json::to_string(&response).map_err(anyhow::Error::from)?;
    sqlx::query!("INSERT INTO conversation_revert_operations (id, source_conversation_id, backup_conversation_id, request_json, response_json) VALUES ($1, $2, $3, $4, $5)", request.operation_id, response.conversation.id, response.backup_conversation_id, request_json, response_json)
        .execute(&mut *tx).await.map_err(anyhow::Error::from)?;
    tx.commit().await.map_err(anyhow::Error::from)?;
    Ok(response)
}

async fn copy_membership(
    tx: &mut SqliteConnection,
    id: &str,
    rows: &[StoredMessage],
) -> AppResult<()> {
    for (index, row) in rows.iter().enumerate() {
        let ordering = index as i64 + 1;
        sqlx::query!("INSERT INTO conversation_messages (conversation_id, message_id, ordering, appended_at_revision, swapped_at_revision) VALUES ($1, $2, $3, $4, $5)", id, row.id, ordering, row.appended_at_revision, row.swapped_at_revision)
            .execute(&mut *tx).await.map_err(anyhow::Error::from)?;
    }
    Ok(())
}

pub async fn visible_call_ids(pool: &SqlitePool, id: &str) -> anyhow::Result<HashSet<String>> {
    let rows = messages::list_messages(pool, id).await?;
    let mut calls = HashSet::new();
    for row in rows {
        for part in row.to_message()?.parts {
            if let ContentPart::ToolCall(call) = part {
                calls.insert(call.id);
            }
        }
    }
    Ok(calls)
}

fn invalid(message: &str) -> AppError {
    AppError::InvalidCut(message.into())
}

/// Only real user prompts and complete assistant replies offer message cuts.
pub fn message_cut(row: &StoredMessage) -> Option<ForkCut> {
    let message = row.to_message().ok()?;
    let side = match row.role.as_str() {
        "user" if details_kind(row.details_json.as_deref()).is_none() => ForkSide::Before,
        "assistant"
            if !message
                .parts
                .iter()
                .any(|part| matches!(part, ContentPart::ToolCall(_))) =>
        {
            ForkSide::After
        }
        _ => return None,
    };
    Some(ForkCut::Message {
        message_id: row.id.clone(),
        side,
    })
}

/// A stopped tool round is a stable cut only after EVERY result has landed.
/// The DTO names the final result row; the UI attaches it to its assistant card.
pub fn tool_cuts(rows: &[StoredMessage]) -> HashMap<String, ForkCut> {
    let mut validator = PrefixValidator::default();
    let mut cuts = HashMap::new();
    for row in rows {
        if validator.append(row).is_err() {
            break;
        }
        if row.role == "tool" && validator.pending.is_empty() {
            cuts.insert(
                row.id.clone(),
                ForkCut::Message {
                    message_id: row.id.clone(),
                    side: ForkSide::After,
                },
            );
        }
    }
    cuts
}

pub async fn replay(
    pool: &SqlitePool,
    source: &str,
    request: &ForkRequest,
) -> AppResult<Option<ForkResponse>> {
    let mut connection = pool.acquire().await.map_err(anyhow::Error::from)?;
    replay_in_tx(&mut connection, source, request).await
}

async fn replay_in_tx(
    connection: &mut SqliteConnection,
    source: &str,
    request: &ForkRequest,
) -> AppResult<Option<ForkResponse>> {
    let row = sqlx::query!("SELECT source_conversation_id, result_conversation_id, request_json, response_json FROM conversation_fork_operations WHERE id = $1", request.operation_id)
        .fetch_optional(connection).await.map_err(anyhow::Error::from)?;
    let Some(row) = row else { return Ok(None) };
    let previous: ForkRequest =
        serde_json::from_str(&row.request_json).map_err(anyhow::Error::from)?;
    if row.source_conversation_id != source || previous != *request {
        return Err(AppError::OperationConflict(
            "operation_id was already used for another fork request".into(),
        ));
    }
    if row.result_conversation_id.is_none() {
        return Err(AppError::OperationConflict(
            "The fork created by this operation has been deleted".into(),
        ));
    }
    Ok(Some(
        serde_json::from_str(&row.response_json).map_err(anyhow::Error::from)?,
    ))
}

/// Caller holds source occupancy and lifecycle admission. BEGIN IMMEDIATE
/// gives normalization and membership insertion one consistent write snapshot.
pub async fn fork(
    pool: &SqlitePool,
    source: &Conversation,
    request: &ForkRequest,
) -> AppResult<ForkResponse> {
    let mut tx = pool
        .begin_with("BEGIN IMMEDIATE")
        .await
        .map_err(anyhow::Error::from)?;
    if let Some(response) = replay_in_tx(&mut tx, &source.id, request).await? {
        return Ok(response);
    }
    let revision = sqlx::query_scalar!(
        "SELECT context_revision FROM conversations WHERE id = $1",
        source.id
    )
    .fetch_optional(&mut *tx)
    .await
    .map_err(anyhow::Error::from)?
    .ok_or_else(|| AppError::NotFound("Conversation not found".into()))?;
    if revision != request.expected_revision {
        return Err(AppError::StaleRevision(
            "Conversation history changed; refresh it before choosing a fork point".into(),
        ));
    }
    let rows = messages::list_messages_in_tx(&mut tx, &source.id).await?;
    let NormalizedHistory {
        prefix,
        cut_revision,
        draft,
        target,
    } = normalize(&mut tx, &rows, revision, &request.cut).await?;
    let title = request
        .title
        .clone()
        .unwrap_or_else(|| format!("{}（分支）", source.title));
    let mut conversation =
        super::conversations::create_conversation_in_tx(&mut tx, &source.project_id, Some(&title))
            .await?;
    sqlx::query!("UPDATE conversations SET origin_kind = 'fork', permission_mode = $1, context_revision = $2, forked_from_conversation_id = $3, forked_at_message_id = $4, forked_at_revision = $2 WHERE id = $5", source.permission_mode, cut_revision, source.id, target, conversation.id)
        .execute(&mut *tx).await.map_err(anyhow::Error::from)?;
    conversation.origin_kind = "fork".into();
    conversation.permission_mode = source.permission_mode.clone();
    conversation.forked_from_conversation_id = Some(source.id.clone());
    conversation.forked_at_message_id = target;
    conversation.forked_at_revision = Some(cut_revision);
    // Dense positive slots remain compatible with compaction parking.
    copy_membership(&mut tx, &conversation.id, &prefix).await?;
    let response = ForkResponse {
        conversation,
        context_revision: cut_revision,
        cut: request.cut.clone(),
        draft,
    };
    let request_json = serde_json::to_string(request).map_err(anyhow::Error::from)?;
    let response_json = serde_json::to_string(&response).map_err(anyhow::Error::from)?;
    sqlx::query!("INSERT INTO conversation_fork_operations (id, source_conversation_id, result_conversation_id, request_json, response_json) VALUES ($1, $2, $3, $4, $5)", request.operation_id, source.id, response.conversation.id, request_json, response_json)
        .execute(&mut *tx).await.map_err(anyhow::Error::from)?;
    tx.commit().await.map_err(anyhow::Error::from)?;
    Ok(response)
}

struct NormalizedHistory {
    prefix: Vec<StoredMessage>,
    cut_revision: i64,
    draft: Option<String>,
    target: Option<String>,
}

/// Shared by fork and revert: restore the historical prefix without mutating content.
async fn normalize(
    tx: &mut SqliteConnection,
    rows: &[StoredMessage],
    revision: i64,
    cut: &ForkCut,
) -> AppResult<NormalizedHistory> {
    let (length, cut_revision, draft, target) = match cut {
        ForkCut::Head => {
            if rows
                .last()
                .is_some_and(|row| message_cut(row).is_none() && row.role != "tool")
            {
                return Err(invalid(
                    "The conversation does not end at a stable message or tool result boundary",
                ));
            }
            (rows.len(), revision, None, None)
        }
        ForkCut::Message { message_id, side } => {
            let index = rows
                .iter()
                .position(|row| &row.id == message_id)
                .ok_or_else(|| {
                    AppError::NotFound("Fork message does not belong to this conversation".into())
                })?;
            let row = &rows[index];
            if message_cut(row).as_ref() != Some(cut)
                && !(row.role == "tool" && *side == ForkSide::After)
            {
                return Err(invalid("Unsupported fork point"));
            }
            match side {
                ForkSide::Before => {
                    let text = row
                        .to_message()?
                        .parts
                        .iter()
                        .filter_map(|part| match part {
                            ContentPart::Text(text) => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    (
                        index,
                        row.appended_at_revision - 1,
                        Some(text),
                        Some(message_id.clone()),
                    )
                }
                ForkSide::After => (
                    index + 1,
                    row.appended_at_revision,
                    None,
                    Some(message_id.clone()),
                ),
            }
        }
    };
    let mut prefix = Vec::new();
    for row in rows
        .iter()
        .cloned()
        .take(length)
        .filter(|row| row.appended_at_revision <= cut_revision)
    {
        let mut row = row;
        if row
            .swapped_at_revision
            .is_some_and(|swapped| swapped > cut_revision)
        {
            let original = row
                .supersedes_message_id
                .as_deref()
                .ok_or_else(|| invalid("Missing copy-on-write original"))?;
            let content = sqlx::query!("SELECT id, role, parts_json, details_json, provider_id, model_id, input_tokens, output_tokens, reasoning_tokens, supersedes_message_id, created_at FROM messages WHERE id = $1", original)
                .fetch_optional(&mut *tx).await.map_err(anyhow::Error::from)?
                .ok_or_else(|| invalid("Missing copy-on-write original"))?;
            if content.supersedes_message_id.is_some() {
                return Err(invalid("Invalid copy-on-write chain"));
            }
            row.id = content.id;
            row.role = content.role;
            row.parts_json = content.parts_json;
            row.details_json = content.details_json;
            row.provider_id = content.provider_id;
            row.model_id = content.model_id;
            row.input_tokens = content.input_tokens;
            row.output_tokens = content.output_tokens;
            row.reasoning_tokens = content.reasoning_tokens;
            row.created_at = content.created_at;
            row.supersedes_message_id = None;
            row.swapped_at_revision = None;
        }
        prefix.push(row);
    }
    validate_prefix(&prefix)?;
    Ok(NormalizedHistory {
        prefix,
        cut_revision,
        draft,
        target,
    })
}

fn validate_prefix(rows: &[StoredMessage]) -> AppResult<()> {
    let mut validator = PrefixValidator::default();
    for row in rows {
        validator.append(row)?;
    }
    if !validator.pending.is_empty() {
        return Err(invalid("Fork point splits a tool call and its result"));
    }
    Ok(())
}

#[derive(Default)]
struct PrefixValidator {
    pending: HashSet<String>,
    seen: HashSet<String>,
}

impl PrefixValidator {
    fn append(&mut self, row: &StoredMessage) -> AppResult<()> {
        let message = row
            .to_message()
            .map_err(|_| invalid("Malformed message in fork history"))?;
        if row.role == "tool"
            && (message.parts.is_empty()
                || message
                    .parts
                    .iter()
                    .any(|part| !matches!(part, ContentPart::ToolResult(_))))
        {
            return Err(invalid("Malformed tool result in fork history"));
        }
        if row.role != "tool" && !self.pending.is_empty() {
            return Err(invalid("Fork history contains unfinished tool calls"));
        }
        for part in message.parts {
            match part {
                ContentPart::ToolCall(call) => {
                    if row.role != "assistant" || !self.seen.insert(call.id.clone()) {
                        return Err(invalid("Invalid tool call in fork history"));
                    }
                    self.pending.insert(call.id);
                }
                ContentPart::ToolResult(result) => {
                    if row.role != "tool" || !self.pending.remove(&result.call_id) {
                        return Err(invalid("Unpaired tool result in fork history"));
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
}
