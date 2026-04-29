use anyhow::Result;
use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::ToSchema;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct Conversation {
    pub id: String,
    pub project_id: String,
    pub title: String,
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
    pool: &PgPool,
    project_id: &str,
) -> Result<Vec<Conversation>> {
    let rows = sqlx::query_as!(
        Conversation,
        "SELECT id, project_id, title, created_at, updated_at FROM conversations WHERE project_id = $1 ORDER BY updated_at DESC",
        project_id,
    )
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

pub async fn create_conversation(
    pool: &PgPool,
    project_id: &str,
    title: Option<&str>,
) -> Result<Conversation> {
    let id = Uuid::new_v4().to_string();
    let title = title.unwrap_or("New conversation");
    let now = chrono::Utc::now().naive_utc();

    sqlx::query!(
        "INSERT INTO conversations (id, project_id, title, created_at, updated_at) VALUES ($1, $2, $3, $4, $5)",
        id,
        project_id,
        title,
        now,
        now,
    )
    .execute(pool)
    .await?;

    Ok(Conversation {
        id,
        project_id: project_id.to_string(),
        title: title.to_string(),
        created_at: now,
        updated_at: now,
    })
}

pub async fn get_conversation(pool: &PgPool, id: &str) -> Result<Option<Conversation>> {
    let row = sqlx::query_as!(
        Conversation,
        "SELECT id, project_id, title, created_at, updated_at FROM conversations WHERE id = $1",
        id,
    )
    .fetch_optional(pool)
    .await?;

    Ok(row)
}

pub async fn delete_conversation(pool: &PgPool, id: &str) -> Result<bool> {
    let result: sqlx::postgres::PgQueryResult = sqlx::query!("DELETE FROM conversations WHERE id = $1", id)
        .execute(pool)
        .await?;

    Ok(result.rows_affected() > 0)
}

pub async fn update_conversation_title(
    pool: &PgPool,
    id: &str,
    title: &str,
) -> Result<bool> {
    let now = chrono::Utc::now().naive_utc();
    let result: sqlx::postgres::PgQueryResult =
        sqlx::query!("UPDATE conversations SET title = $1, updated_at = $2 WHERE id = $3", title, now, id)
        .execute(pool)
        .await?;

    Ok(result.rows_affected() > 0)
}
