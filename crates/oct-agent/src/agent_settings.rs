//! Typed global/project defaults shared by delegated workers and title requests.
//! Missing override fields inherit; PATCH null removes a local override.

use serde::{Deserialize, Deserializer, Serialize};
use sqlx::{SqliteConnection, SqlitePool};
use utoipa::ToSchema;

use crate::error::{AppError, AppResult};

pub const BUILTIN_TOOLS: &[&str] = &[
    "read_file",
    "list_dir",
    "grep",
    "glob",
    "write_file",
    "edit_file",
    "execute_command",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum ModelChoice {
    Parent,
    Fixed {
        provider_id: String,
        model_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolSelector {
    Builtin { name: String },
    Mcp { server_id: String, tool: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolChoice {
    Parent,
    Allowlist { items: Vec<ToolSelector> },
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentOverride {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<ToolChoice>,
}

fn patch_field<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(d).map(Some)
}

#[derive(Debug, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentPatch {
    #[serde(default, deserialize_with = "patch_field")]
    pub model: Option<Option<ModelChoice>>,
    #[serde(default, deserialize_with = "patch_field")]
    pub instructions: Option<Option<String>>,
    #[serde(default, deserialize_with = "patch_field")]
    pub tools: Option<Option<ToolChoice>>,
}

impl AgentPatch {
    fn apply(self, value: &mut AgentOverride) {
        if let Some(v) = self.model {
            value.model = v;
        }
        if let Some(v) = self.instructions {
            value.instructions = v;
        }
        if let Some(v) = self.tools {
            value.tools = v;
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AgentSettings {
    pub model: ModelChoice,
    pub instructions: String,
    pub tools: ToolChoice,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SettingSources {
    pub model: String,
    pub instructions: String,
    pub tools: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct AgentSettingsView {
    pub key: String,
    pub name: String,
    pub description: String,
    pub delegatable: bool,
    pub tools_editable: bool,
    pub overrides: AgentOverride,
    pub effective: AgentSettings,
    pub global_effective: AgentSettings,
    pub sources: SettingSources,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SnapshotTool {
    pub selector: ToolSelector,
    pub exposed_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkerSnapshot {
    pub version: u32,
    pub agent_type: String,
    pub instructions: String,
    pub prompt_template_version: u32,
    pub tools: Vec<SnapshotTool>,
    pub sources: SettingSources,
}

impl WorkerSnapshot {
    pub fn decode(raw: Option<&str>) -> anyhow::Result<Option<Self>> {
        raw.map(|raw| {
            let v: Self = serde_json::from_str(raw)?;
            anyhow::ensure!(
                v.version == 1 && v.prompt_template_version == 1 && v.agent_type == "worker",
                "Unsupported worker configuration snapshot"
            );
            validate_tool_list(
                &v.tools
                    .iter()
                    .map(|t| t.selector.clone())
                    .collect::<Vec<_>>(),
            )
            .map_err(|e| anyhow::anyhow!("Invalid worker tool snapshot: {e}"))?;
            let mut names = std::collections::HashSet::new();
            anyhow::ensure!(
                v.tools
                    .iter()
                    .all(|t| !t.exposed_name.is_empty() && names.insert(&t.exposed_name)),
                "Invalid worker tool aliases"
            );
            Ok(v)
        })
        .transpose()
    }
}

pub fn require_key(key: &str) -> AppResult<()> {
    if matches!(key, "worker" | "title") {
        Ok(())
    } else {
        Err(AppError::NotFound("Agent configuration not found".into()))
    }
}

fn defaults(key: &str) -> AgentSettings {
    AgentSettings {
        model: ModelChoice::Parent,
        instructions: String::new(),
        tools: if key == "title" {
            ToolChoice::Allowlist { items: vec![] }
        } else {
            ToolChoice::Parent
        },
    }
}

fn overlay(
    value: &mut AgentSettings,
    src: &mut SettingSources,
    local: &AgentOverride,
    source: &str,
) {
    if let Some(v) = &local.model {
        value.model = v.clone();
        src.model = source.into();
    }
    if let Some(v) = &local.instructions {
        value.instructions = v.clone();
        src.instructions = source.into();
    }
    if let Some(v) = &local.tools {
        value.tools = v.clone();
        src.tools = source.into();
    }
}

fn view(key: &str, global: AgentOverride, project: Option<AgentOverride>) -> AgentSettingsView {
    let mut effective = defaults(key);
    let mut sources = SettingSources {
        model: "builtin".into(),
        instructions: "builtin".into(),
        tools: "builtin".into(),
    };
    overlay(&mut effective, &mut sources, &global, "global");
    let global_effective = effective.clone();
    let overrides = if let Some(project) = project {
        overlay(&mut effective, &mut sources, &project, "project");
        project
    } else {
        global
    };
    AgentSettingsView {
        key: key.into(),
        name: if key == "worker" {
            "Worker 子代理"
        } else {
            "Title 标题生成"
        }
        .into(),
        description: if key == "worker" {
            "委派任务的模型、角色指令与工具"
        } else {
            "自动生成标题的专用模型与命名偏好"
        }
        .into(),
        delegatable: key == "worker",
        tools_editable: key == "worker",
        overrides,
        effective,
        global_effective,
        sources,
    }
}

async fn read_conn(
    conn: &mut SqliteConnection,
    key: &str,
    project_id: Option<&str>,
) -> AppResult<AgentSettingsView> {
    require_key(key)?;
    let storage_key = format!("agent.{key}");
    let global = sqlx::query_scalar!("SELECT value FROM settings WHERE key = ?", storage_key)
        .fetch_optional(&mut *conn)
        .await?;
    let global = global
        .map(|s| serde_json::from_str::<AgentOverride>(&s))
        .transpose()
        .map_err(anyhow::Error::from)?
        .unwrap_or_default();
    let project = if let Some(id) = project_id {
        if sqlx::query_scalar!("SELECT id FROM projects WHERE id = ?", id)
            .fetch_optional(&mut *conn)
            .await?
            .is_none()
        {
            return Err(AppError::NotFound("Project not found".into()));
        }
        let local = sqlx::query_scalar!(
            "SELECT value FROM project_settings WHERE project_id = ? AND key = ?",
            id,
            storage_key
        )
        .fetch_optional(&mut *conn)
        .await?;
        Some(
            local
                .map(|s| serde_json::from_str::<AgentOverride>(&s))
                .transpose()
                .map_err(anyhow::Error::from)?
                .unwrap_or_default(),
        )
    } else {
        None
    };
    let result = view(key, global, project);
    validate_shape(key, &result.effective)?;
    Ok(result)
}

pub async fn read(
    pool: &SqlitePool,
    key: &str,
    project_id: Option<&str>,
) -> AppResult<AgentSettingsView> {
    let mut tx = pool.begin().await?;
    let v = read_conn(&mut tx, key, project_id).await?;
    tx.commit().await?;
    Ok(v)
}

/// Execution preparation additionally rejects stale explicit model/server references.
/// The editor read remains available so the user can repair an invalid selection.
pub async fn read_for_run(
    pool: &SqlitePool,
    key: &str,
    project_id: Option<&str>,
) -> AppResult<AgentSettingsView> {
    let mut tx = pool.begin().await?;
    let value = read_conn(&mut tx, key, project_id).await?;
    validate_references(&mut tx, &value.effective, project_id).await?;
    tx.commit().await?;
    Ok(value)
}

pub async fn list(
    pool: &SqlitePool,
    project_id: Option<&str>,
) -> AppResult<Vec<AgentSettingsView>> {
    let mut tx = pool.begin().await?;
    let mut values = vec![];
    for key in ["worker", "title"] {
        values.push(read_conn(&mut tx, key, project_id).await?);
    }
    tx.commit().await?;
    Ok(values)
}

fn bad(message: impl Into<String>) -> AppError {
    AppError::BadRequest(message.into())
}

pub fn validate_tool_list(items: &[ToolSelector]) -> AppResult<()> {
    let mut seen = std::collections::HashSet::new();
    for item in items {
        if !seen.insert(item) {
            return Err(bad("Duplicate tool selection"));
        }
        match item {
            ToolSelector::Builtin { name } if !BUILTIN_TOOLS.contains(&name.as_str()) => {
                return Err(bad(format!("Tool is unavailable to workers: {name}")));
            }
            ToolSelector::Mcp { server_id, tool } if server_id.is_empty() || tool.is_empty() => {
                return Err(bad("MCP tool requires server_id and original tool name"));
            }
            _ => {}
        }
    }
    let builtin = |name: &str| {
        items
            .iter()
            .any(|t| matches!(t, ToolSelector::Builtin { name: n } if n == name))
    };
    if builtin("edit_file") && !builtin("read_file") {
        return Err(bad("edit_file requires read_file"));
    }
    Ok(())
}

fn validate_shape(key: &str, value: &AgentSettings) -> AppResult<()> {
    if value.instructions.len() > 32 * 1024 {
        return Err(bad("Agent instructions exceed 32 KiB"));
    }
    if key == "title"
        && !matches!(&value.tools, ToolChoice::Allowlist { items } if items.is_empty())
    {
        return Err(bad("Title generation cannot use tools"));
    }
    if let ToolChoice::Allowlist { items } = &value.tools {
        validate_tool_list(items)?;
    }
    Ok(())
}

async fn validate_references(
    conn: &mut SqliteConnection,
    value: &AgentSettings,
    project_id: Option<&str>,
) -> AppResult<()> {
    if let ModelChoice::Fixed {
        provider_id,
        model_id,
    } = &value.model
    {
        let model = sqlx::query!(
            "SELECT tool_call FROM models WHERE provider_id = ? AND model_id = ?",
            provider_id,
            model_id
        )
        .fetch_optional(&mut *conn)
        .await?
        .ok_or_else(|| bad("Configured model does not exist"))?;
        if !model.tool_call
            && !matches!(&value.tools, ToolChoice::Allowlist { items } if items.is_empty())
        {
            return Err(bad("Selected model does not support tool calls"));
        }
    }
    if let ToolChoice::Allowlist { items } = &value.tools {
        for item in items {
            if let ToolSelector::Mcp { server_id, .. } = item {
                let row =
                    sqlx::query!("SELECT project_id FROM mcp_servers WHERE id = ?", server_id)
                        .fetch_optional(&mut *conn)
                        .await?
                        .ok_or_else(|| bad("Selected MCP server no longer exists"))?;
                if row.project_id.is_some() && row.project_id.as_deref() != project_id {
                    return Err(bad("MCP server is outside this configuration scope"));
                }
            }
        }
    }
    Ok(())
}

pub async fn update(
    pool: &SqlitePool,
    key: &str,
    project_id: Option<&str>,
    patch: AgentPatch,
) -> AppResult<AgentSettingsView> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let old = read_conn(&mut tx, key, project_id).await?;
    let mut local = old.overrides;
    patch.apply(&mut local);
    // Read the global layer in this same transaction; project overrides stay independent.
    let global = read_conn(&mut tx, key, None).await?.overrides;
    let next = if project_id.is_some() {
        view(key, global, Some(local.clone()))
    } else {
        view(key, local.clone(), None)
    };
    validate_shape(key, &next.effective)?;
    validate_references(&mut tx, &next.effective, project_id).await?;
    let storage_key = format!("agent.{key}");
    let json = serde_json::to_string(&local).map_err(anyhow::Error::from)?;
    if let Some(id) = project_id {
        sqlx::query!("INSERT INTO project_settings (project_id, key, value) VALUES (?, ?, ?) ON CONFLICT(project_id, key) DO UPDATE SET value = excluded.value, updated_at = CURRENT_TIMESTAMP", id, storage_key, json).execute(&mut *tx).await?;
    } else {
        sqlx::query!("INSERT INTO settings (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = CURRENT_TIMESTAMP", storage_key, json).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(next)
}

impl ModelChoice {
    pub fn identity(&self, parent_provider: &str, parent_model: &str) -> (String, String) {
        match self {
            Self::Parent => (parent_provider.into(), parent_model.into()),
            Self::Fixed {
                provider_id,
                model_id,
            } => (provider_id.clone(), model_id.clone()),
        }
    }
}
