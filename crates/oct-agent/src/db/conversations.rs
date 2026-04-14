use anyhow::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use utoipa::ToSchema;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct Conversation {
    pub id: String,
    pub title: String,
    pub working_dir: String,
    pub provider_spec: String,
    pub created_at: String,
    pub updated_at: String,
}

pub async fn create_conversation(
    pool: &SqlitePool,
    working_dir: &str,
    provider_spec: &str,
    title: Option<&str>,
) -> Result<Conversation> {
    let id = Uuid::new_v4().to_string();
    let title = title.unwrap_or("New conversation");
    let now = Utc::now().naive_utc().format("%Y-%m-%d %H:%M:%S").to_string();

    sqlx::query(
        "INSERT INTO conversations (id, title, working_dir, provider_spec, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(title)
    .bind(working_dir)
    .bind(provider_spec)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await?;

    Ok(Conversation {
        id,
        title: title.to_string(),
        working_dir: working_dir.to_string(),
        provider_spec: provider_spec.to_string(),
        created_at: now.clone(),
        updated_at: now,
    })
}

pub async fn list_conversations(pool: &SqlitePool) -> Result<Vec<Conversation>> {
    let rows = sqlx::query_as::<_, Conversation>(
        "SELECT id, title, working_dir, provider_spec, created_at, updated_at FROM conversations ORDER BY updated_at DESC",
    )
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

pub async fn get_conversation(pool: &SqlitePool, id: &str) -> Result<Option<Conversation>> {
    let row = sqlx::query_as::<_, Conversation>(
        "SELECT id, title, working_dir, provider_spec, created_at, updated_at FROM conversations WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;

    Ok(row)
}

pub async fn delete_conversation(pool: &SqlitePool, id: &str) -> Result<bool> {
    let result = sqlx::query("DELETE FROM conversations WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;

    Ok(result.rows_affected() > 0)
}

pub async fn update_conversation_title(pool: &SqlitePool, id: &str, title: &str) -> Result<bool> {
    let now = Utc::now().naive_utc().format("%Y-%m-%d %H:%M:%S").to_string();
    let result =
        sqlx::query("UPDATE conversations SET title = ?, updated_at = ? WHERE id = ?")
            .bind(title)
            .bind(&now)
            .bind(id)
            .execute(pool)
            .await?;

    Ok(result.rows_affected() > 0)
}

pub async fn update_conversation_provider(
    pool: &SqlitePool,
    id: &str,
    provider_spec: &str,
) -> Result<bool> {
    let now = Utc::now().naive_utc().format("%Y-%m-%d %H:%M:%S").to_string();
    let result = sqlx::query(
        "UPDATE conversations SET provider_spec = ?, updated_at = ? WHERE id = ?",
    )
    .bind(provider_spec)
    .bind(&now)
    .bind(id)
    .execute(pool)
    .await?;

    Ok(result.rows_affected() > 0)
}
