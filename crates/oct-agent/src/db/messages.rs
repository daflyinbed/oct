use anyhow::Result;
use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::SqlitePool;
use sqlx::sqlite::SqliteConnection;
use std::collections::HashSet;
use utoipa::ToSchema;
use uuid::Uuid;

use oct_llm_provider::core::{ContentPart, Message, Role, ToolResult};

// The LLM view snapshot and its anchor type live in the neutral `context`
// module (shared with the agent loop); re-exported here for existing paths.
pub use crate::context::{COMPACTION_MARKER_KIND, LlmView, UsageAnchor};
use crate::context::{details_kind, estimate_message};

/// One stored message as seen from a conversation: the pure content row
/// (`messages`) joined with its timeline slot (`conversation_messages`).
/// Content rows are insert-only (usage backfill aside) and shared across
/// future forks; ordering and the revision stamps are per-conversation
/// membership state.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct StoredMessage {
    pub id: String,
    pub conversation_id: String,
    pub role: String,
    pub parts_json: String,
    /// UI-only tool-result metadata, serialized JSON. Never part of the
    /// model-visible message parts.
    pub details_json: Option<String>,
    pub ordering: i64,
    pub provider_id: Option<String>,
    pub model_id: Option<String>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub reasoning_tokens: Option<i64>,
    /// Copy-on-write back-pointer: non-NULL on a replacement copy, naming the
    /// original content row it superseded (always exactly one hop).
    pub supersedes_message_id: Option<String>,
    /// Conversation revision stamped when this row joined the timeline.
    /// Never changed afterwards — rollback/fork cut points and usage-anchor
    /// eligibility compare against it.
    pub appended_at_revision: i64,
    /// Conversation revision stamped when a copy-on-write swap replaced this
    /// slot's message; NULL while the slot still shows its original content.
    pub swapped_at_revision: Option<i64>,
    pub created_at: NaiveDateTime,
}

impl StoredMessage {
    pub fn to_message(&self) -> Result<Message> {
        let role = match self.role.as_str() {
            "system" => Role::System,
            "user" => Role::User,
            "assistant" => Role::Assistant,
            "tool" => Role::Tool,
            other => anyhow::bail!("Unknown role: {other}"),
        };
        let parts: Vec<ContentPart> = serde_json::from_str(&self.parts_json)?;
        Ok(Message { role, parts })
    }
}

fn role_to_string(role: &Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

// ======================================================================
// Row insertion (content row + membership, one transaction)
// ======================================================================

/// Everything needed to build the content-row half of a stored message.
struct RowFields {
    id: String,
    parts_json: String,
    details_json: Option<String>,
    now: NaiveDateTime,
}

fn build_row_fields(message: &Message, details: Option<&Value>) -> Result<RowFields> {
    Ok(RowFields {
        id: Uuid::new_v4().to_string(),
        parts_json: serde_json::to_string(&message.parts)?,
        details_json: match details {
            Some(value) => Some(serde_json::to_string(value)?),
            None => None,
        },
        now: chrono::Utc::now().naive_utc(),
    })
}

/// Bump the conversation's revision counter and return the new value. Every
/// membership mutation (append, insert-at-cut, COW swap) calls this inside
/// its transaction, so revisions strictly order every LLM-view change
/// independently of `ordering` (which marker insertion renumbers).
async fn bump_revision(tx: &mut SqliteConnection, conversation_id: &str) -> Result<i64> {
    let revision = sqlx::query_scalar!(
        "UPDATE conversations SET context_revision = context_revision + 1 WHERE id = $1 RETURNING context_revision",
        conversation_id,
    )
    .fetch_one(&mut *tx)
    .await?;
    Ok(revision)
}

/// Insert the content row and append a NEW timeline slot at `ordering`.
#[allow(clippy::too_many_arguments)]
async fn append_row(
    tx: &mut SqliteConnection,
    conversation_id: &str,
    message: &Message,
    provider_id: Option<&str>,
    model_id: Option<&str>,
    details: Option<&Value>,
    ordering: i64,
    revision: i64,
    supersedes: Option<&str>,
) -> Result<StoredMessage> {
    let f = build_row_fields(message, details)?;
    let role = role_to_string(&message.role);
    sqlx::query!(
        "INSERT INTO messages (id, role, parts_json, details_json, provider_id, model_id, supersedes_message_id, created_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        f.id,
        role,
        f.parts_json,
        f.details_json,
        provider_id,
        model_id,
        supersedes,
        f.now,
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "INSERT INTO conversation_messages (conversation_id, message_id, ordering, appended_at_revision, swapped_at_revision) VALUES ($1, $2, $3, $4, NULL)",
        conversation_id,
        f.id,
        ordering,
        revision,
    )
    .execute(&mut *tx)
    .await?;

    Ok(StoredMessage {
        id: f.id,
        conversation_id: conversation_id.to_string(),
        role: role_to_string(&message.role).to_string(),
        parts_json: f.parts_json,
        details_json: f.details_json,
        ordering,
        provider_id: provider_id.map(|s| s.to_string()),
        model_id: model_id.map(|s| s.to_string()),
        input_tokens: None,
        output_tokens: None,
        reasoning_tokens: None,
        supersedes_message_id: supersedes.map(|s| s.to_string()),
        appended_at_revision: revision,
        swapped_at_revision: None,
        created_at: f.now,
    })
}

/// The ordering `insert_message` would assign to the next append — MAX+1.
/// The degraded-append fallback for runs whose persist failed: keeps the
/// in-run context entries parallel with the DB instead of a blind last+1,
/// which could desync a later compaction marker's slot.
pub async fn next_ordering(pool: &SqlitePool, conversation_id: &str) -> Result<i64> {
    let ordering = sqlx::query_scalar!(
        "SELECT COALESCE(MAX(ordering), 0) + 1 FROM conversation_messages WHERE conversation_id = $1",
        conversation_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(ordering)
}

pub async fn insert_message(
    pool: &SqlitePool,
    conversation_id: &str,
    message: &Message,
    provider_id: Option<&str>,
    model_id: Option<&str>,
    details: Option<&Value>,
) -> Result<StoredMessage> {
    // The transaction reads (SELECT MAX) before writing, so a concurrent
    // writer committing in between surfaces as an immediate BUSY (plain or
    // the 517 snapshot-upgrade variant) that busy_timeout will not retry.
    // Root and subagent loops append to one database concurrently now;
    // retrying a fresh transaction is the standard resolution, and the
    // second attempt sees the committed ordering.
    const BUSY_ATTEMPTS: usize = 8;
    for attempt in 1..=BUSY_ATTEMPTS {
        let mut tx = pool.begin().await?;
        match insert_message_in_tx(
            &mut tx,
            conversation_id,
            message,
            provider_id,
            model_id,
            details,
        )
        .await
        {
            Ok(stored) => {
                tx.commit().await?;
                return Ok(stored);
            }
            Err(e)
                if e.downcast_ref::<sqlx::Error>()
                    .is_some_and(crate::db::is_busy)
                    && attempt < BUSY_ATTEMPTS =>
            {
                tx.rollback().await.ok();
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!("the loop returns on its final attempt")
}

/// [`insert_message`] inside a caller-owned transaction (the subagent
/// registration creates the child conversation, delegation message and run
/// row atomically).
pub async fn insert_message_in_tx(
    tx: &mut SqliteConnection,
    conversation_id: &str,
    message: &Message,
    provider_id: Option<&str>,
    model_id: Option<&str>,
    details: Option<&Value>,
) -> Result<StoredMessage> {
    let ordering = sqlx::query_scalar!(
        "SELECT COALESCE(MAX(ordering), 0) + 1 FROM conversation_messages WHERE conversation_id = $1",
        conversation_id,
    )
    .fetch_one(&mut *tx)
    .await?;
    let revision = bump_revision(tx, conversation_id).await?;
    let stored = append_row(
        tx,
        conversation_id,
        message,
        provider_id,
        model_id,
        details,
        ordering,
        revision,
        None,
    )
    .await?;
    Ok(stored)
}

/// Insert a message INTO the timeline at `ordering`, shifting every later
/// slot up by one. Used by compaction for its marker row: the marker must sit
/// BETWEEN the summarized region and the verbatim tail, or the tail would
/// fall before the marker and vanish from the LLM view on the next run.
///
/// The shift is a two-phase move inside one transaction: a single
/// `UPDATE ... ORDER BY` cannot be relied on (it needs the non-default
/// SQLITE_ENABLE_UPDATE_DELETE_LIMIT compile option, and SQLite documents
/// that ORDER BY does not govern row-modification order). Parking the
/// affected rows in a disjoint negative range first makes the move safe
/// under any scan order — parked values never collide with the untouched
/// non-negative ones, and the negative mapping is injective.
pub async fn insert_message_at_ordering(
    pool: &SqlitePool,
    conversation_id: &str,
    message: &Message,
    provider_id: Option<&str>,
    model_id: Option<&str>,
    details: Option<&Value>,
    ordering: i64,
) -> Result<StoredMessage> {
    let mut tx = pool.begin().await?;
    sqlx::query!(
        "UPDATE conversation_messages SET ordering = -ordering WHERE conversation_id = $1 AND ordering >= $2",
        conversation_id,
        ordering,
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "UPDATE conversation_messages SET ordering = -ordering + 1 WHERE conversation_id = $1 AND ordering < 0",
        conversation_id,
    )
    .execute(&mut *tx)
    .await?;
    let revision = bump_revision(&mut tx, conversation_id).await?;
    let stored = append_row(
        &mut tx,
        conversation_id,
        message,
        provider_id,
        model_id,
        details,
        ordering,
        revision,
        None,
    )
    .await?;
    tx.commit().await?;
    Ok(stored)
}

// ======================================================================
// Copy-on-write swaps (microcompact persistence)
// ======================================================================

/// One copy-on-write swap: `copy` is the pre-built replacement message
/// (placeholder content, same call_id/is_error, images cleared) that takes
/// over the timeline slot currently holding the tool result for `call_id`.
/// Call ids (not row ids) key the swap because the agent loop tracks
/// messages without their database identities.
pub struct CowSwap {
    pub call_id: String,
    pub copy: Message,
    pub details: Option<Value>,
}

/// A successfully applied swap: the call whose result row was replaced plus
/// the stored copy row (whose `supersedes_message_id` points back at the
/// original content).
pub struct AppliedCowSwap {
    pub call_id: String,
    pub copy: StoredMessage,
}

/// Apply a batch of COW swaps atomically: insert the copy content rows (each
/// carrying `supersedes_message_id` back at its original), bump the
/// conversation revision once, and repoint the membership slots at the
/// copies, stamping `swapped_at_revision`. The slot's `appended_at_revision`
/// is deliberately preserved — it records when the SLOT joined the timeline
/// (cut points compare against it), not when its current content did.
///
/// Call ids are resolved to their CURRENT tool-result slot in this
/// conversation (newest occurrence wins — ids are unique in practice). Swaps
/// whose call has no live result row are skipped, which makes re-runs
/// idempotent: after a swap the slot holds the placeholder-carrying copy,
/// which callers select away.
pub async fn apply_cow_swaps(
    pool: &SqlitePool,
    conversation_id: &str,
    swaps: &[CowSwap],
) -> Result<Vec<AppliedCowSwap>> {
    let mut tx = pool.begin().await?;

    // Resolve call_id → current tool slot. ASC ordering makes later inserts
    // overwrite earlier ones in the map, i.e. the newest occurrence wins.
    // Rows that are already COW copies are excluded: a copy carries the same
    // call_id as the original it replaced, and re-swapping it would chain
    // copies — this is what makes re-running a batch idempotent at the DB
    // level.
    let tool_rows = sqlx::query!(
        "SELECT cm.message_id AS message_id, m.parts_json AS parts_json, cm.ordering AS ordering, cm.appended_at_revision AS appended_at_revision FROM conversation_messages cm JOIN messages m ON m.id = cm.message_id WHERE cm.conversation_id = $1 AND m.role = 'tool' AND m.supersedes_message_id IS NULL ORDER BY cm.ordering ASC",
        conversation_id,
    )
    .fetch_all(&mut *tx)
    .await?;
    let mut slot_by_call: std::collections::HashMap<String, (String, i64, i64)> =
        std::collections::HashMap::new();
    for row in &tool_rows {
        let Ok(parts) = serde_json::from_str::<Vec<ContentPart>>(&row.parts_json) else {
            continue;
        };
        for part in parts {
            if let ContentPart::ToolResult(tr) = part {
                // ASC scan → later occurrences overwrite: newest slot wins.
                slot_by_call.insert(
                    tr.call_id,
                    (
                        row.message_id.clone(),
                        row.ordering,
                        row.appended_at_revision,
                    ),
                );
            }
        }
    }

    let revision = bump_revision(&mut tx, conversation_id).await?;
    let mut out = Vec::with_capacity(swaps.len());
    for swap in swaps {
        let Some((original_id, ordering, appended_at)) = slot_by_call.get(&swap.call_id).cloned()
        else {
            // No live result row for this call in this conversation.
            continue;
        };
        let f = build_row_fields(&swap.copy, swap.details.as_ref())?;
        let role = role_to_string(&swap.copy.role);
        sqlx::query!(
            "INSERT INTO messages (id, role, parts_json, details_json, provider_id, model_id, supersedes_message_id, created_at) VALUES ($1, $2, $3, $4, NULL, NULL, $5, $6)",
            f.id,
            role,
            f.parts_json,
            f.details_json,
            original_id,
            f.now,
        )
        .execute(&mut *tx)
        .await?;
        let updated = sqlx::query!(
            "UPDATE conversation_messages SET message_id = $1, swapped_at_revision = $2 WHERE conversation_id = $3 AND message_id = $4",
            f.id,
            revision,
            conversation_id,
            original_id,
        )
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() == 0 {
            // Raced away between the SELECT and the UPDATE; the copy row is a
            // harmless orphan.
            continue;
        }
        out.push(AppliedCowSwap {
            call_id: swap.call_id.clone(),
            copy: StoredMessage {
                id: f.id,
                conversation_id: conversation_id.to_string(),
                role: role.to_string(),
                parts_json: f.parts_json,
                details_json: f.details_json,
                ordering,
                provider_id: None,
                model_id: None,
                input_tokens: None,
                output_tokens: None,
                reasoning_tokens: None,
                supersedes_message_id: Some(original_id),
                appended_at_revision: appended_at,
                swapped_at_revision: Some(revision),
                created_at: f.now,
            },
        });
    }
    tx.commit().await?;
    Ok(out)
}

// ======================================================================
// Conversation-scoped reads
// ======================================================================

pub async fn list_messages(pool: &SqlitePool, conversation_id: &str) -> Result<Vec<StoredMessage>> {
    let rows = sqlx::query_as!(
        StoredMessage,
        "SELECT m.id, cm.conversation_id, m.role, m.parts_json, m.details_json, cm.ordering, m.provider_id, m.model_id, m.input_tokens, m.output_tokens, m.reasoning_tokens, m.supersedes_message_id, cm.appended_at_revision, cm.swapped_at_revision, m.created_at FROM conversation_messages cm JOIN messages m ON m.id = cm.message_id WHERE cm.conversation_id = $1 ORDER BY cm.ordering ASC",
        conversation_id,
    )
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

/// How many user messages the conversation already holds. Title generation
/// uses this as its one-shot guard: it only ever fires on the FIRST user
/// message, so a failed attempt is never retried on later turns.
pub async fn count_user_messages(pool: &SqlitePool, conversation_id: &str) -> Result<i64> {
    let count = sqlx::query_scalar!(
        "SELECT COUNT(*) FROM conversation_messages cm JOIN messages m ON m.id = cm.message_id WHERE cm.conversation_id = $1 AND m.role = 'user'",
        conversation_id,
    )
    .fetch_one(pool)
    .await?;

    Ok(count)
}

/// Whether the row is a compaction marker, by its details_json `kind`
/// (recognition rule shared with the agent side via [`crate::context`]).
pub async fn get_last_provider_spec(
    pool: &SqlitePool,
    conversation_id: &str,
) -> Result<Option<(String, String)>> {
    let row: Option<StoredMessage> = sqlx::query_as!(
        StoredMessage,
        "SELECT m.id, cm.conversation_id, m.role, m.parts_json, m.details_json, cm.ordering, m.provider_id, m.model_id, m.input_tokens, m.output_tokens, m.reasoning_tokens, m.supersedes_message_id, cm.appended_at_revision, cm.swapped_at_revision, m.created_at FROM conversation_messages cm JOIN messages m ON m.id = cm.message_id WHERE cm.conversation_id = $1 AND m.provider_id IS NOT NULL AND m.model_id IS NOT NULL ORDER BY cm.ordering DESC LIMIT 1",
        conversation_id,
    )
    .fetch_optional(pool)
    .await?;

    Ok(row.and_then(|m| match (m.provider_id, m.model_id) {
        (Some(p), Some(m)) => Some((p, m)),
        _ => None,
    }))
}

/// Ids of assistant ToolCalls that have no matching ToolResult anywhere in
/// the conversation — the persisted history of a run interrupted mid-tool
/// (e.g. backend crash). Sending that history to a provider as-is is a
/// protocol violation (400), so callers repair it via
/// [`repair_dangling_tool_calls`] before building an LLM request.
///
/// Marker-aware: only calls AFTER the last compaction marker are reported.
/// Pre-marker calls are excluded from every future LLM request anyway, and
/// repairing them would append placeholder rows at the END of the
/// conversation — past the marker — where they turn into orphan tool results
/// inside the view.
pub async fn find_dangling_tool_calls(
    pool: &SqlitePool,
    conversation_id: &str,
) -> Result<Vec<String>> {
    let stored = list_messages(pool, conversation_id).await?;
    let start = stored.iter().rposition(is_compaction_marker).unwrap_or(0);
    let mut call_ids: Vec<String> = Vec::new();
    let mut answered: HashSet<String> = HashSet::new();
    for msg in &stored[start..] {
        // Unparseable rows (corrupt parts_json) can neither contribute calls
        // nor answer them; skipping keeps repair additive instead of failing
        // the whole request.
        let Ok(parsed) = msg.to_message() else {
            continue;
        };
        for part in parsed.parts {
            match part {
                ContentPart::ToolCall(tc) => call_ids.push(tc.id),
                ContentPart::ToolResult(tr) => {
                    answered.insert(tr.call_id);
                }
                _ => {}
            }
        }
    }
    Ok(call_ids
        .into_iter()
        .filter(|id| !answered.contains(id))
        .collect())
}

/// Placeholder content for a tool call whose execution was interrupted by a
/// backend restart: the workspace state after the crash is unknown, so the
/// model is told as much instead of a fabricated result.
pub const INTERRUPTED_TOOL_RESULT_CONTENT: &str = "工具执行被中断（后端重启），工作区状态未知。";

/// Persist a synthetic error ToolResult for every dangling ToolCall (see
/// [`find_dangling_tool_calls`]), mirroring what the cancel path writes for
/// cancelled calls. Returns the repaired call ids. Idempotent: calls answered
/// by a real tool message (including partially-completed parallel batches)
/// are skipped.
pub async fn repair_dangling_tool_calls(
    pool: &SqlitePool,
    conversation_id: &str,
) -> Result<Vec<String>> {
    repair_dangling_tool_calls_except(pool, conversation_id, &[]).await
}

/// [`repair_dangling_tool_calls`] with an exclusion set: call ids in
/// `exclude` are NEVER repaired. The recovery coordinator passes the managed
/// `task` delegations here — those calls are waiting on their child runs and
/// must not be turned into error placeholders (父级未结算的
/// task 不走普通“补中断错误”修复).
pub async fn repair_dangling_tool_calls_except(
    pool: &SqlitePool,
    conversation_id: &str,
    exclude: &[String],
) -> Result<Vec<String>> {
    let dangling = find_dangling_tool_calls(pool, conversation_id)
        .await?
        .into_iter()
        .filter(|id| !exclude.contains(id))
        .collect::<Vec<_>>();
    for call_id in &dangling {
        let placeholder = Message {
            role: Role::Tool,
            parts: vec![ContentPart::ToolResult(ToolResult {
                call_id: call_id.clone(),
                content: Value::String(INTERRUPTED_TOOL_RESULT_CONTENT.into()),
                is_error: true,
                images: Vec::new(),
            })],
        };
        // details carry UI-only metadata ("执行被中断" title), never anything
        // model-visible.
        insert_message(
            pool,
            conversation_id,
            &placeholder,
            None,
            None,
            Some(&json!({ "title": "执行被中断" })),
        )
        .await?;
    }
    Ok(dangling)
}

/// The "结果只落一次" primitive: persist a ToolResult
/// answering `call_id` in `conversation_id` — but only when no tool row
/// already answers it (same scan family as [`find_dangling_tool_calls`]).
/// Returns whether THIS call inserted the row; the envelope is the caller's
/// single source of truth, so a `false` just means the result already landed
/// (e.g. the previous recovery attempt crashed right after persisting).
pub async fn answer_tool_call_if_absent(
    pool: &SqlitePool,
    conversation_id: &str,
    call_id: &str,
    content: &str,
    is_error: bool,
    details: Option<Value>,
) -> Result<bool> {
    // Absent exactly when the call is still dangling in this conversation
    // (same marker-aware scan as the repair path).
    let dangling = find_dangling_tool_calls(pool, conversation_id).await?;
    if !dangling.iter().any(|id| id == call_id) {
        return Ok(false);
    }
    let tool_msg = Message {
        role: Role::Tool,
        parts: vec![ContentPart::ToolResult(ToolResult {
            call_id: call_id.to_string(),
            content: Value::String(content.to_string()),
            is_error,
            images: Vec::new(),
        })],
    };
    insert_message(
        pool,
        conversation_id,
        &tool_msg,
        None,
        None,
        details.as_ref(),
    )
    .await?;
    Ok(true)
}

/// Whether the conversation's last message (by ordering) is a user message —
/// i.e. a turn was sent but never got any reply. Compaction markers don't
/// count: a manual compact right after an interrupted turn must not mask the
/// resumability signal.
pub async fn last_message_is_user(pool: &SqlitePool, conversation_id: &str) -> Result<bool> {
    Ok(list_messages(pool, conversation_id)
        .await?
        .last()
        .is_some_and(|m| m.role == "user" && !is_compaction_marker(m)))
}

/// The role of the conversation's last message (by ordering), or `None` for
/// an empty conversation. The extended resume-eligibility probe needs the
/// raw role: a `lost` run may have crashed after every tool result landed
/// but before the next model request, leaving `tool` as the last row
/// (恢复资格看 run 状态).
pub async fn last_message_role(pool: &SqlitePool, conversation_id: &str) -> Result<Option<String>> {
    Ok(list_messages(pool, conversation_id)
        .await?
        .last()
        .map(|m| m.role.clone()))
}

/// The conversation's most recent message timestamp. The idle trigger for
/// microcompact is computed against this BEFORE a new user message is
/// inserted (anything later would always look "just active").
pub async fn last_activity(
    pool: &SqlitePool,
    conversation_id: &str,
) -> Result<Option<chrono::NaiveDateTime>> {
    let last = sqlx::query_scalar!(
        "SELECT MAX(m.created_at) AS \"last: chrono::NaiveDateTime\" FROM conversation_messages cm JOIN messages m ON m.id = cm.message_id WHERE cm.conversation_id = $1",
        conversation_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(last)
}

/// A pure content row (no membership fields) — the response shape of the
/// "view original" endpoint.
#[derive(Debug, Clone, Serialize, sqlx::FromRow, ToSchema)]
pub struct MessageContent {
    pub id: String,
    pub role: String,
    pub parts_json: String,
    pub details_json: Option<String>,
    pub created_at: NaiveDateTime,
}

/// Fetch one content row by id (no membership/scope check — callers must
/// already hold a scoped reference, e.g. a run row's `final_message_id`).
pub async fn get_message_content(pool: &SqlitePool, id: &str) -> Result<Option<MessageContent>> {
    let row = sqlx::query_as!(
        MessageContent,
        "SELECT id, role, parts_json, details_json, created_at FROM messages WHERE id = $1",
        id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// The id of a conversation's FIRST user message — the delegation prompt of
/// a subagent conversation (its run boundary), by construction its first
/// row.
pub async fn first_user_message_id(
    pool: &SqlitePool,
    conversation_id: &str,
) -> Result<Option<String>> {
    let id = sqlx::query_scalar!(
        "SELECT m.id FROM conversation_messages cm JOIN messages m ON m.id = cm.message_id WHERE cm.conversation_id = $1 AND m.role = 'user' ORDER BY cm.ordering ASC LIMIT 1",
        conversation_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(id)
}

/// Fetch the original content row that the COW copy `copy_id` supersedes,
/// scope-checked through the copy's membership slot in `conversation_id` (the
/// original itself no longer belongs to any timeline after the swap).
pub async fn get_superseded_message(
    pool: &SqlitePool,
    conversation_id: &str,
    copy_id: &str,
) -> Result<Option<MessageContent>> {
    let row = sqlx::query_as!(
        MessageContent,
        "SELECT o.id, o.role, o.parts_json, o.details_json, o.created_at FROM conversation_messages cm JOIN messages c ON c.id = cm.message_id JOIN messages o ON o.id = c.supersedes_message_id WHERE cm.conversation_id = $1 AND cm.message_id = $2",
        conversation_id,
        copy_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Delete content rows nothing references anymore: no membership slot holds
/// them and no copy's `supersedes_message_id` points at them (the FK is
/// NO ACTION — deleting a referenced original would be rejected anyway; the
/// filter makes the intent explicit). Runs to a fixpoint: a single pass keeps
/// an original alive while its (also doomed) copy still exists, so sweeping
/// a conversation with COW copies needs the second pass to take the original
/// too. Chains are one hop by construction, capping the loop in practice at
/// two iterations. The lazy-GC path shared by conversation deletion and any
/// future maintenance entry point.
pub async fn delete_orphan_content_rows(pool: &SqlitePool) -> Result<u64> {
    let mut total = 0u64;
    // Hard cap against pathological chains (cannot exist today).
    for _ in 0..8 {
        let result = sqlx::query!(
            "DELETE FROM messages WHERE id NOT IN (SELECT message_id FROM conversation_messages) AND id NOT IN (SELECT supersedes_message_id FROM messages WHERE supersedes_message_id IS NOT NULL)"
        )
        .execute(pool)
        .await?;
        total += result.rows_affected();
        if result.rows_affected() == 0 {
            break;
        }
    }
    Ok(total)
}

pub async fn update_message_usage(
    pool: &SqlitePool,
    message_id: &str,
    input_tokens: Option<u32>,
    output_tokens: Option<u32>,
    reasoning_tokens: Option<u32>,
) -> Result<()> {
    let input = input_tokens.map(|v| v as i64);
    let output = output_tokens.map(|v| v as i64);
    let reasoning = reasoning_tokens.map(|v| v as i64);

    sqlx::query!(
        "UPDATE messages SET input_tokens = $1, output_tokens = $2, reasoning_tokens = $3 WHERE id = $4",
        input,
        output,
        reasoning,
        message_id,
    )
    .execute(pool)
    .await?;

    Ok(())
}

mod view;
use view::is_compaction_marker;
pub use view::{load_llm_view, load_messages_for_llm};
