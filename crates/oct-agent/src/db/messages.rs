use anyhow::Result;
use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use utoipa::ToSchema;
use uuid::Uuid;

use oct_llm_provider::core::{ContentPart, Message, Role};

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct StoredMessage {
    pub id: String,
    pub conversation_id: String,
    pub role: String,
    pub parts_json: String,
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
) -> Result<StoredMessage> {
    let id = Uuid::new_v4().to_string();
    let role = role_to_string(&message.role);
    let parts_json = serde_json::to_string(&message.parts)?;
    let now = chrono::Utc::now().naive_utc();

    let ordering = sqlx::query_scalar!(
        "INSERT INTO messages (id, conversation_id, role, parts_json, ordering, provider_id, model_id, created_at) SELECT $1, $2, $3, $4, COALESCE(MAX(ordering), 0) + 1, $5, $6, $7 FROM messages WHERE conversation_id = $2 RETURNING ordering",
        id,
        conversation_id,
        role,
        parts_json,
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
        ordering,
        provider_id: provider_id.map(|s| s.to_string()),
        model_id: model_id.map(|s| s.to_string()),
        input_tokens: None,
        output_tokens: None,
        reasoning_tokens: None,
        created_at: now,
    })
}

pub async fn list_messages(
    pool: &SqlitePool,
    conversation_id: &str,
) -> Result<Vec<StoredMessage>> {
    let rows = sqlx::query_as!(
        StoredMessage,
        "SELECT id, conversation_id, role, parts_json, ordering, provider_id, model_id, input_tokens, output_tokens, reasoning_tokens, created_at FROM messages WHERE conversation_id = $1 ORDER BY ordering ASC",
        conversation_id,
    )
    .fetch_all(pool)
    .await?;

    Ok(rows)
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
        "SELECT id, conversation_id, role, parts_json, ordering, provider_id, model_id, input_tokens, output_tokens, reasoning_tokens, created_at FROM messages WHERE conversation_id = $1 AND provider_id IS NOT NULL AND model_id IS NOT NULL ORDER BY ordering DESC LIMIT 1",
        conversation_id,
    )
    .fetch_optional(pool)
    .await?;

    Ok(row.and_then(|m| {
        match (m.provider_id, m.model_id) {
            (Some(p), Some(m)) => Some((p, m)),
            _ => None,
        }
    }))
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
