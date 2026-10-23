//! Agent settings use cases. Reads and updates compose pure policy with the
//! repository on one connection; the editor may display stale references.
use sqlx::{SqliteConnection, SqlitePool};

use crate::agent_settings::AgentKey;
use crate::agent_settings::AgentOverride;
use crate::agent_settings::BUILTIN_TOOLS;
use crate::agent_settings::LiveCatalog;
use crate::agent_settings::Patch;
use crate::agent_settings::ResolvedSettings;
use crate::agent_settings::SettingsPatch;
use crate::agent_settings::ToolChoice;
use crate::agent_settings::ToolOption;
use crate::agent_settings::policy;
use crate::db::agent_settings as repository;
use crate::error::AppResult;
use crate::mcp::{McpManager, manager::ServerStatus};
use crate::tool_contract::ToolSelector;

#[derive(Clone)]
pub struct SettingsApplication {
    pool: SqlitePool,
}

impl SettingsApplication {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn read(&self, key: &str, project: Option<&str>) -> AppResult<ResolvedSettings> {
        let mut tx = self.pool.begin().await?;
        let value = read_conn(&mut tx, AgentKey::parse(key)?, project).await?;
        tx.commit().await?;
        Ok(value)
    }

    /// Execution preparation additionally rejects stale explicit model/server
    /// references. Editor reads remain available so invalid choices can be repaired.
    pub async fn read_for_run(
        &self,
        key: &str,
        project: Option<&str>,
    ) -> AppResult<ResolvedSettings> {
        let mut tx = self.pool.begin().await?;
        let value = read_conn(&mut tx, AgentKey::parse(key)?, project).await?;
        let references = repository::references(&mut tx, &value.effective).await?;
        policy::validate_references(&value.effective, project, &references)?;
        tx.commit().await?;
        Ok(value)
    }

    pub async fn list(&self, project: Option<&str>) -> AppResult<Vec<ResolvedSettings>> {
        let mut tx = self.pool.begin().await?;
        let mut values = Vec::new();
        for key in AgentKey::ALL {
            values.push(read_conn(&mut tx, key, project).await?);
        }
        tx.commit().await?;
        Ok(values)
    }

    pub async fn update(
        &self,
        key: &str,
        project: Option<&str>,
        patch: SettingsPatch,
        mcp: &McpManager,
    ) -> AppResult<ResolvedSettings> {
        // Capture the known connected catalogs without network work. Persisted
        // server/model existence and scope are checked inside the write transaction.
        let catalog = live_catalog(mcp, &patch);
        policy::validate_catalog_patch(&patch, &catalog)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if key == "worker" {
            let mode = crate::db::settings::mcp_tool_mode(&mut tx, project).await?;
            policy::validate_code_mode_patch(key, mode, &patch)?;
        }
        let key = AgentKey::parse(key)?;
        let old = read_conn(&mut tx, key, project).await?;
        let mut local = old.overrides;
        patch.apply(&mut local);
        // Read/validate the global layer in this same transaction; project
        // overrides stay independent and an unrelated field is never flattened.
        let global = read_conn(&mut tx, key, None).await?.overrides;
        let next = if project.is_some() {
            policy::resolve(key, global, Some(local.clone()))
        } else {
            policy::resolve(key, local.clone(), None)
        };
        policy::validate_shape(key, &next.effective)?;
        let references = repository::references(&mut tx, &next.effective).await?;
        policy::validate_references(&next.effective, project, &references)?;
        repository::write_override(&mut tx, key, project, &local).await?;
        tx.commit().await?;
        Ok(next)
    }

    pub async fn catalog(
        &self,
        project: Option<&str>,
        mcp: &McpManager,
    ) -> AppResult<Vec<ToolOption>> {
        if let Some(id) = project {
            self.read("worker", Some(id)).await?;
        }
        // Discover enabled servers when opening the picker, including before
        // the first chat turn. Reconciliation shares connections and isolates
        // failures; unavailable saved selections remain editable.
        mcp.ensure_connections(&self.pool, project).await;
        let mut options: Vec<_> = BUILTIN_TOOLS
            .iter()
            .map(|name| ToolOption {
                selector: ToolSelector::Builtin {
                    name: (*name).into(),
                },
                name: (*name).into(),
                server_name: None,
                available: true,
            })
            .collect();
        for server in crate::db::mcp::list_mcp_servers(&self.pool).await? {
            if server.project_id.is_some() && server.project_id.as_deref() != project {
                continue;
            }
            for tool in mcp.tools_of(&server.id) {
                options.push(ToolOption {
                    selector: ToolSelector::Mcp {
                        server_id: server.id.clone(),
                        tool: tool.name.clone(),
                    },
                    name: tool.name,
                    server_name: Some(server.name.clone()),
                    available: server.enabled
                        && matches!(
                            mcp.status_of(&server.id),
                            Some(ServerStatus::Connected { .. })
                        ),
                });
            }
        }
        Ok(options)
    }
}

async fn read_conn(
    conn: &mut SqliteConnection,
    key: AgentKey,
    project: Option<&str>,
) -> AppResult<ResolvedSettings> {
    let global = decode(repository::read_global(conn, key).await?)?;
    let project = match project {
        Some(id) => Some(decode(repository::read_project(conn, key, id).await?)?),
        None => None,
    };
    let value = policy::resolve(key, global, project);
    policy::validate_shape(key, &value.effective)?;
    Ok(value)
}

fn decode(raw: Option<String>) -> AppResult<AgentOverride> {
    Ok(raw
        .map(|value| serde_json::from_str(&value))
        .transpose()
        .map_err(anyhow::Error::from)?
        .unwrap_or_default())
}

fn live_catalog(mcp: &McpManager, patch: &SettingsPatch) -> LiveCatalog {
    let mut catalog = LiveCatalog::default();
    if let Patch::Set(ToolChoice::Allowlist { items }) = &patch.tools {
        for item in items {
            if let ToolSelector::Mcp { server_id, .. } = item
                && matches!(
                    mcp.status_of(server_id),
                    Some(ServerStatus::Connected { .. })
                )
            {
                catalog.tools.insert(
                    server_id.clone(),
                    mcp.tools_of(server_id)
                        .into_iter()
                        .map(|tool| tool.name)
                        .collect(),
                );
            }
        }
    }
    catalog
}
