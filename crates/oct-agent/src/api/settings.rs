//! Application settings: the typed surface over the `settings` KV table.
//! `llm_wire_log`, the LLM wire log switch, is persisted
//! for the next start AND flipped live on the process-wide
//! [`WireLogControl`](oct_llm_provider::wire::WireLogControl) in the same
//! request.

use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::AppState;
use super::error::ApiResult;
use crate::codemode;
use crate::db::settings as db;
use crate::tool_contract::McpToolMode;

#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
pub struct AppSettings {
    /// Whether LLM wire logging is currently in effect (the live control,
    /// not just the persisted row — a process started with
    /// `OCT_LLM_WIRE_LOG=off` reports `false` here regardless of the row).
    pub llm_wire_log: bool,
    /// MCP exposure for new root runs; workers retain direct tools.
    pub mcp_tool_mode: McpToolMode,
    pub code_mode_available: bool,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateAppSettings {
    /// Omitted keys are left untouched (the PUT is a partial update).
    pub llm_wire_log: Option<bool>,
    pub mcp_tool_mode: Option<McpToolMode>,
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
        mcp_tool_mode: codemode::mode(&state.pool, None).await?,
        code_mode_available: oct_codemode::worker_path().is_some(),
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
    if let Some(mode) = payload.mcp_tool_mode {
        db::set(&state.pool, codemode::SETTING, mode.as_str()).await?;
    }
    Ok(Json(AppSettings {
        llm_wire_log: state.wire_log.enabled(),
        mcp_tool_mode: codemode::mode(&state.pool, None).await?,
        code_mode_available: oct_codemode::worker_path().is_some(),
    }))
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ProjectMcpSettings {
    /// Null inherits the global mode.
    pub mcp_tool_mode: Option<McpToolMode>,
    pub effective_mcp_tool_mode: McpToolMode,
    pub code_mode_available: bool,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateProjectMcpSettings {
    /// Null restores inheritance.
    pub mcp_tool_mode: Option<McpToolMode>,
}

async fn require_project(state: &AppState, id: &str) -> ApiResult<()> {
    if crate::db::projects::get_project(&state.pool, id)
        .await?
        .is_none()
    {
        return Err(super::error::AppError::NotFound("Project not found".into()));
    }
    Ok(())
}

async fn project_view(state: &AppState, id: &str) -> ApiResult<Json<ProjectMcpSettings>> {
    let local = codemode::project_mode(&state.pool, id).await?;
    let effective = match local {
        Some(mode) => mode,
        None => codemode::mode(&state.pool, None).await?,
    };
    Ok(Json(ProjectMcpSettings {
        mcp_tool_mode: local,
        effective_mcp_tool_mode: effective,
        code_mode_available: oct_codemode::worker_path().is_some(),
    }))
}

#[utoipa::path(get, tag = "settings", path = "/projects/{id}/settings/mcp",
    params(("id" = String, Path, description = "Project ID")),
    responses((status = 200, description = "Project MCP exposure", body = ProjectMcpSettings)))]
pub async fn get_project_mcp_settings(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<ProjectMcpSettings>> {
    require_project(&state, &id).await?;
    project_view(&state, &id).await
}

#[utoipa::path(put, tag = "settings", path = "/projects/{id}/settings/mcp",
    params(("id" = String, Path, description = "Project ID")), request_body = UpdateProjectMcpSettings,
    responses((status = 200, description = "Updated project MCP exposure", body = ProjectMcpSettings)))]
pub async fn update_project_mcp_settings(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(payload): Json<UpdateProjectMcpSettings>,
) -> ApiResult<Json<ProjectMcpSettings>> {
    require_project(&state, &id).await?;
    codemode::set_project_mode(&state.pool, &id, payload.mcp_tool_mode).await?;
    project_view(&state, &id).await
}
