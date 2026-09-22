use anyhow::Result;
use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::SqlitePool;
use std::collections::HashSet;
use utoipa::ToSchema;
use uuid::Uuid;

use oct_llm_provider::core::{ContentPart, Message, Role, ToolResult};

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

pub async fn insert_message(
    pool: &SqlitePool,
    conversation_id: &str,
    message: &Message,
    provider_id: Option<&str>,
    model_id: Option<&str>,
    details: Option<&serde_json::Value>,
) -> Result<StoredMessage> {
    let id = Uuid::new_v4().to_string();
    let role = role_to_string(&message.role);
    let parts_json = serde_json::to_string(&message.parts)?;
    // UI-only metadata; stored in its own column so it never leaks into the
    // model-visible parts_json.
    let details_json = match details {
        Some(value) => Some(serde_json::to_string(value)?),
        None => None,
    };
    let now = chrono::Utc::now().naive_utc();

    let ordering = sqlx::query_scalar!(
        "INSERT INTO messages (id, conversation_id, role, parts_json, details_json, ordering, provider_id, model_id, created_at) SELECT $1, $2, $3, $4, $5, COALESCE(MAX(ordering), 0) + 1, $6, $7, $8 FROM messages WHERE conversation_id = $2 RETURNING ordering",
        id,
        conversation_id,
        role,
        parts_json,
        details_json,
        provider_id,
        model_id,
        now,
    )
    .fetch_one(pool)
    .await?;

    Ok(StoredMessage {
        id,
        conversation_id: conversation_id.to_string(),
        role: role.to_string(),
        parts_json,
        details_json,
        ordering,
        provider_id: provider_id.map(|s| s.to_string()),
        model_id: model_id.map(|s| s.to_string()),
        input_tokens: None,
        output_tokens: None,
        reasoning_tokens: None,
        created_at: now,
    })
}

pub async fn list_messages(pool: &SqlitePool, conversation_id: &str) -> Result<Vec<StoredMessage>> {
    let rows = sqlx::query_as!(
        StoredMessage,
        "SELECT id, conversation_id, role, parts_json, details_json, ordering, provider_id, model_id, input_tokens, output_tokens, reasoning_tokens, created_at FROM messages WHERE conversation_id = $1 ORDER BY ordering ASC",
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
        "SELECT COUNT(*) FROM messages WHERE conversation_id = $1 AND role = 'user'",
        conversation_id
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
#[derive(Debug, Clone)]
pub struct UsageAnchor {
    pub tokens: u64,
    pub message_index: usize,
}

/// The model-visible view of a conversation plus the estimation anchor:
/// `[last compaction marker (if any), everything after it]` with orphan tool
/// results filtered out. System prompt is NOT included — the loop prepends it
/// fresh on every run.
#[derive(Debug, Clone)]
pub struct LlmView {
    pub messages: Vec<Message>,
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
pub async fn load_llm_view(pool: &SqlitePool, conversation_id: &str) -> Result<LlmView> {
    let stored = list_messages(pool, conversation_id).await?;
    // The view starts AT the last marker (inclusive): the marker IS the
    // summary message the model sees. Older markers are inside the excluded
    // prefix and simply stop mattering.
    let start = stored.iter().rposition(is_compaction_marker).unwrap_or(0);
    let region = &stored[start..];

    let parsed: Vec<Message> = region
        .iter()
        .map(|s| s.to_message())
        .collect::<Result<_>>()?;

    // Orphan tool results (call summarized away or repaired past the marker)
    // are a protocol violation for providers — drop them defensively. Calls
    // are only "seen" once their assistant message appears, so ordering is
    // enforced too.
    let mut messages: Vec<Message> = Vec::with_capacity(parsed.len());
    let mut kept_region_idx: Vec<usize> = Vec::with_capacity(parsed.len());
    let mut seen_calls: HashSet<String> = HashSet::new();
    for (idx, msg) in parsed.into_iter().enumerate() {
        if msg.role == Role::Tool {
            let has_live_call = msg.parts.iter().all(|p| match p {
                ContentPart::ToolResult(tr) => seen_calls.contains(&tr.call_id),
                _ => true,
            });
            if !has_live_call {
                continue;
            }
        } else {
            for part in &msg.parts {
                if let ContentPart::ToolCall(tc) = part {
                    seen_calls.insert(tc.id.clone());
                }
            }
        }
        kept_region_idx.push(idx);
        messages.push(msg);
    }

    // Anchor: the last kept assistant row that carries usage. Tool rows never
    // carry usage, so dropped orphans cannot have been the anchor.
    let mut anchor: Option<(usize, usize)> = None; // (message_index, region_index)
    for (mi, &r) in kept_region_idx.iter().enumerate() {
        if region[r].role == "assistant" && region[r].input_tokens.is_some_and(|v| v > 0) {
            anchor = Some((mi, r));
        }
    }
    let anchor = anchor.map(|(mi, r)| UsageAnchor {
        tokens: (region[r].input_tokens.unwrap_or(0) + region[r].output_tokens.unwrap_or(0)) as u64,
        message_index: mi,
    });

    Ok(LlmView { messages, anchor })
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
        "SELECT id, conversation_id, role, parts_json, details_json, ordering, provider_id, model_id, input_tokens, output_tokens, reasoning_tokens, created_at FROM messages WHERE conversation_id = $1 AND provider_id IS NOT NULL AND model_id IS NOT NULL ORDER BY ordering DESC LIMIT 1",
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
