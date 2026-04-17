use anyhow::Result;
use chrono::Utc;
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
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub reasoning_tokens: Option<i64>,
    pub created_at: String,
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
) -> Result<StoredMessage> {
    let id = Uuid::new_v4().to_string();
    let role = role_to_string(&message.role);
    let parts_json = serde_json::to_string(&message.parts)?;
    let now = Utc::now().naive_utc().format("%Y-%m-%d %H:%M:%S").to_string();

    let ordering: i64 = sqlx::query_scalar!(
        "SELECT COALESCE(MAX(ordering), 0) + 1 FROM messages WHERE conversation_id = ?",
        conversation_id,
    )
    .fetch_one(pool)
    .await?;

    sqlx::query!(
        "INSERT INTO messages (id, conversation_id, role, parts_json, ordering, created_at) VALUES (?, ?, ?, ?, ?, ?)",
        id,
        conversation_id,
        role,
        parts_json,
        ordering,
        now,
    )
    .execute(pool)
    .await?;

    Ok(StoredMessage {
        id,
        conversation_id: conversation_id.to_string(),
        role: role.to_string(),
        parts_json,
        ordering,
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
        "SELECT id, conversation_id, role, parts_json, ordering, input_tokens, output_tokens, reasoning_tokens, created_at FROM messages WHERE conversation_id = ? ORDER BY ordering ASC",
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
        "UPDATE messages SET input_tokens = ?, output_tokens = ?, reasoning_tokens = ? WHERE id = ?",
        input,
        output,
        reasoning,
        message_id,
    )
    .execute(pool)
    .await?;

    Ok(())
}
