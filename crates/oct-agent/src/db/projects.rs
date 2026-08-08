use anyhow::Result;
use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use utoipa::ToSchema;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub working_dir: String,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateProjectRequest {
    pub name: String,
    pub working_dir: String,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateProjectRequest {
    pub name: Option<String>,
    pub working_dir: Option<String>,
}

pub async fn list_projects(pool: &SqlitePool) -> Result<Vec<Project>> {
    let rows = sqlx::query_as!(
        Project,
        "SELECT id, name, working_dir, created_at, updated_at FROM projects ORDER BY updated_at DESC"
    )
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

pub async fn get_project(pool: &SqlitePool, id: &str) -> Result<Option<Project>> {
    let row = sqlx::query_as!(
        Project,
        "SELECT id, name, working_dir, created_at, updated_at FROM projects WHERE id = $1",
        id,
    )
    .fetch_optional(pool)
    .await?;

    Ok(row)
}

pub async fn create_project(pool: &SqlitePool, req: &CreateProjectRequest) -> Result<Project> {
    let id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().naive_utc();

    sqlx::query!(
        "INSERT INTO projects (id, name, working_dir, created_at, updated_at) VALUES ($1, $2, $3, $4, $5)",
        id,
        req.name,
        req.working_dir,
        now,
        now,
    )
    .execute(pool)
    .await?;

    Ok(Project {
        id,
        name: req.name.clone(),
        working_dir: req.working_dir.clone(),
        created_at: now,
        updated_at: now,
    })
}

pub async fn update_project(pool: &SqlitePool, id: &str, req: &UpdateProjectRequest) -> Result<bool> {
    let existing = sqlx::query_as!(
        Project,
        "SELECT id, name, working_dir, created_at, updated_at FROM projects WHERE id = $1",
        id,
    )
    .fetch_optional(pool)
    .await?;

    let Some(existing) = existing else { return Ok(false) };

    let now = chrono::Utc::now().naive_utc();
    let name = req.name.as_deref().unwrap_or(&existing.name);
    let working_dir = req.working_dir.as_deref().unwrap_or(&existing.working_dir);

    let result: sqlx::sqlite::SqliteQueryResult = sqlx::query!(
        "UPDATE projects SET name = $1, working_dir = $2, updated_at = $3 WHERE id = $4",
        name,
        working_dir,
        now,
        id,
    )
    .execute(pool)
    .await?;

    Ok(result.rows_affected() > 0)
}

pub async fn delete_project(pool: &SqlitePool, id: &str) -> Result<bool> {
    let result: sqlx::sqlite::SqliteQueryResult = sqlx::query!("DELETE FROM projects WHERE id = $1", id)
        .execute(pool)
        .await?;

    Ok(result.rows_affected() > 0)
}
