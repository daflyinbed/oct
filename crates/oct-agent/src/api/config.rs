use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::AppState;

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct AgentConfigResponse {
    /// Current provider:model spec.
    pub provider_spec: String,
    /// Working directory for file operations.
    pub working_dir: String,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateConfigRequest {
    /// New provider:model spec.
    pub provider_spec: Option<String>,
}

/// Get current agent configuration.
#[utoipa::path(
    get,
    path = "/config",
    responses(
        (status = 200, description = "Current configuration", body = AgentConfigResponse)
    ),
    tag = "config"
)]
pub async fn get_config(State(state): State<AppState>) -> Json<AgentConfigResponse> {
    let provider_spec = state
        .provider_spec
        .read()
        .map(|s| s.clone())
        .unwrap_or_default();

    Json(AgentConfigResponse {
        provider_spec,
        working_dir: state.working_dir.to_string_lossy().to_string(),
    })
}

/// Update agent configuration.
#[utoipa::path(
    put,
    path = "/config",
    request_body = UpdateConfigRequest,
    responses(
        (status = 200, description = "Configuration updated", body = AgentConfigResponse),
        (status = 400, description = "Invalid configuration")
    ),
    tag = "config"
)]
pub async fn update_config(
    State(state): State<AppState>,
    Json(req): Json<UpdateConfigRequest>,
) -> Result<Json<AgentConfigResponse>, StatusCode> {
    if let Some(ref spec) = req.provider_spec {
        // Validate the provider spec can be resolved
        state
            .registry
            .resolve_chat_model(spec)
            .map_err(|_| StatusCode::BAD_REQUEST)?;

        let mut current = state
            .provider_spec
            .write()
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        *current = spec.clone();
    }

    let provider_spec = state
        .provider_spec
        .read()
        .map(|s| s.clone())
        .unwrap_or_default();

    Ok(Json(AgentConfigResponse {
        provider_spec,
        working_dir: state.working_dir.to_string_lossy().to_string(),
    }))
}
