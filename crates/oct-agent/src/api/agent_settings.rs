//! Agent defaults and a read-only tool picker catalog, in both settings scopes.
use super::{
    AppState,
    error::{ApiResult, AppError},
};
use crate::agent_settings::{
    self as config, AgentPatch, AgentSettingsView, ToolChoice, ToolSelector,
};
use axum::{
    Json,
    extract::{Path, State},
};
use serde::Serialize;
use utoipa::ToSchema;

#[derive(Debug, Serialize, ToSchema)]
pub struct AgentToolOption {
    pub selector: ToolSelector,
    pub label: String,
    pub group: String,
    pub available: bool,
}

fn validate_catalog(state: &AppState, patch: &AgentPatch) -> ApiResult<()> {
    if let Some(Some(ToolChoice::Allowlist { items })) = &patch.tools {
        for item in items {
            if let ToolSelector::Mcp { server_id, tool } = item
                && matches!(
                    state.mcp.status_of(server_id),
                    Some(crate::mcp::manager::ServerStatus::Connected { .. })
                )
                && !state
                    .mcp
                    .tools_of(server_id)
                    .iter()
                    .any(|t| &t.name == tool)
            {
                return Err(AppError::BadRequest(format!(
                    "MCP tool no longer exists: {tool}"
                )));
            }
        }
    }
    Ok(())
}

async fn catalog(state: &AppState, project: Option<&str>) -> ApiResult<Vec<AgentToolOption>> {
    if let Some(id) = project {
        config::read(&state.pool, "worker", Some(id)).await?;
    }
    // Discover enabled servers when opening the picker, including before the
    // first chat turn. Reconciliation shares existing connections and isolates
    // failures per server; unavailable saved selections remain editable.
    state.mcp.ensure_connections(&state.pool, project).await;
    let mut options: Vec<_> = config::BUILTIN_TOOLS
        .iter()
        .map(|name| AgentToolOption {
            selector: ToolSelector::Builtin {
                name: (*name).into(),
            },
            label: (*name).into(),
            group: "内置工具".into(),
            available: true,
        })
        .collect();
    for server in crate::db::mcp::list_mcp_servers(&state.pool).await? {
        if server.project_id.is_some() && server.project_id.as_deref() != project {
            continue;
        }
        for tool in state.mcp.tools_of(&server.id) {
            options.push(AgentToolOption {
                selector: ToolSelector::Mcp {
                    server_id: server.id.clone(),
                    tool: tool.name.clone(),
                },
                label: tool.name,
                group: server.name.clone(),
                available: server.enabled
                    && matches!(
                        state.mcp.status_of(&server.id),
                        Some(crate::mcp::manager::ServerStatus::Connected { .. })
                    ),
            });
        }
    }
    Ok(options)
}

#[utoipa::path(get, path = "/settings/agents", tag = "settings", responses((status = 200, body = Vec<AgentSettingsView>), (status = 400, body = super::error::ErrorBody), (status = 404, body = super::error::ErrorBody), (status = 500, body = super::error::ErrorBody)))]
pub async fn list_global(State(state): State<AppState>) -> ApiResult<Json<Vec<AgentSettingsView>>> {
    Ok(Json(config::list(&state.pool, None).await?))
}

#[utoipa::path(get, path = "/settings/agents/{key}", tag = "settings", params(("key" = String, Path)), responses((status = 200, body = AgentSettingsView), (status = 400, body = super::error::ErrorBody), (status = 404, body = super::error::ErrorBody), (status = 500, body = super::error::ErrorBody)))]
pub async fn get_global(
    State(state): State<AppState>,
    Path(key): Path<String>,
) -> ApiResult<Json<AgentSettingsView>> {
    Ok(Json(config::read(&state.pool, &key, None).await?))
}

#[utoipa::path(patch, path = "/settings/agents/{key}", tag = "settings", params(("key" = String, Path)), request_body = AgentPatch, responses((status = 200, body = AgentSettingsView), (status = 400, body = super::error::ErrorBody), (status = 404, body = super::error::ErrorBody), (status = 500, body = super::error::ErrorBody)))]
pub async fn update_global(
    State(state): State<AppState>,
    Path(key): Path<String>,
    payload: Result<Json<AgentPatch>, axum::extract::rejection::JsonRejection>,
) -> ApiResult<Json<AgentSettingsView>> {
    let patch = payload
        .map_err(|error| AppError::BadRequest(error.body_text()))?
        .0;
    validate_catalog(&state, &patch)?;
    Ok(Json(config::update(&state.pool, &key, None, patch).await?))
}

#[utoipa::path(get, path = "/projects/{id}/settings/agents", tag = "settings", params(("id" = String, Path)), responses((status = 200, body = Vec<AgentSettingsView>), (status = 400, body = super::error::ErrorBody), (status = 404, body = super::error::ErrorBody), (status = 500, body = super::error::ErrorBody)))]
pub async fn list_project(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<AgentSettingsView>>> {
    Ok(Json(config::list(&state.pool, Some(&id)).await?))
}

#[utoipa::path(get, path = "/projects/{id}/settings/agents/{key}", tag = "settings", params(("id" = String, Path), ("key" = String, Path)), responses((status = 200, body = AgentSettingsView), (status = 400, body = super::error::ErrorBody), (status = 404, body = super::error::ErrorBody), (status = 500, body = super::error::ErrorBody)))]
pub async fn get_project(
    State(state): State<AppState>,
    Path((id, key)): Path<(String, String)>,
) -> ApiResult<Json<AgentSettingsView>> {
    Ok(Json(config::read(&state.pool, &key, Some(&id)).await?))
}

#[utoipa::path(patch, path = "/projects/{id}/settings/agents/{key}", tag = "settings", params(("id" = String, Path), ("key" = String, Path)), request_body = AgentPatch, responses((status = 200, body = AgentSettingsView), (status = 400, body = super::error::ErrorBody), (status = 404, body = super::error::ErrorBody), (status = 500, body = super::error::ErrorBody)))]
pub async fn update_project(
    State(state): State<AppState>,
    Path((id, key)): Path<(String, String)>,
    payload: Result<Json<AgentPatch>, axum::extract::rejection::JsonRejection>,
) -> ApiResult<Json<AgentSettingsView>> {
    let patch = payload
        .map_err(|error| AppError::BadRequest(error.body_text()))?
        .0;
    validate_catalog(&state, &patch)?;
    Ok(Json(
        config::update(&state.pool, &key, Some(&id), patch).await?,
    ))
}

#[utoipa::path(get, path = "/settings/agent-tools", tag = "settings", responses((status = 200, body = Vec<AgentToolOption>), (status = 400, body = super::error::ErrorBody), (status = 404, body = super::error::ErrorBody), (status = 500, body = super::error::ErrorBody)))]
pub async fn global_tools(State(state): State<AppState>) -> ApiResult<Json<Vec<AgentToolOption>>> {
    Ok(Json(catalog(&state, None).await?))
}

#[utoipa::path(get, path = "/projects/{id}/settings/agent-tools", tag = "settings", params(("id" = String, Path)), responses((status = 200, body = Vec<AgentToolOption>), (status = 400, body = super::error::ErrorBody), (status = 404, body = super::error::ErrorBody), (status = 500, body = super::error::ErrorBody)))]
pub async fn project_tools(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<AgentToolOption>>> {
    Ok(Json(catalog(&state, Some(&id)).await?))
}
