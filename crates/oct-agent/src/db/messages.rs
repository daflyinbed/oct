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

pub async fn insert_message(
    pool: &SqlitePool,
    conversation_id: &str,
    message: &Message,
    provider_id: Option<&str>,
    model_id: Option<&str>,
    details: Option<&Value>,
) -> Result<StoredMessage> {
    let mut tx = pool.begin().await?;
    let ordering = sqlx::query_scalar!(
        "SELECT COALESCE(MAX(ordering), 0) + 1 FROM conversation_messages WHERE conversation_id = $1",
        conversation_id,
    )
    .fetch_one(&mut *tx)
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

/// details_json `kind` value marking a compaction marker row (see
/// `agent::compact`): a normal `user` message whose parts carry the summary
/// and whose details carry compaction metadata. Rows before the LAST marker
/// stay in the DB but never reach the LLM again.
pub const COMPACTION_MARKER_KIND: &str = "compaction";

/// Provider-truth anchor for context estimates: the `input + output` tokens
/// of the request that produced the anchor message, plus that message's index
/// in the LLM view. Everything appended after it has never been counted by a
/// provider and must be estimated locally.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageAnchor {
    pub tokens: u64,
    pub message_index: usize,
}

/// The model-visible view of a conversation plus the estimation anchor:
/// `[last compaction marker (if any), everything after it]` with orphan tool
/// results filtered out. System prompt is NOT included — the loop prepends it
/// fresh on every run. `orderings` is parallel to `messages` and carries each
/// message's timeline ordering (compaction consumes it to place its marker).
#[derive(Debug, Clone)]
pub struct LlmView {
    pub messages: Vec<Message>,
    pub orderings: Vec<i64>,
    pub anchor: Option<UsageAnchor>,
}

fn details_kind(m: &StoredMessage) -> Option<String> {
    m.details_json
        .as_ref()
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .and_then(|v| v.get("kind").and_then(Value::as_str).map(str::to_string))
}

fn is_compaction_marker(m: &StoredMessage) -> bool {
    details_kind(m).as_deref() == Some(COMPACTION_MARKER_KIND)
}

/// Build the LLM view for a conversation (see [`LlmView`]).
///
/// Anchor selection and adjustment:
/// * eligibility — the last assistant row with usage whose
///   `appended_at_revision` is LATER than the marker's. A tail assistant
///   appended before the marker had the entire compacted-away prefix inside
///   its input tokens; treating it as the anchor after a reload would
///   over-estimate wildly. No post-marker usage yet (e.g. the run died right
///   after compaction) means no anchor — local schema-aware estimation takes
///   over, mirroring the in-run `anchor = None` reset.
/// * COW deduction — a swap must be subtracted from the anchor's provider
///   tokens iff the original content was counted by the anchor's request
///   (slot ordering earlier than the anchor's) AND the swap happened after
///   that request (`swapped_at_revision > anchor.appended_at_revision`).
///   Provider usage itself is never rewritten; the adjustment is recomputed
///   from persisted state on every load, so it survives crashes.
pub async fn load_llm_view(pool: &SqlitePool, conversation_id: &str) -> Result<LlmView> {
    let stored = list_messages(pool, conversation_id).await?;
    // The view starts AT the last marker (inclusive): the marker IS the
    // summary message the model sees. Older markers are inside the excluded
    // prefix and simply stop mattering.
    let marker_index = stored.iter().rposition(is_compaction_marker);
    let start = marker_index.unwrap_or(0);
    let region = &stored[start..];

    // Orphan tool results (call summarized away or repaired past the marker)
    // are a protocol violation for providers — drop them defensively. Calls
    // are only "seen" once their assistant message appears, so ordering is
    // enforced too.
    let mut messages: Vec<Message> = Vec::with_capacity(region.len());
    let mut kept: Vec<&StoredMessage> = Vec::with_capacity(region.len());
    let mut seen_calls: HashSet<String> = HashSet::new();
    for row in region {
        let parsed = row.to_message()?;
        if parsed.role == Role::Tool {
            let has_live_call = parsed.parts.iter().all(|p| match p {
                ContentPart::ToolResult(tr) => seen_calls.contains(&tr.call_id),
                _ => true,
            });
            if !has_live_call {
                continue;
            }
        } else {
            for part in &parsed.parts {
                if let ContentPart::ToolCall(tc) = part {
                    seen_calls.insert(tc.id.clone());
                }
            }
        }
        kept.push(row);
        messages.push(parsed);
    }
    let orderings: Vec<i64> = kept.iter().map(|row| row.ordering).collect();

    // Anchor: the last kept assistant row that carries usage AND was appended
    // after the marker row (when one exists). Tool rows never carry usage, so
    // dropped orphans cannot have been the anchor.
    let marker_revision = marker_index.map(|i| stored[i].appended_at_revision);
    let mut anchor: Option<(usize, &StoredMessage)> = None;
    for (mi, row) in kept.iter().enumerate() {
        if row.role == "assistant"
            && row.input_tokens.is_some_and(|v| v > 0)
            && marker_revision.is_none_or(|mr| row.appended_at_revision > mr)
        {
            anchor = Some((mi, row));
        }
    }

    let anchor = match anchor {
        Some((mi, row)) => {
            let tokens = cow_adjusted_anchor_tokens(pool, &kept, &messages, row).await?;
            Some(UsageAnchor {
                tokens,
                message_index: mi,
            })
        }
        None => None,
    };

    Ok(LlmView {
        messages,
        orderings,
        anchor,
    })
}

/// The anchor row's provider tokens minus the savings of every COW swap the
/// anchor's request counted but that happened after it (the rule documented
/// on [`load_llm_view`]). Savings are recomputed from the two content rows (a
/// pure function), never stored; a missing or unparsable original is skipped
/// — over-estimating is the safe direction for a trigger.
async fn cow_adjusted_anchor_tokens(
    pool: &SqlitePool,
    kept: &[&StoredMessage],
    messages: &[Message],
    anchor_row: &StoredMessage,
) -> Result<u64> {
    let tokens =
        (anchor_row.input_tokens.unwrap_or(0) + anchor_row.output_tokens.unwrap_or(0)) as u64;
    let anchor_ordering = anchor_row.ordering;
    let anchor_revision = anchor_row.appended_at_revision;
    let is_affected = |r: &StoredMessage| {
        r.swapped_at_revision.is_some_and(|s| s > anchor_revision)
            && r.ordering < anchor_ordering
            && r.supersedes_message_id.is_some()
    };
    let original_ids: Vec<&str> = kept
        .iter()
        .filter(|r| is_affected(r))
        .filter_map(|r| r.supersedes_message_id.as_deref())
        .collect();
    if original_ids.is_empty() {
        return Ok(tokens);
    }
    let ids_json = serde_json::to_string(&original_ids)?;
    let original_rows = sqlx::query!(
        "SELECT id, parts_json FROM messages WHERE id IN (SELECT value FROM json_each($1))",
        ids_json,
    )
    .fetch_all(pool)
    .await?;
    let mut savings = 0u64;
    for (r, current) in kept.iter().zip(messages) {
        if !is_affected(r) {
            continue;
        }
        let Some(original) = original_rows
            .iter()
            .find(|o| o.id == r.supersedes_message_id.as_deref().expect("checked above"))
            .and_then(|o| serde_json::from_str::<Vec<ContentPart>>(&o.parts_json).ok())
            .map(|parts| Message {
                role: Role::Tool,
                parts,
            })
        else {
            continue;
        };
        savings += crate::agent::compact::estimate_message(&original)
            .saturating_sub(crate::agent::compact::estimate_message(current));
    }
    Ok(tokens.saturating_sub(savings))
}

pub async fn load_messages_for_llm(
    pool: &SqlitePool,
    conversation_id: &str,
) -> Result<Vec<Message>> {
    Ok(load_llm_view(pool, conversation_id).await?.messages)
}

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
    let dangling = find_dangling_tool_calls(pool, conversation_id).await?;
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
