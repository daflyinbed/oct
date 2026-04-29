use anyhow::Result;
use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::ToSchema;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct Provider {
    pub id: String,
    pub name: String,
    pub adapter_type: String,
    pub base_url: Option<String>,
    #[serde(skip_serializing)]
    pub api_key: String,
    pub doc_url: Option<String>,
    pub source: String,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateProviderRequest {
    pub id: String,
    pub name: String,
    pub base_url: String,
    pub api_key: String,
    pub doc_url: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateProviderRequest {
    pub name: Option<String>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub doc_url: Option<String>,
}

pub async fn list_providers(pool: &PgPool) -> Result<Vec<Provider>> {
    let rows = sqlx::query_as!(
        Provider,
        "SELECT id, name, adapter_type, base_url, api_key, doc_url, source, created_at, updated_at FROM providers ORDER BY created_at ASC"
    )
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

pub async fn get_provider(pool: &PgPool, id: &str) -> Result<Option<Provider>> {
    let row = sqlx::query_as!(
        Provider,
        "SELECT id, name, adapter_type, base_url, api_key, doc_url, source, created_at, updated_at FROM providers WHERE id = $1",
        id,
    )
    .fetch_optional(pool)
    .await?;

    Ok(row)
}

pub async fn create_custom_provider(
    pool: &PgPool,
    req: &CreateProviderRequest,
) -> Result<Provider> {
    let now = chrono::Utc::now().naive_utc();

    sqlx::query!(
        "INSERT INTO providers (id, name, adapter_type, base_url, api_key, doc_url, source, created_at, updated_at) VALUES ($1, $2, 'openai_compatible', $3, $4, $5, 'custom', $6, $7)",
        req.id,
        req.name,
        req.base_url,
        req.api_key,
        req.doc_url,
        now,
        now,
    )
    .execute(pool)
    .await?;

    Ok(Provider {
        id: req.id.clone(),
        name: req.name.clone(),
        adapter_type: "openai_compatible".to_string(),
        base_url: Some(req.base_url.clone()),
        api_key: req.api_key.clone(),
        doc_url: req.doc_url.clone(),
        source: "custom".to_string(),
        created_at: now,
        updated_at: now,
    })
}

pub async fn update_provider(
    pool: &PgPool,
    id: &str,
    req: &UpdateProviderRequest,
) -> Result<bool> {
    let existing = sqlx::query_as!(
        Provider,
        "SELECT id, name, adapter_type, base_url, api_key, doc_url, source, created_at, updated_at FROM providers WHERE id = $1",
        id,
    )
    .fetch_optional(pool)
    .await?;

    let Some(existing) = existing else { return Ok(false) };

    let now = chrono::Utc::now().naive_utc();
    let name = req.name.as_deref().unwrap_or(&existing.name);
    let base_url = req.base_url.as_deref().or(existing.base_url.as_deref());
    let api_key = req.api_key.as_deref().unwrap_or(&existing.api_key);
    let doc_url = req.doc_url.as_deref().or(existing.doc_url.as_deref());

    let result: sqlx::postgres::PgQueryResult = sqlx::query!(
        "UPDATE providers SET name = $1, base_url = $2, api_key = $3, doc_url = $4, updated_at = $5 WHERE id = $6",
        name,
        base_url,
        api_key,
        doc_url,
        now,
        id,
    )
    .execute(pool)
    .await?;

    Ok(result.rows_affected() > 0)
}

pub async fn delete_provider(pool: &PgPool, id: &str) -> Result<bool> {
    let result: sqlx::postgres::PgQueryResult = sqlx::query!(
        "DELETE FROM providers WHERE id = $1 AND source = 'custom'",
        id
    )
    .execute(pool)
    .await?;

    Ok(result.rows_affected() > 0)
}
