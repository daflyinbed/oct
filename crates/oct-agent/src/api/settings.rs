//! Application settings: the typed surface over the `settings` KV table.
//! v1 holds one key — `llm_wire_log`, the LLM wire log switch — persisted
//! for the next start AND flipped live on the process-wide
//! [`WireLogControl`](oct_llm_provider::wire::WireLogControl) in the same
//! request.

use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::AppState;
use super::error::ApiResult;
use crate::db::settings as db;

#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
pub struct AppSettings {
    /// Whether LLM wire logging is currently in effect (the live control,
    /// not just the persisted row — a process started with
    /// `OCT_LLM_WIRE_LOG=off` reports `false` here regardless of the row).
    pub llm_wire_log: bool,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateAppSettings {
    /// Omitted keys are left untouched (the PUT is a partial update).
    pub llm_wire_log: Option<bool>,
}

#[utoipa::path(
    get,
    tag = "settings",
    path = "/settings",
    responses(
        (status = 200, description = "Current application settings", body = AppSettings),
    )
)]
pub async fn get_settings(State(state): State<AppState>) -> ApiResult<Json<AppSettings>> {
    Ok(Json(AppSettings {
        llm_wire_log: state.wire_log.enabled(),
    }))
}

#[utoipa::path(
    put,
    tag = "settings",
    path = "/settings",
    request_body = UpdateAppSettings,
    responses(
        (status = 200, description = "Settings after applying the update", body = AppSettings),
    )
)]
pub async fn update_settings(
    State(state): State<AppState>,
    Json(payload): Json<UpdateAppSettings>,
) -> ApiResult<Json<AppSettings>> {
    if let Some(on) = payload.llm_wire_log {
        db::set(
            &state.pool,
            db::KEY_LLM_WIRE_LOG,
            if on { "on" } else { "off" },
        )
        .await?;
        state.wire_log.set_enabled(on);
    }
    Ok(Json(AppSettings {
        llm_wire_log: state.wire_log.enabled(),
    }))
}
