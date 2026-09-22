use std::collections::HashMap;

use anyhow::Result;
use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use sqlx::types::Json;
use utoipa::ToSchema;
use uuid::Uuid;

/// A registered MCP server the agent can pull tools from.
///
/// Phase 1 covers the `stdio` transport only (`command` + `args` + `env`);
/// `url`/`headers` are stored for the Phase 2 `http` transport but unused.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct McpServer {
    pub id: String,
    pub name: String,
    /// Transport type: `stdio` today, `http` reserved for Phase 2.
    pub transport: String,
    /// Executable to launch (stdio transport).
    pub command: Option<String>,
    /// argv for the stdio launch, stored as a JSON array.
    #[schema(value_type = Option<Vec<String>>)]
    pub args: Option<Json<Vec<String>>>,
    /// Environment variables for the stdio launch, stored as a JSON object.
    #[schema(value_type = Option<HashMap<String, String>>)]
    pub env: Option<Json<HashMap<String, String>>>,
    /// Remote endpoint (http transport, Phase 2).
    pub url: Option<String>,
    /// Request headers (http transport, Phase 2), stored as a JSON object.
    #[schema(value_type = Option<HashMap<String, String>>)]
    pub headers: Option<Json<HashMap<String, String>>>,
    /// Disabled servers stay configured but are not launched.
    pub enabled: bool,
    /// Per-server tool-call timeout override, in milliseconds.
    pub tool_timeout_ms: Option<i64>,
    /// Owning project: `NULL` = global (visible to every project), non-NULL
    /// = visible only to that project's runs. A run's effective set is the
    /// two-level merge "global + own project".
    pub project_id: Option<String>,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateMcpServerRequest {
    pub name: String,
    /// Transport type; only `stdio` is accepted in Phase 1.
    pub transport: String,
    pub command: Option<String>,
    pub args: Option<Vec<String>>,
    pub env: Option<HashMap<String, String>>,
    pub url: Option<String>,
    pub headers: Option<HashMap<String, String>>,
    /// Defaults to enabled.
    pub enabled: Option<bool>,
    pub tool_timeout_ms: Option<i64>,
    /// Owning project (`None` = global). Must reference an existing project.
    pub project_id: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateMcpServerRequest {
    pub name: Option<String>,
    /// Only `stdio` is accepted in Phase 1.
    pub transport: Option<String>,
    pub command: Option<String>,
    pub args: Option<Vec<String>>,
    pub env: Option<HashMap<String, String>>,
    pub url: Option<String>,
    pub headers: Option<HashMap<String, String>>,
    pub enabled: Option<bool>,
    pub tool_timeout_ms: Option<i64>,
    /// Set the owning project; absent leaves the scope unchanged.
    pub project_id: Option<String>,
}

pub async fn list_mcp_servers(pool: &SqlitePool) -> Result<Vec<McpServer>> {
    let rows = sqlx::query_as!(
        McpServer,
        // The `: _` overrides decode the JSON text columns straight into the
        // record's Json<...> fields (the macro's default TEXT mapping does
        // not know Json is compatible), and the `: bool` override widens the
        // `enabled INTEGER` column to the record's `bool` field (unlike
        // models.is_enabled the column is declared INTEGER, not BOOLEAN, so
        // the macro needs the hint).
        // Every enabled row across ALL projects: the connection manager's
        // reconcile needs the full picture (removals are scope-independent),
        // so no scope filter lives here.
        "SELECT id, name, transport, command, args_json AS \"args: _\", env_json AS \"env: _\", url, headers_json AS \"headers: _\", enabled AS \"enabled: bool\", tool_timeout_ms, project_id, created_at, updated_at FROM mcp_servers ORDER BY created_at ASC"
    )
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

/// The effective MCP servers for one project's runs: enabled rows that are
/// global (`project_id IS NULL`) or scoped to `project_id` — the two-level
/// "global + own project" merge. Disabled and other projects' rows are
/// excluded.
pub async fn list_effective_mcp_servers(
    pool: &SqlitePool,
    project_id: &str,
) -> Result<Vec<McpServer>> {
    let rows = sqlx::query_as!(
        McpServer,
        "SELECT id, name, transport, command, args_json AS \"args: _\", env_json AS \"env: _\", url, headers_json AS \"headers: _\", enabled AS \"enabled: bool\", tool_timeout_ms, project_id, created_at, updated_at FROM mcp_servers WHERE enabled = 1 AND (project_id IS NULL OR project_id = $1) ORDER BY created_at ASC",
        project_id,
    )
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

pub async fn get_mcp_server(pool: &SqlitePool, id: &str) -> Result<Option<McpServer>> {
    let row = sqlx::query_as!(
        McpServer,
        "SELECT id, name, transport, command, args_json AS \"args: _\", env_json AS \"env: _\", url, headers_json AS \"headers: _\", enabled AS \"enabled: bool\", tool_timeout_ms, project_id, created_at, updated_at FROM mcp_servers WHERE id = $1",
        id,
    )
    .fetch_optional(pool)
    .await?;

    Ok(row)
}

/// Names are the user-facing identity of a server (and UNIQUE in the table);
/// the API layer uses this to turn create/update collisions into 409s.
pub async fn get_mcp_server_by_name(pool: &SqlitePool, name: &str) -> Result<Option<McpServer>> {
    let row = sqlx::query_as!(
        McpServer,
        "SELECT id, name, transport, command, args_json AS \"args: _\", env_json AS \"env: _\", url, headers_json AS \"headers: _\", enabled AS \"enabled: bool\", tool_timeout_ms, project_id, created_at, updated_at FROM mcp_servers WHERE name = $1",
        name,
    )
    .fetch_optional(pool)
    .await?;

    Ok(row)
}

pub async fn create_mcp_server(
    pool: &SqlitePool,
    req: &CreateMcpServerRequest,
) -> Result<McpServer> {
    let id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().naive_utc();
    let enabled = req.enabled.unwrap_or(true);
    // The query! macro borrows its arguments later in the statement, so the
    // Json wrappers must outlive the macro invocation.
    let args = req.args.clone().map(Json);
    let env = req.env.clone().map(Json);
    let headers = req.headers.clone().map(Json);

    sqlx::query!(
        "INSERT INTO mcp_servers (id, name, transport, command, args_json, env_json, url, headers_json, enabled, tool_timeout_ms, project_id, created_at, updated_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
        id,
        req.name,
        req.transport,
        req.command,
        args,
        env,
        req.url,
        headers,
        enabled,
        req.tool_timeout_ms,
        req.project_id,
        now,
        now,
    )
    .execute(pool)
    .await?;

    Ok(McpServer {
        id,
        name: req.name.clone(),
        transport: req.transport.clone(),
        command: req.command.clone(),
        args: req.args.clone().map(Json),
        env: req.env.clone().map(Json),
        url: req.url.clone(),
        headers: req.headers.clone().map(Json),
        enabled,
        tool_timeout_ms: req.tool_timeout_ms,
        project_id: req.project_id.clone(),
        created_at: now,
        updated_at: now,
    })
}

pub async fn update_mcp_server(
    pool: &SqlitePool,
    id: &str,
    req: &UpdateMcpServerRequest,
) -> Result<bool> {
    let existing = sqlx::query_as!(
        McpServer,
        "SELECT id, name, transport, command, args_json AS \"args: _\", env_json AS \"env: _\", url, headers_json AS \"headers: _\", enabled AS \"enabled: bool\", tool_timeout_ms, project_id, created_at, updated_at FROM mcp_servers WHERE id = $1",
        id,
    )
    .fetch_optional(pool)
    .await?;

    let Some(existing) = existing else {
        return Ok(false);
    };

    let now = chrono::Utc::now().naive_utc();
    let name = req.name.as_deref().unwrap_or(&existing.name);
    let transport = req.transport.as_deref().unwrap_or(&existing.transport);
    let command = req.command.as_deref().or(existing.command.as_deref());
    let url = req.url.as_deref().or(existing.url.as_deref());
    let enabled = req.enabled.unwrap_or(existing.enabled);
    let tool_timeout_ms = req.tool_timeout_ms.or(existing.tool_timeout_ms);
    // Present → replace the scope; absent → keep the current one (same
    // optionality contract as every other field on this update path).
    let project_id = req.project_id.clone().or(existing.project_id.clone());
    let args = req.args.clone().map(Json).or_else(|| existing.args.clone());
    let env = req.env.clone().map(Json).or_else(|| existing.env.clone());
    let headers = req
        .headers
        .clone()
        .map(Json)
        .or_else(|| existing.headers.clone());

    let result: sqlx::sqlite::SqliteQueryResult = sqlx::query!(
        "UPDATE mcp_servers SET name = $1, transport = $2, command = $3, args_json = $4, env_json = $5, url = $6, headers_json = $7, enabled = $8, tool_timeout_ms = $9, project_id = $10, updated_at = $11 WHERE id = $12",
        name,
        transport,
        command,
        args,
        env,
        url,
        headers,
        enabled,
        tool_timeout_ms,
        project_id,
        now,
        id,
    )
    .execute(pool)
    .await?;

    Ok(result.rows_affected() > 0)
}

pub async fn delete_mcp_server(pool: &SqlitePool, id: &str) -> Result<bool> {
    let result: sqlx::sqlite::SqliteQueryResult =
        sqlx::query!("DELETE FROM mcp_servers WHERE id = $1", id)
            .execute(pool)
            .await?;

    Ok(result.rows_affected() > 0)
}
