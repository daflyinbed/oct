use anyhow::{Context, Result};
use serde::Deserialize;
use tracing::info;

use crate::db;

const MODELS_DEV_URL: &str = "https://models.dev/api.json";

#[derive(Debug, Deserialize)]
struct ModelsDevResponse {
    #[serde(flatten)]
    providers: std::collections::BTreeMap<String, ProviderEntry>,
}

#[derive(Debug, Deserialize)]
struct ProviderEntry {
    id: String,
    name: String,
    doc: Option<String>,
    api: Option<String>,
    npm: Option<String>,
    #[serde(default)]
    models: std::collections::BTreeMap<String, ModelEntry>,
}

#[derive(Debug, Deserialize)]
struct ModelEntry {
    id: String,
    name: String,
    family: Option<String>,
    #[serde(default)]
    reasoning: bool,
    #[serde(default)]
    tool_call: bool,
    #[serde(default)]
    attachment: bool,
    #[serde(default)]
    structured_output: bool,
    #[serde(default = "default_true")]
    temperature: bool,
    knowledge: Option<String>,
    release_date: Option<String>,
    #[serde(default)]
    open_weights: bool,
    cost: Option<CostEntry>,
    limit: Option<LimitEntry>,
    modalities: Option<ModalitiesEntry>,
}

#[derive(Debug, Deserialize)]
struct CostEntry {
    input: Option<f64>,
    output: Option<f64>,
    cache_read: Option<f64>,
    cache_write: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct LimitEntry {
    context: Option<i64>,
    output: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct ModalitiesEntry {
    input: Option<Vec<String>>,
    output: Option<Vec<String>>,
}

fn default_true() -> bool {
    true
}

fn resolve_adapter_type(entry: &ProviderEntry) -> &'static str {
    match entry.npm.as_deref() {
        Some("@ai-sdk/anthropic") => "anthropic",
        _ => "openai_compatible",
    }
}

pub async fn run(database_url: &str) -> Result<()> {
    let pool = db::init_pool(database_url).await?;

    info!("Fetching model catalog from {MODELS_DEV_URL}");
    let client = reqwest::Client::new();
    let catalog: ModelsDevResponse = client
        .get(MODELS_DEV_URL)
        .send()
        .await
        .context("Failed to fetch models.dev API")?
        .json()
        .await
        .context("Failed to parse models.dev API response")?;

    let now = chrono::Utc::now().naive_utc();
    let mut providers_upserted = 0u64;
    let mut models_upserted = 0u64;

    for (_key, provider) in catalog.providers {
        let adapter_type = resolve_adapter_type(&provider);
        let base_url = provider.api.as_deref();

        let existing = db::providers::get_provider(&pool, &provider.id).await?;

        if let Some(existing) = existing {
            if existing.source == "seeded" {
                sqlx::query!(
                    "UPDATE providers SET name = $1, adapter_type = $2, base_url = $3, doc_url = $4, updated_at = $5 WHERE id = $6",
                    provider.name,
                    adapter_type,
                    base_url,
                    provider.doc,
                    now,
                    provider.id,
                )
                .execute(&pool)
                .await
                .context(format!("Failed to update provider '{}'", provider.id))?;
            }
        } else {
            sqlx::query!(
                "INSERT INTO providers (id, name, adapter_type, base_url, api_key, doc_url, source, created_at, updated_at) VALUES ($1, $2, $3, $4, '', $5, 'seeded', $6, $7)",
                provider.id,
                provider.name,
                adapter_type,
                base_url,
                provider.doc,
                now,
                now,
            )
            .execute(&pool)
            .await
            .context(format!("Failed to insert provider '{}'", provider.id))?;
        }
        providers_upserted += 1;

        for (_model_key, model) in provider.models {
            let model_row_id = format!("{}.{}", provider.id, model.id);

            let modalities_input = model
                .modalities
                .as_ref()
                .and_then(|m| m.input.as_ref())
                .map(|v| serde_json::to_string(v).unwrap_or_default());

            let modalities_output = model
                .modalities
                .as_ref()
                .and_then(|m| m.output.as_ref())
                .map(|v| serde_json::to_string(v).unwrap_or_default());

            let cost_input = model.cost.as_ref().and_then(|c| c.input);
            let cost_output = model.cost.as_ref().and_then(|c| c.output);
            let cost_cache_read = model.cost.as_ref().and_then(|c| c.cache_read);
            let cost_cache_write = model.cost.as_ref().and_then(|c| c.cache_write);
            let limit_context = model.limit.as_ref().and_then(|l| l.context);
            let limit_output = model.limit.as_ref().and_then(|l| l.output);

            let existing_model = sqlx::query_scalar!(
                "SELECT COUNT(*) FROM models WHERE id = $1",
                model_row_id,
            )
            .fetch_one(&pool)
            .await?;

            if existing_model > 0 {
                sqlx::query!(
                    "UPDATE models SET name = $1, family = $2, reasoning = $3, tool_call = $4, attachment = $5, structured_output = $6, temperature = $7, knowledge = $8, release_date = $9, open_weights = $10, cost_input = $11, cost_output = $12, cost_cache_read = $13, cost_cache_write = $14, limit_context = $15, limit_output = $16, modalities_input = $17, modalities_output = $18 WHERE id = $19",
                    model.name,
                    model.family,
                    model.reasoning,
                    model.tool_call,
                    model.attachment,
                    model.structured_output,
                    model.temperature,
                    model.knowledge,
                    model.release_date,
                    model.open_weights,
                    cost_input,
                    cost_output,
                    cost_cache_read,
                    cost_cache_write,
                    limit_context,
                    limit_output,
                    modalities_input,
                    modalities_output,
                    model_row_id,
                )
                .execute(&pool)
                .await
                .context(format!("Failed to update model '{}'", model_row_id))?;
            } else {
                sqlx::query!(
                    "INSERT INTO models (id, provider_id, model_id, name, family, reasoning, tool_call, attachment, structured_output, temperature, knowledge, release_date, open_weights, cost_input, cost_output, cost_cache_read, cost_cache_write, limit_context, limit_output, modalities_input, modalities_output, source, is_enabled, created_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, 'seeded', true, $22)",
                    model_row_id,
                    provider.id,
                    model.id,
                    model.name,
                    model.family,
                    model.reasoning,
                    model.tool_call,
                    model.attachment,
                    model.structured_output,
                    model.temperature,
                    model.knowledge,
                    model.release_date,
                    model.open_weights,
                    cost_input,
                    cost_output,
                    cost_cache_read,
                    cost_cache_write,
                    limit_context,
                    limit_output,
                    modalities_input,
                    modalities_output,
                    now,
                )
                .execute(&pool)
                .await
                .context(format!("Failed to insert model '{}'", model_row_id))?;
            }
            models_upserted += 1;
        }
    }

    info!("Seed complete: {providers_upserted} providers, {models_upserted} models upserted");
    Ok(())
}
