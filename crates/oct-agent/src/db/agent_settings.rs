//! Transaction-scoped storage and reference facts for agent configuration.
use sqlx::SqliteConnection;

use crate::agent_settings::AgentKey;
use crate::agent_settings::AgentSettings;
use crate::agent_settings::ModelChoice;
use crate::agent_settings::ReferenceSnapshot;
use crate::agent_settings::ToolChoice;
use crate::error::{AppError, AppResult};
use crate::tool_contract::ToolSelector;

pub(crate) async fn read_global(
    conn: &mut SqliteConnection,
    key: AgentKey,
) -> AppResult<Option<String>> {
    let storage_key = format!("agent.{}", key.as_str());
    Ok(
        sqlx::query_scalar!("SELECT value FROM settings WHERE key = ?", storage_key)
            .fetch_optional(conn)
            .await?,
    )
}

pub(crate) async fn read_project(
    conn: &mut SqliteConnection,
    key: AgentKey,
    id: &str,
) -> AppResult<Option<String>> {
    if sqlx::query_scalar!("SELECT id FROM projects WHERE id = ?", id)
        .fetch_optional(&mut *conn)
        .await?
        .is_none()
    {
        return Err(AppError::NotFound("Project not found".into()));
    }
    let storage_key = format!("agent.{}", key.as_str());
    Ok(sqlx::query_scalar!(
        "SELECT value FROM project_settings WHERE project_id = ? AND key = ?",
        id,
        storage_key
    )
    .fetch_optional(conn)
    .await?)
}

pub(crate) async fn references(
    conn: &mut SqliteConnection,
    value: &AgentSettings,
) -> AppResult<ReferenceSnapshot> {
    let model_tool_call = if let ModelChoice::Fixed {
        provider_id,
        model_id,
    } = &value.model
    {
        sqlx::query!(
            "SELECT tool_call FROM models WHERE provider_id = ? AND model_id = ?",
            provider_id,
            model_id
        )
        .fetch_optional(&mut *conn)
        .await?
        .map(|row| row.tool_call)
    } else {
        None
    };
    let mut servers = std::collections::HashMap::new();
    if let ToolChoice::Allowlist { items } = &value.tools {
        for item in items {
            if let ToolSelector::Mcp { server_id, .. } = item {
                let row =
                    sqlx::query!("SELECT project_id FROM mcp_servers WHERE id = ?", server_id)
                        .fetch_optional(&mut *conn)
                        .await?;
                if let Some(row) = row {
                    servers.insert(server_id.clone(), row.project_id);
                }
            }
        }
    }
    Ok(ReferenceSnapshot {
        model_tool_call,
        servers,
    })
}

pub(crate) async fn write_override(
    conn: &mut SqliteConnection,
    key: AgentKey,
    project: Option<&str>,
    value: &crate::agent_settings::AgentOverride,
) -> AppResult<()> {
    let storage_key = format!("agent.{}", key.as_str());
    let json = serde_json::to_string(value).map_err(anyhow::Error::from)?;
    if let Some(id) = project {
        sqlx::query!("INSERT INTO project_settings (project_id, key, value) VALUES (?, ?, ?) ON CONFLICT(project_id, key) DO UPDATE SET value = excluded.value, updated_at = CURRENT_TIMESTAMP", id, storage_key, json).execute(conn).await?;
    } else {
        sqlx::query!("INSERT INTO settings (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = CURRENT_TIMESTAMP", storage_key, json).execute(conn).await?;
    }
    Ok(())
}
