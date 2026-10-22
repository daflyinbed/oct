//! Forks share immutable content, but own their timeline and runtime state.
use std::collections::HashSet;

use oct_llm_provider::core::ContentPart;
use serde::{Deserialize, Serialize};
use sqlx::{SqliteConnection, SqlitePool};

use super::{
    conversations::Conversation,
    messages::{self, StoredMessage},
};
use crate::{
    attachment_types::AttachmentDto,
    error::{AppError, AppResult},
};

// Compatibility exports for callers using the former history transport types.
// These request/cut values also preserve the durable receipt serde format.
pub use crate::history::{
    ForkRequest, HistoryCut as ForkCut, HistorySide as ForkSide, RevertRequest,
};
use crate::history::{
    HistoryContent, HistoryEntry, HistoryMembership, HistoryPlan, HistorySnapshot, policy,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForkResponse {
    pub conversation: Conversation,
    pub context_revision: i64,
    pub cut: ForkCut,
    pub draft: Option<String>,
    /// Sent source attachments to copy into the destination's staging queue.
    #[serde(default)]
    pub draft_attachments: Vec<AttachmentDto>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
        .await?
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
    let rows = messages::list_messages_in_tx(&mut tx, &source.id).await?;
    let snapshot = load_snapshot(&mut tx, &rows, revision, &request.cut).await?;
    let normalized = policy::plan_revert(&snapshot, &request.cut)?;
    let title = format!(
        "{}（回退前 · {}）",
        source.title,
        chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC")
    );
    let mut backup =
        super::conversations::create_conversation_in_tx(&mut tx, &source.project_id, Some(&title))
            .await?;
    sqlx::query!("UPDATE conversations SET origin_kind = 'revert_backup', permission_mode = $1, context_revision = $2, backup_of_conversation_id = $3, backup_at_revision = $2 WHERE id = $4", source.permission_mode, revision, source.id, backup.id)
        .execute(&mut *tx).await.map_err(anyhow::Error::from)?;
    backup.origin_kind = "revert_backup".into();
    backup.permission_mode = source.permission_mode.clone();
    backup.backup_of_conversation_id = Some(source.id.clone());
    backup.backup_at_revision = Some(revision);
    copy_membership(
        &mut tx,
        &backup.id,
        &rows.iter().map(HistoryMembership::from).collect::<Vec<_>>(),
    )
    .await?;
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
        draft_attachments: normalized.draft_attachments,
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
    rows: &[HistoryMembership],
) -> AppResult<()> {
    for (index, row) in rows.iter().enumerate() {
        let ordering = index as i64 + 1;
        sqlx::query!("INSERT INTO conversation_messages (conversation_id, message_id, ordering, appended_at_revision, swapped_at_revision) VALUES ($1, $2, $3, $4, $5)", id, row.message_id, ordering, row.appended_at_revision, row.swapped_at_revision)
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
    let snapshot = load_snapshot(&mut tx, &rows, revision, &request.cut).await?;
    let HistoryPlan {
        prefix,
        cut_revision,
        draft,
        draft_attachments,
        target,
    } = policy::plan_cut(&snapshot, &request.cut)?;
    let title = request
        .title
        .clone()
        .unwrap_or_else(|| format!("{}（分支）", source.title));
    let mut conversation =
        super::conversations::create_conversation_in_tx(&mut tx, &source.project_id, Some(&title))
            .await?;
    // 无显式标题时回落为 `default`：「（分支）」只是占位——分叉创建时内容与
    // 源对话完全一致，重新生成只会得到重复标题；等用户在分叉内发出第一条
    // 自己的消息（真正的分岔点）再由一次性标题生成接管。显式标题仍是
    // 用户命名（'user'），永不被覆盖。
    let auto_title_eligible = request.title.is_none();
    sqlx::query!("UPDATE conversations SET origin_kind = 'fork', permission_mode = $1, context_revision = $2, forked_from_conversation_id = $3, forked_at_message_id = $4, forked_at_revision = $2, title_source = CASE WHEN $6 THEN 'default' ELSE 'user' END WHERE id = $5", source.permission_mode, cut_revision, source.id, target, conversation.id, auto_title_eligible)
        .execute(&mut *tx).await.map_err(anyhow::Error::from)?;
    conversation.origin_kind = "fork".into();
    conversation.permission_mode = source.permission_mode.clone();
    conversation.title_source = if auto_title_eligible {
        "default".into()
    } else {
        "user".into()
    };
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
        draft_attachments,
    };
    let request_json = serde_json::to_string(request).map_err(anyhow::Error::from)?;
    let response_json = serde_json::to_string(&response).map_err(anyhow::Error::from)?;
    sqlx::query!("INSERT INTO conversation_fork_operations (id, source_conversation_id, result_conversation_id, request_json, response_json) VALUES ($1, $2, $3, $4, $5)", request.operation_id, source.id, response.conversation.id, request_json, response_json)
        .execute(&mut *tx).await.map_err(anyhow::Error::from)?;
    tx.commit().await.map_err(anyhow::Error::from)?;
    Ok(response)
}

/// Adapt storage rows at the repository boundary; pure rules never see sqlx
/// models or provider/usage columns irrelevant to membership selection.
impl From<&StoredMessage> for HistoryEntry {
    fn from(row: &StoredMessage) -> Self {
        Self {
            content: HistoryContent {
                id: row.id.clone(),
                role: row.role.clone(),
                parts_json: row.parts_json.clone(),
                details_json: row.details_json.clone(),
                supersedes_message_id: row.supersedes_message_id.clone(),
            },
            appended_at_revision: row.appended_at_revision,
            swapped_at_revision: row.swapped_at_revision,
        }
    }
}

impl From<&StoredMessage> for HistoryMembership {
    fn from(row: &StoredMessage) -> Self {
        Self {
            message_id: row.id.clone(),
            appended_at_revision: row.appended_at_revision,
            swapped_at_revision: row.swapped_at_revision,
        }
    }
}

/// Load only the COW originals needed by the cut, inside the same write transaction
/// that reads memberships, computes the plan, applies it and saves its receipt.
async fn load_snapshot(
    tx: &mut SqliteConnection,
    rows: &[StoredMessage],
    revision: i64,
    cut: &ForkCut,
) -> AppResult<HistorySnapshot> {
    let mut snapshot = HistorySnapshot {
        revision,
        entries: rows.iter().map(HistoryEntry::from).collect(),
        originals: Default::default(),
    };
    let originals = policy::original_ids_to_restore(&snapshot, cut)?;
    if !originals.is_empty() {
        let ids = serde_json::to_string(&originals).map_err(anyhow::Error::from)?;
        snapshot.originals = sqlx::query!("SELECT id, role, parts_json, details_json, supersedes_message_id FROM messages WHERE id IN (SELECT value FROM json_each($1))", ids)
            .fetch_all(&mut *tx).await.map_err(anyhow::Error::from)?
            .into_iter().map(|row| {
                let content = HistoryContent { id: row.id, role: row.role, parts_json: row.parts_json, details_json: row.details_json, supersedes_message_id: row.supersedes_message_id };
                (content.id.clone(), content)
            }).collect();
    }
    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oct_llm_provider::core::{Message, Role, ToolCall, ToolResult};
    use serde_json::json;

    async fn append(pool: &SqlitePool, id: &str, message: Message) -> StoredMessage {
        messages::insert_message(pool, id, &message, None, None, None)
            .await
            .unwrap()
    }

    fn tool_result(call_id: &str, content: &str) -> Message {
        Message::new(
            Role::Tool,
            vec![ContentPart::ToolResult(ToolResult {
                call_id: call_id.into(),
                content: json!(content),
                is_error: false,
                images: Vec::new(),
            })],
        )
    }

    #[tokio::test]
    async fn snapshot_loads_only_originals_restored_by_the_selected_cut() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", dir.path().join("history.db").display());
        let pool = crate::db::init_pool(&url).await.unwrap();
        let project = crate::db::projects::create_project(
            &pool,
            &crate::db::projects::CreateProjectRequest {
                name: "history".into(),
                working_dir: dir.path().display().to_string(),
                additional_dirs: None,
            },
        )
        .await
        .unwrap();
        let conversation =
            super::super::conversations::create_conversation(&pool, &project.id, None)
                .await
                .unwrap();
        let id = &conversation.id;
        let mut prompts = Vec::new();
        let mut originals = Vec::new();
        let mut answers = Vec::new();
        for call_id in ["first", "middle", "last"] {
            prompts.push(append(&pool, id, Message::text(Role::User, call_id)).await);
            append(
                &pool,
                id,
                Message::new(
                    Role::Assistant,
                    vec![ContentPart::ToolCall(ToolCall {
                        id: call_id.into(),
                        name: "read_file".into(),
                        arguments: "{}".into(),
                    })],
                ),
            )
            .await;
            originals.push(append(&pool, id, tool_result(call_id, &"x".repeat(50_000))).await);
            answers.push(append(&pool, id, Message::text(Role::Assistant, "done")).await);
            if call_id == "first" {
                // This swap already existed at the middle cut and must stay cleared.
                messages::apply_cow_swaps(
                    &pool,
                    id,
                    &[messages::CowSwap {
                        call_id: call_id.into(),
                        copy: tool_result(call_id, crate::agent::micro::PLACEHOLDER_CONTENT),
                        details: None,
                    }],
                )
                .await
                .unwrap();
            }
        }
        let swaps = ["middle", "last"].map(|call_id| messages::CowSwap {
            call_id: call_id.into(),
            copy: tool_result(call_id, crate::agent::micro::PLACEHOLDER_CONTENT),
            details: None,
        });
        messages::apply_cow_swaps(&pool, id, &swaps).await.unwrap();
        // Later markers may be ordered inside the historical prefix.
        let marker = messages::insert_message_at_ordering(
            &pool,
            id,
            &Message::text(Role::User, "summary"),
            None,
            None,
            Some(&json!({"kind": "compaction"})),
            answers[1].ordering,
        )
        .await
        .unwrap();
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
        let rows = messages::list_messages_in_tx(&mut tx, id).await.unwrap();
        let cases = [
            (ForkCut::Head, HashSet::new()),
            (
                ForkCut::Message {
                    message_id: answers[0].id.clone(),
                    side: ForkSide::After,
                },
                HashSet::from([originals[0].id.as_str()]),
            ),
            (
                ForkCut::Message {
                    message_id: prompts[1].id.clone(),
                    side: ForkSide::Before,
                },
                HashSet::new(),
            ),
            (
                ForkCut::Message {
                    message_id: answers[1].id.clone(),
                    side: ForkSide::After,
                },
                HashSet::from([originals[1].id.as_str()]),
            ),
            (
                ForkCut::Message {
                    message_id: prompts[2].id.clone(),
                    side: ForkSide::Before,
                },
                HashSet::from([originals[1].id.as_str()]),
            ),
        ];
        for (cut, expected) in cases {
            let snapshot = load_snapshot(&mut tx, &rows, marker.appended_at_revision, &cut)
                .await
                .unwrap();
            assert_eq!(
                snapshot
                    .originals
                    .keys()
                    .map(String::as_str)
                    .collect::<HashSet<_>>(),
                expected,
                "unneeded original content must not be materialized for {cut:?}"
            );
            policy::plan_cut(&snapshot, &cut).unwrap();
        }
        tx.rollback().await.unwrap();
    }
}
