use anyhow::Result;
use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use utoipa::ToSchema;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct Model {
    pub id: String,
    pub provider_id: String,
    pub model_id: String,
    pub name: String,
    pub family: Option<String>,
    pub reasoning: bool,
    pub tool_call: bool,
    pub attachment: bool,
    pub structured_output: bool,
    pub temperature: bool,
    pub knowledge: Option<String>,
    pub release_date: Option<String>,
    pub open_weights: bool,
    pub cost_input: Option<f64>,
    pub cost_output: Option<f64>,
    pub cost_cache_read: Option<f64>,
    pub cost_cache_write: Option<f64>,
    pub limit_context: Option<i64>,
    pub limit_output: Option<i64>,
    pub modalities_input: Option<String>,
    pub modalities_output: Option<String>,
    pub source: String,
    pub is_enabled: bool,
    pub created_at: NaiveDateTime,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateModelRequest {
    pub model_id: String,
    pub name: String,
    pub family: Option<String>,
    pub reasoning: Option<bool>,
    pub tool_call: Option<bool>,
    pub attachment: Option<bool>,
    pub structured_output: Option<bool>,
    pub temperature: Option<bool>,
    pub knowledge: Option<String>,
    pub release_date: Option<String>,
    pub open_weights: Option<bool>,
    pub cost_input: Option<f64>,
    pub cost_output: Option<f64>,
    pub cost_cache_read: Option<f64>,
    pub cost_cache_write: Option<f64>,
    pub limit_context: Option<i64>,
    pub limit_output: Option<i64>,
    pub modalities_input: Option<String>,
    pub modalities_output: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateModelRequest {
    pub name: Option<String>,
    pub family: Option<String>,
    pub reasoning: Option<bool>,
    pub tool_call: Option<bool>,
    pub attachment: Option<bool>,
    pub structured_output: Option<bool>,
    pub temperature: Option<bool>,
    pub knowledge: Option<String>,
    pub release_date: Option<String>,
    pub open_weights: Option<bool>,
    pub cost_input: Option<f64>,
    pub cost_output: Option<f64>,
    pub cost_cache_read: Option<f64>,
    pub cost_cache_write: Option<f64>,
    pub limit_context: Option<i64>,
    pub limit_output: Option<i64>,
    pub modalities_input: Option<String>,
    pub modalities_output: Option<String>,
    pub is_enabled: Option<bool>,
}

pub async fn list_models_by_provider(pool: &SqlitePool, provider_id: &str) -> Result<Vec<Model>> {
    let rows = sqlx::query_as!(
        Model,
        "SELECT id, provider_id, model_id, name, family, reasoning, tool_call, attachment, structured_output, temperature, knowledge, release_date, open_weights, cost_input, cost_output, cost_cache_read, cost_cache_write, limit_context, limit_output, modalities_input, modalities_output, source, is_enabled, created_at FROM models WHERE provider_id = $1 ORDER BY created_at ASC",
        provider_id,
    )
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

pub async fn list_enabled_models(pool: &SqlitePool) -> Result<Vec<Model>> {
    let rows = sqlx::query_as!(
        Model,
        "SELECT id, provider_id, model_id, name, family, reasoning, tool_call, attachment, structured_output, temperature, knowledge, release_date, open_weights, cost_input, cost_output, cost_cache_read, cost_cache_write, limit_context, limit_output, modalities_input, modalities_output, source, is_enabled, created_at FROM models WHERE is_enabled = true ORDER BY provider_id, created_at ASC"
    )
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

pub async fn get_model(
    pool: &SqlitePool,
    provider_id: &str,
    model_id: &str,
) -> Result<Option<Model>> {
    let row = sqlx::query_as!(
        Model,
        "SELECT id, provider_id, model_id, name, family, reasoning, tool_call, attachment, structured_output, temperature, knowledge, release_date, open_weights, cost_input, cost_output, cost_cache_read, cost_cache_write, limit_context, limit_output, modalities_input, modalities_output, source, is_enabled, created_at FROM models WHERE provider_id = $1 AND model_id = $2",
        provider_id,
        model_id,
    )
    .fetch_optional(pool)
    .await?;

    Ok(row)
}

pub async fn create_model(
    pool: &SqlitePool,
    provider_id: &str,
    req: &CreateModelRequest,
) -> Result<Model> {
    let id = format!("{}.{}", provider_id, req.model_id);
    let now = chrono::Utc::now().naive_utc();

    let reasoning = req.reasoning.unwrap_or(false);
    let tool_call = req.tool_call.unwrap_or(true);
    let attachment = req.attachment.unwrap_or(false);
    let structured_output = req.structured_output.unwrap_or(false);
    let temperature = req.temperature.unwrap_or(true);
    let open_weights = req.open_weights.unwrap_or(false);

    sqlx::query!(
        "INSERT INTO models (id, provider_id, model_id, name, family, reasoning, tool_call, attachment, structured_output, temperature, knowledge, release_date, open_weights, cost_input, cost_output, cost_cache_read, cost_cache_write, limit_context, limit_output, modalities_input, modalities_output, source) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, 'custom')",
        id,
        provider_id,
        req.model_id,
        req.name,
        req.family,
        reasoning,
        tool_call,
        attachment,
        structured_output,
        temperature,
        req.knowledge,
        req.release_date,
        open_weights,
        req.cost_input,
        req.cost_output,
        req.cost_cache_read,
        req.cost_cache_write,
        req.limit_context,
        req.limit_output,
        req.modalities_input,
        req.modalities_output,
    )
    .execute(pool)
    .await?;

    Ok(Model {
        id,
        provider_id: provider_id.to_string(),
        model_id: req.model_id.clone(),
        name: req.name.clone(),
        family: req.family.clone(),
        reasoning,
        tool_call,
        attachment,
        structured_output,
        temperature,
        knowledge: req.knowledge.clone(),
        release_date: req.release_date.clone(),
        open_weights,
        cost_input: req.cost_input,
        cost_output: req.cost_output,
        cost_cache_read: req.cost_cache_read,
        cost_cache_write: req.cost_cache_write,
        limit_context: req.limit_context,
        limit_output: req.limit_output,
        modalities_input: req.modalities_input.clone(),
        modalities_output: req.modalities_output.clone(),
        source: "custom".to_string(),
        is_enabled: true,
        created_at: now,
    })
}

pub async fn update_model(
    pool: &SqlitePool,
    provider_id: &str,
    model_id: &str,
    req: &UpdateModelRequest,
) -> Result<bool> {
    let existing = get_model(pool, provider_id, model_id).await?;
    let Some(existing) = existing else { return Ok(false) };

    let name = req.name.as_deref().unwrap_or(&existing.name);
    let family = req.family.as_deref().or(existing.family.as_deref());
    let reasoning = req.reasoning.unwrap_or(existing.reasoning);
    let tool_call = req.tool_call.unwrap_or(existing.tool_call);
    let attachment = req.attachment.unwrap_or(existing.attachment);
    let structured_output = req.structured_output.unwrap_or(existing.structured_output);
    let temperature = req.temperature.unwrap_or(existing.temperature);
    let knowledge = req.knowledge.as_deref().or(existing.knowledge.as_deref());
    let release_date = req.release_date.as_deref().or(existing.release_date.as_deref());
    let open_weights = req.open_weights.unwrap_or(existing.open_weights);
    let cost_input = req.cost_input.or(existing.cost_input);
    let cost_output = req.cost_output.or(existing.cost_output);
    let cost_cache_read = req.cost_cache_read.or(existing.cost_cache_read);
    let cost_cache_write = req.cost_cache_write.or(existing.cost_cache_write);
    let limit_context = req.limit_context.or(existing.limit_context);
    let limit_output = req.limit_output.or(existing.limit_output);
    let modalities_input = req.modalities_input.as_deref().or(existing.modalities_input.as_deref());
    let modalities_output = req.modalities_output.as_deref().or(existing.modalities_output.as_deref());
    let is_enabled = req.is_enabled.unwrap_or(existing.is_enabled);

    let result: sqlx::sqlite::SqliteQueryResult = sqlx::query!(
        "UPDATE models SET name = $1, family = $2, reasoning = $3, tool_call = $4, attachment = $5, structured_output = $6, temperature = $7, knowledge = $8, release_date = $9, open_weights = $10, cost_input = $11, cost_output = $12, cost_cache_read = $13, cost_cache_write = $14, limit_context = $15, limit_output = $16, modalities_input = $17, modalities_output = $18, is_enabled = $19 WHERE provider_id = $20 AND model_id = $21",
        name,
        family,
        reasoning,
        tool_call,
        attachment,
        structured_output,
        temperature,
        knowledge,
        release_date,
        open_weights,
        cost_input,
        cost_output,
        cost_cache_read,
        cost_cache_write,
        limit_context,
        limit_output,
        modalities_input,
        modalities_output,
        is_enabled,
        provider_id,
        model_id,
    )
    .execute(pool)
    .await?;

    Ok(result.rows_affected() > 0)
}

pub async fn delete_model(pool: &SqlitePool, provider_id: &str, model_id: &str) -> Result<bool> {
    let result: sqlx::sqlite::SqliteQueryResult = sqlx::query!(
        "DELETE FROM models WHERE provider_id = $1 AND model_id = $2 AND source = 'custom'",
        provider_id,
        model_id,
    )
    .execute(pool)
    .await?;

    Ok(result.rows_affected() > 0)
}
