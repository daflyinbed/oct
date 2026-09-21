use anyhow::Result;
use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use utoipa::ToSchema;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct Conversation {
    pub id: String,
    pub project_id: String,
    pub title: String,
    /// Where `title` came from: `default` (untitled placeholder), `ai`
    /// (auto-generated), `user` (explicitly set via API). Manual names must
    /// win over automatic ones, so the distinction is data, not string
    /// matching on the default title.
    pub title_source: String,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateConversationRequest {
    pub title: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateConversationRequest {
    pub title: Option<String>,
}

pub async fn list_conversations_by_project(
    pool: &SqlitePool,
    project_id: &str,
) -> Result<Vec<Conversation>> {
    let rows = sqlx::query_as!(
        Conversation,
        "SELECT id, project_id, title, title_source, created_at, updated_at FROM conversations WHERE project_id = $1 ORDER BY updated_at DESC",
        project_id,
    )
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

pub async fn create_conversation(
    pool: &SqlitePool,
    project_id: &str,
    title: Option<&str>,
) -> Result<Conversation> {
    let id = Uuid::new_v4().to_string();
    // An explicit title at creation is a user-chosen name: automatic title
    // generation must not override it.
    let title_source = if title.is_some() { "user" } else { "default" };
    let title = title.unwrap_or("New conversation");
    let now = chrono::Utc::now().naive_utc();

    sqlx::query!(
        "INSERT INTO conversations (id, project_id, title, title_source, created_at, updated_at) VALUES ($1, $2, $3, $4, $5, $6)",
        id,
        project_id,
        title,
        title_source,
        now,
        now,
    )
    .execute(pool)
    .await?;

    Ok(Conversation {
        id,
        project_id: project_id.to_string(),
        title: title.to_string(),
        title_source: title_source.to_string(),
        created_at: now,
        updated_at: now,
    })
}

pub async fn get_conversation(pool: &SqlitePool, id: &str) -> Result<Option<Conversation>> {
    let row = sqlx::query_as!(
        Conversation,
        "SELECT id, project_id, title, title_source, created_at, updated_at FROM conversations WHERE id = $1",
        id,
    )
    .fetch_optional(pool)
    .await?;

    Ok(row)
}

pub async fn delete_conversation(pool: &SqlitePool, id: &str) -> Result<bool> {
    let result: sqlx::sqlite::SqliteQueryResult = sqlx::query!("DELETE FROM conversations WHERE id = $1", id)
        .execute(pool)
        .await?;

    Ok(result.rows_affected() > 0)
}

pub async fn update_conversation_title(
    pool: &SqlitePool,
    id: &str,
    title: &str,
) -> Result<bool> {
    let now = chrono::Utc::now().naive_utc();
    let result: sqlx::sqlite::SqliteQueryResult =
        sqlx::query!("UPDATE conversations SET title = $1, title_source = 'user', updated_at = $2 WHERE id = $3", title, now, id)
        .execute(pool)
        .await?;

    Ok(result.rows_affected() > 0)
}

/// Write an automatically generated title, but only while the conversation
/// still carries its default one. The conditional makes the write both
/// one-shot (a second generation attempt no-ops) and lose-able: a manual
/// rename flips `title_source` to `user` and this UPDATE matches nothing, so
/// the user's name always wins without any locking. `updated_at` is
/// deliberately untouched — a background title landing must not reorder the
/// conversation list mid-session.
pub async fn set_auto_title(pool: &SqlitePool, id: &str, title: &str) -> Result<bool> {
    let result: sqlx::sqlite::SqliteQueryResult = sqlx::query!(
        "UPDATE conversations SET title = $1, title_source = 'ai' WHERE id = $2 AND title_source = 'default'",
        title,
        id
    )
    .execute(pool)
    .await?;

    Ok(result.rows_affected() > 0)
}
