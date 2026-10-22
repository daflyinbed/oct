//! DB-driven model resolution — the single construction path for every
//! `ChatModel` the server builds. The API layer (send / resume / manual
//! compact / background title generation), the `task` tool and the crash
//! recovery orchestrator all resolve through
//! [`ModelResolver::resolve_model`], which
//! funnels into [`resolve_chat_model`] here; nothing else constructs
//! models.
//!
//! Resolution is per-run: every call re-reads the provider row, so a
//! settings change (base URL / API key) takes effect on the NEXT request —
//! no caching, no restart, no process-env re-read.
//!
//! Configuration sources, in priority order (per field):
//!
//! 1. the DB provider row (the settings page is the source of truth);
//! 2. the provider family's startup defaults — the built-in base URL plus
//!    the API key materialized from the environment when the registry was
//!    built (`ANTHROPIC_API_KEY` for the Anthropic family; custom
//!    OpenAI-compatible rows have NO env fallback).
//!
//! Blank DB values mean "not configured" and fall back, never "override
//! with empty" (see [`ConnectionConfig::from_row_fields`]).

use std::sync::Arc;

use oct_llm_provider::model::ChatModel;
use oct_llm_provider::provider::{ConnectionConfig, ProviderRegistry};
use sqlx::SqlitePool;

use crate::db::providers as provider_db;
use crate::error::AppError;

/// Registry key of the built-in Anthropic family: provider rows with
/// `adapter_type = 'anthropic'` resolve through it, the row's own
/// base_url/api_key overriding the family defaults.
const ANTHROPIC_FAMILY: &str = "anthropic";

/// Build the [`ChatModel`] for one provider row (pure: no DB access, so
/// the caller controls when the row snapshot is taken — per run).
pub(super) fn resolve_chat_model(
    registry: &ProviderRegistry,
    provider: &provider_db::Provider,
    provider_id: &str,
    model_id: &str,
) -> Result<Box<dyn ChatModel>, AppError> {
    let connection =
        ConnectionConfig::from_row_fields(provider.base_url.as_deref(), &provider.api_key);

    match provider.adapter_type.as_str() {
        // Built-in Anthropic family: the row's explicit base_url/api_key
        // drive the endpoint (a local mock, a proxy, a mirror — anything);
        // each unset field falls back to the family default the registry
        // materialized at startup. The row's own key wins even when the
        // environment also carries one, so the settings page stays the
        // single source of truth at runtime.
        "anthropic" => {
            let defaults = registry
                .provider_default_connection(ANTHROPIC_FAMILY)
                .ok_or(AppError::Internal(anyhow::anyhow!(
                    "the '{ANTHROPIC_FAMILY}' provider family is not registered"
                )))?;
            // Same fallback chain the adapter applies when building: keep
            // the resolution-time 400 (instead of a mid-run auth failure)
            // when NEITHER the row nor the env default supplies a key.
            connection
                .api_key
                .clone()
                .or(defaults.api_key)
                .ok_or_else(|| {
                    AppError::BadRequest(format!(
                        "Provider '{provider_id}' has no API key configured \
                         (set it on the provider or export ANTHROPIC_API_KEY)"
                    ))
                })?;
            registry
                .resolve_chat_model_with_connection(ANTHROPIC_FAMILY, model_id, &connection)
                .map_err(AppError::from)
        }
        // Custom OpenAI-compatible providers have no built-in defaults: the
        // row itself must carry both the endpoint and the key.
        "openai_compatible" => {
            let base_url = connection.base_url.clone().ok_or_else(|| {
                AppError::BadRequest(format!(
                    "Provider '{provider_id}' has no base_url configured"
                ))
            })?;
            let api_key = connection.api_key.clone().ok_or_else(|| {
                AppError::BadRequest(format!(
                    "Provider '{provider_id}' has no API key configured"
                ))
            })?;
            registry
                .resolve_custom_openai_model(provider_id, model_id, &base_url, &api_key)
                .map_err(AppError::from)
        }
        other => Err(AppError::BadRequest(format!(
            "Unknown adapter_type: '{other}'"
        ))),
    }
}

/// DB-driven model access shared by run assembly and independent title jobs.
/// It carries no sessions, toolsets, managers or run lifecycle.
#[derive(Clone)]
pub struct ModelResolver {
    pool: SqlitePool,
    registry: Arc<ProviderRegistry>,
}

impl ModelResolver {
    pub fn new(pool: SqlitePool, registry: Arc<ProviderRegistry>) -> Self {
        Self { pool, registry }
    }
    /// Resolve `provider_id:model_id` into a live `ChatModel` (moved here
    /// from the API layer so subagents and the recovery orchestrator resolve
    /// exactly like a chat turn does). Errors map 404/400 as before.
    ///
    /// Every call re-reads the provider row, so the DB configuration drives
    /// each request afresh — a settings change takes effect on the next
    /// run. Construction rules (adapter dispatch, DB-over-env fallback
    /// order) live in [`resolve_chat_model`].
    pub async fn resolve_model(
        &self,
        provider_spec: &str,
    ) -> Result<(Box<dyn ChatModel>, String, String), AppError> {
        let (provider_id, model_id) = provider_spec.split_once(':').ok_or_else(|| {
            AppError::BadRequest(format!(
                "Invalid provider_spec format: '{}'. Expected 'provider_id:model_id'",
                provider_spec
            ))
        })?;

        let provider = crate::db::providers::get_provider(&self.pool, provider_id)
            .await
            .map_err(AppError::from)?
            .ok_or_else(|| AppError::NotFound(format!("Provider '{}' not found", provider_id)))?;

        let model = resolve_chat_model(&self.registry, &provider, provider_id, model_id)?;

        Ok((model, provider_id.to_string(), model_id.to_string()))
    }

    /// Context window for compaction decisions: built-in provider info first
    /// (Anthropic/Moonshot carry their limits in code), then the models
    /// table's `limit_context` (seeded from models.dev or set on custom
    /// models). `None` means unknown — auto compaction stays disabled,
    /// manual still works.
    pub async fn resolve_context_window(
        &self,
        model: &dyn ChatModel,
        provider_id: &str,
        model_id: &str,
    ) -> Option<u32> {
        if let Some(window) = model.info().limits.max_input_tokens.filter(|w| *w > 0) {
            return Some(window);
        }
        let row = crate::db::models::get_model(&self.pool, provider_id, model_id)
            .await
            .ok()
            .flatten()?;
        row.limit_context
            .and_then(|v| u32::try_from(v).ok())
            .filter(|w| *w > 0)
    }

    /// Input modalities of the resolved model from the models table
    /// (`modalities_input`, seeded from models.dev or set on custom models).
    /// No row / NULL / unparsable → `Unknown`, which keeps the historical
    /// allow-everything gating behavior.
    pub async fn resolve_input_modalities(
        &self,
        provider_id: &str,
        model_id: &str,
    ) -> crate::modality::InputModalities {
        crate::db::models::get_model(&self.pool, provider_id, model_id)
            .await
            .ok()
            .flatten()
            .map(|row| crate::modality::InputModalities::parse(row.modalities_input.as_deref()))
            .unwrap_or(crate::modality::InputModalities::Unknown)
    }
}
