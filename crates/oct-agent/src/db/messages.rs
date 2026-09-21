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

pub async fn load_messages_for_llm(
    pool: &SqlitePool,
    conversation_id: &str,
) -> Result<Vec<Message>> {
    let stored = list_messages(pool, conversation_id).await?;
    stored.iter().map(|s| s.to_message()).collect()
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
pub async fn find_dangling_tool_calls(
    pool: &SqlitePool,
    conversation_id: &str,
) -> Result<Vec<String>> {
    let stored = list_messages(pool, conversation_id).await?;
    let mut call_ids: Vec<String> = Vec::new();
    let mut answered: HashSet<String> = HashSet::new();
    for msg in &stored {
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
/// i.e. a turn was sent but never got any reply.
pub async fn last_message_is_user(pool: &SqlitePool, conversation_id: &str) -> Result<bool> {
    Ok(list_messages(pool, conversation_id)
        .await?
        .last()
        .is_some_and(|m| m.role == "user"))
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
