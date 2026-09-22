use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::AppState;
use super::error::{ApiResult, AppError};
use crate::db::mcp as db;
use crate::db::projects as project_db;
use crate::mcp::ServerStatus;

/// Transports the API accepts: `stdio` (local child process) and `http`
/// (Streamable HTTP remote endpoint).
const SUPPORTED_TRANSPORTS: [&str; 2] = ["stdio", "http"];

/// Query parameters of `GET /mcp/servers`.
#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListMcpServersQuery {
    /// Restrict the list to the servers effective for this project (global
    /// plus that project's scoped rows, enabled only). Omitted: every row.
    project_id: Option<String>,
}

/// One cached `tools/list` entry, as surfaced to the UI while the server is
/// connected. `description` is the raw server-provided text (no `[mcp:...]`
/// prefix, no LLM-facing truncation).
#[derive(Debug, Serialize, ToSchema)]
pub struct McpToolSummary {
    pub name: String,
    pub description: Option<String>,
}

/// An MCP server row merged with the connection manager's live status.
///
/// The storage fields mirror [`db::McpServer`] one-to-one (utoipa cannot
/// express `serde(flatten)`, so — like `ProviderResponse` for providers —
/// the response DTO lists them explicitly); `status`/`tool_count`/`error`/
/// `tools` come from the in-process manager without touching the DB. Rows
/// the manager holds no entry for (never connected since startup, or
/// disabled) report `not_connected`.
#[derive(Debug, Serialize, ToSchema)]
pub struct McpServerResponse {
    pub id: String,
    pub name: String,
    pub transport: String,
    pub command: Option<String>,
    pub args: Option<Vec<String>>,
    pub env: Option<HashMap<String, String>>,
    pub url: Option<String>,
    pub headers: Option<HashMap<String, String>>,
    pub enabled: bool,
    pub tool_timeout_ms: Option<i64>,
    /// Owning project: `null` = global (every project), otherwise the id of
    /// the single project this server is visible to.
    pub project_id: Option<String>,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
    /// `connected` | `failed` | `not_connected`.
    pub status: String,
    /// Number of tools the server exposes while connected; `null` otherwise.
    pub tool_count: Option<usize>,
    /// Last connection error while `status` is `failed`; `null` otherwise.
    pub error: Option<String>,
    /// Cached `tools/list` while connected; `null` otherwise.
    pub tools: Option<Vec<McpToolSummary>>,
}

fn to_mcp_server_response(
    server: &db::McpServer,
    status: Option<ServerStatus>,
    tools: Vec<oct_mcp::McpToolInfo>,
) -> McpServerResponse {
    let (status, tool_count, error, tools) = match status {
        Some(ServerStatus::Connected { tool_count }) => (
            "connected",
            Some(tool_count),
            None,
            Some(
                tools
                    .into_iter()
                    .map(|tool| McpToolSummary {
                        name: tool.name,
                        description: tool.description,
                    })
                    .collect::<Vec<_>>(),
            ),
        ),
        Some(ServerStatus::Failed { error }) => ("failed", None, Some(error), None),
        None => ("not_connected", None, None, None),
    };
    McpServerResponse {
        id: server.id.clone(),
        name: server.name.clone(),
        transport: server.transport.clone(),
        command: server.command.clone(),
        args: server.args.as_deref().cloned(),
        env: server.env.as_deref().cloned(),
        url: server.url.clone(),
        headers: server.headers.as_deref().cloned(),
        enabled: server.enabled,
        tool_timeout_ms: server.tool_timeout_ms,
        project_id: server.project_id.clone(),
        created_at: server.created_at,
        updated_at: server.updated_at,
        status: status.to_string(),
        tool_count,
        error,
        tools,
    }
}

/// Shared create/update validation. Transport, command, url and timeout
/// rules are enforced here so both entry points stay in lockstep; `args`/
/// `env`/`headers` are already structurally validated by their
/// `Vec<String>` / `HashMap<String, String>` request types (serde rejects
/// wrong shapes before the handler runs).
fn validate_mcp_fields(
    name: Option<&str>,
    transport: Option<&str>,
    command: Option<&str>,
    url: Option<&str>,
    tool_timeout_ms: Option<i64>,
) -> Result<(), AppError> {
    if name.is_some_and(|name| name.trim().is_empty()) {
        return Err(AppError::BadRequest("name must not be empty".into()));
    }

    if let Some(transport) = transport {
        if !SUPPORTED_TRANSPORTS.contains(&transport) {
            return Err(AppError::BadRequest(format!(
                "unsupported transport '{transport}'; supported: {}",
                SUPPORTED_TRANSPORTS.join(", ")
            )));
        }
    }

    if command.is_some_and(|command| command.trim().is_empty()) {
        return Err(AppError::BadRequest("command must not be empty".into()));
    }

    if url.is_some_and(|url| url.trim().is_empty()) {
        return Err(AppError::BadRequest("url must not be empty".into()));
    }

    if tool_timeout_ms.is_some_and(|timeout| timeout <= 0) {
        return Err(AppError::BadRequest(format!(
            "tool_timeout_ms must be a positive number of milliseconds, got {tool_timeout_ms:?}"
        )));
    }

    Ok(())
}

/// Cross-field check tying the transport to its addressing fields. Semantics:
/// `stdio` needs a non-blank `command` (nothing to run otherwise); `http`
/// needs an http(s) `url`. The reverse residue is deliberately tolerated —
/// a `stdio` row may keep a stale `url`/`headers` (and an `http` row a stale
/// `command`), so switching transports back and forth never destroys the
/// other side's config.
fn validate_transport_fields(
    transport: &str,
    command: Option<&str>,
    url: Option<&str>,
) -> Result<(), AppError> {
    match transport {
        "stdio" => {
            if command.is_none_or(|command| command.trim().is_empty()) {
                return Err(AppError::BadRequest(
                    "stdio transport requires a command".into(),
                ));
            }
        }
        "http" => {
            let url = url.map(str::trim).filter(|url| !url.is_empty());
            match url {
                None => {
                    return Err(AppError::BadRequest("http transport requires a url".into()));
                }
                Some(url) => {
                    // Cheap structural guard (full validity is only knowable
                    // at connect time); catches the obvious missing-scheme
                    // typo before it becomes an opaque connect failure.
                    if !url.starts_with("http://") && !url.starts_with("https://") {
                        return Err(AppError::BadRequest(
                            "http transport url must start with http:// or https://".into(),
                        ));
                    }
                }
            }
        }
        other => {
            return Err(AppError::BadRequest(format!(
                "unsupported transport '{other}'; supported: {}",
                SUPPORTED_TRANSPORTS.join(", ")
            )));
        }
    }
    Ok(())
}

/// Reject an MCP server scoped to a project that does not exist (the FK
/// would otherwise surface as a 500). Same error style as the conversations
/// endpoints' project handling.
async fn reject_unknown_project(state: &AppState, project_id: &str) -> Result<(), AppError> {
    if project_db::get_project(&state.pool, project_id)
        .await?
        .is_none()
    {
        return Err(AppError::NotFound("Project not found".into()));
    }
    Ok(())
}

#[utoipa::path(
    get,
    path = "/mcp/servers",
    params(ListMcpServersQuery),
    responses(
        (status = 200, description = "List of MCP servers with live connection status; with `project_id`, the project's effective set (global + project-scoped, enabled only)", body = Vec<McpServerResponse>),
        (status = 404, description = "Project not found (only when `project_id` is given)")
    ),
    tag = "mcp"
)]
pub async fn list_mcp_servers(
    State(state): State<AppState>,
    Query(query): Query<ListMcpServersQuery>,
) -> ApiResult<Json<Vec<McpServerResponse>>> {
    let servers = match query.project_id.as_deref() {
        Some(project_id) => {
            reject_unknown_project(&state, project_id).await?;
            db::list_effective_mcp_servers(&state.pool, project_id).await?
        }
        None => db::list_mcp_servers(&state.pool).await?,
    };
    let result = servers
        .iter()
        .map(|server| {
            to_mcp_server_response(
                server,
                state.mcp.status_of(&server.id),
                state.mcp.tools_of(&server.id),
            )
        })
        .collect();
    Ok(Json(result))
}

/// Reconcile + read in one round trip for the status-bar indicator: connects
/// the project's effective set (global + project-scoped) before returning it,
/// so a fresh conversation shows live statuses without waiting for the first
/// chat turn. Reconcile is the same best-effort pass a chat turn runs — a
/// server that cannot be spawned reports `failed` here, never a 5xx.
#[utoipa::path(
    post,
    path = "/projects/{projectId}/mcp/ensure",
    params(("projectId" = String, Path, description = "Project ID")),
    responses(
        (status = 200, description = "The project's effective MCP set (global + project-scoped, enabled only) with post-reconcile live status", body = Vec<McpServerResponse>),
        (status = 404, description = "Project not found")
    ),
    tag = "mcp"
)]
pub async fn ensure_project_mcp_servers(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
) -> ApiResult<Json<Vec<McpServerResponse>>> {
    reject_unknown_project(&state, &project_id).await?;
    state
        .mcp
        .ensure_connections(&state.pool, Some(&project_id))
        .await;
    let servers = db::list_effective_mcp_servers(&state.pool, &project_id).await?;
    let result = servers
        .iter()
        .map(|server| {
            to_mcp_server_response(
                server,
                state.mcp.status_of(&server.id),
                state.mcp.tools_of(&server.id),
            )
        })
        .collect();
    Ok(Json(result))
}

#[utoipa::path(
    post,
    path = "/mcp/servers",
    request_body = db::CreateMcpServerRequest,
    responses(
        (status = 201, description = "Created MCP server (live status is `not_connected` until the next chat turn reconciles)", body = McpServerResponse),
        (status = 400, description = "Invalid transport, missing command (stdio) or url (http), empty name, or non-positive timeout"),
        (status = 404, description = "project_id does not reference an existing project"),
        (status = 409, description = "MCP server name already exists")
    ),
    tag = "mcp"
)]
pub async fn create_mcp_server(
    State(state): State<AppState>,
    Json(req): Json<db::CreateMcpServerRequest>,
) -> ApiResult<(StatusCode, Json<McpServerResponse>)> {
    validate_mcp_fields(
        Some(&req.name),
        Some(&req.transport),
        req.command.as_deref(),
        req.url.as_deref(),
        req.tool_timeout_ms,
    )?;
    // stdio launches a local process (command required); http reaches a
    // remote endpoint (url required). Cross-residue is tolerated and stored.
    validate_transport_fields(&req.transport, req.command.as_deref(), req.url.as_deref())?;

    if let Some(project_id) = req.project_id.as_deref() {
        reject_unknown_project(&state, project_id).await?;
    }

    if db::get_mcp_server_by_name(&state.pool, &req.name)
        .await?
        .is_some()
    {
        return Err(AppError::Conflict(format!(
            "MCP server '{}' already exists",
            req.name
        )));
    }

    let server = db::create_mcp_server(&state.pool, &req).await?;
    let response = to_mcp_server_response(&server, state.mcp.status_of(&server.id), Vec::new());
    Ok((StatusCode::CREATED, Json(response)))
}

#[utoipa::path(
    get,
    path = "/mcp/servers/{id}",
    params(("id" = String, Path, description = "MCP server ID")),
    responses(
        (status = 200, description = "MCP server found, with live connection status", body = McpServerResponse),
        (status = 404, description = "MCP server not found")
    ),
    tag = "mcp"
)]
pub async fn get_mcp_server(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<McpServerResponse>> {
    let server = db::get_mcp_server(&state.pool, &id)
        .await?
        .ok_or_else(|| AppError::NotFound("MCP server not found".into()))?;
    Ok(Json(to_mcp_server_response(
        &server,
        state.mcp.status_of(&server.id),
        state.mcp.tools_of(&server.id),
    )))
}

#[utoipa::path(
    patch,
    path = "/mcp/servers/{id}",
    params(("id" = String, Path, description = "MCP server ID")),
    request_body = db::UpdateMcpServerRequest,
    responses(
        (status = 200, description = "MCP server updated"),
        (status = 400, description = "Invalid transport, empty command/name/url, or non-positive timeout"),
        (status = 404, description = "MCP server not found, or project_id does not reference an existing project"),
        (status = 409, description = "MCP server name already exists")
    ),
    tag = "mcp"
)]
pub async fn update_mcp_server(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<db::UpdateMcpServerRequest>,
) -> ApiResult<StatusCode> {
    validate_mcp_fields(
        req.name.as_deref(),
        req.transport.as_deref(),
        req.command.as_deref(),
        req.url.as_deref(),
        req.tool_timeout_ms,
    )?;

    // Fetch the row first: the transport↔addressing cross-check runs on the
    // EFFECTIVE post-merge values (PATCH semantics: provide = replace,
    // absent = keep), so "PATCH {transport: http}" on a row without a url is
    // a 400 here — while the same PATCH with a url (or on a row that already
    // has one) passes.
    let existing = db::get_mcp_server(&state.pool, &id)
        .await?
        .ok_or_else(|| AppError::NotFound("MCP server not found".into()))?;
    let transport = req.transport.as_deref().unwrap_or(&existing.transport);
    let command = req.command.as_deref().or(existing.command.as_deref());
    let url = req.url.as_deref().or(existing.url.as_deref());
    validate_transport_fields(transport, command, url)?;

    if let Some(project_id) = req.project_id.as_deref() {
        reject_unknown_project(&state, project_id).await?;
    }

    if let Some(name) = req.name.as_deref() {
        let conflict = db::get_mcp_server_by_name(&state.pool, name).await?;
        if conflict.is_some_and(|other| other.id != id) {
            return Err(AppError::Conflict(format!(
                "MCP server '{name}' already exists"
            )));
        }
    }

    let updated = db::update_mcp_server(&state.pool, &id, &req).await?;
    if updated {
        Ok(StatusCode::OK)
    } else {
        Err(AppError::NotFound("MCP server not found".into()))
    }
}

#[utoipa::path(
    delete,
    path = "/mcp/servers/{id}",
    params(("id" = String, Path, description = "MCP server ID")),
    responses(
        (status = 204, description = "MCP server deleted"),
        (status = 404, description = "MCP server not found")
    ),
    tag = "mcp"
)]
pub async fn delete_mcp_server(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let deleted = db::delete_mcp_server(&state.pool, &id).await?;
    if deleted {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound("MCP server not found".into()))
    }
}
