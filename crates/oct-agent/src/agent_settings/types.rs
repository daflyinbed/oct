//! Persisted configuration and application values. No SQL or HTTP types.
use serde::{Deserialize, Serialize};

pub const BUILTIN_TOOLS: &[&str] = &[
    "read_file",
    "list_dir",
    "grep",
    "glob",
    "write_file",
    "edit_file",
    "execute_command",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum ModelChoice {
    Parent,
    Fixed {
        provider_id: String,
        model_id: String,
    },
}

use crate::tool_contract::ToolSelector;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolChoice {
    Parent,
    Allowlist { items: Vec<ToolSelector> },
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentOverride {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<ToolChoice>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSettings {
    pub model: ModelChoice,
    pub instructions: String,
    pub tools: ToolChoice,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettingSources {
    pub model: String,
    pub instructions: String,
    pub tools: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotTool {
    pub selector: ToolSelector,
    pub exposed_name: String,
    /// Frozen presentation: the description rendered into the system prompt
    /// (`[mcp:{server}] ...` embeds the server name, so a rename would
    /// change the bytes). Empty on snapshots predating the field — restore
    /// then falls back to the live description.
    #[serde(default)]
    pub exposed_description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerSnapshot {
    pub version: u32,
    pub agent_type: String,
    pub instructions: String,
    pub prompt_template_version: u32,
    pub tools: Vec<SnapshotTool>,
    pub sources: SettingSources,
    #[serde(default)]
    pub tool_mode: crate::tool_contract::McpToolMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_catalog_version: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentKey {
    Worker,
    Title,
}
impl AgentKey {
    pub const ALL: [Self; 2] = [Self::Worker, Self::Title];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Worker => "worker",
            Self::Title => "title",
        }
    }
    pub fn parse(key: &str) -> Result<Self, SettingsPolicyError> {
        match key {
            "worker" => Ok(Self::Worker),
            "title" => Ok(Self::Title),
            _ => Err(SettingsPolicyError::UnknownAgent),
        }
    }
}

/// Explicit application semantics; HTTP omission/null decoding stays in the API.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Patch<T> {
    #[default]
    Keep,
    Inherit,
    Set(T),
}

#[derive(Debug, Clone, Default)]
pub struct SettingsPatch {
    pub model: Patch<ModelChoice>,
    pub instructions: Patch<String>,
    pub tools: Patch<ToolChoice>,
}

#[derive(Debug, Clone)]
pub struct ResolvedSettings {
    pub key: AgentKey,
    pub overrides: AgentOverride,
    pub effective: AgentSettings,
    pub global_effective: AgentSettings,
    pub sources: SettingSources,
}

/// Database reference facts from the same transaction as the configuration.
#[derive(Debug, Clone, Default)]
pub struct ReferenceSnapshot {
    pub model_tool_call: Option<bool>,
    pub servers: std::collections::HashMap<String, Option<String>>,
}

/// Only connected servers appear here. An unavailable server's saved selection
/// remains editable; known connected catalogs reject obsolete original names.
#[derive(Debug, Clone, Default)]
pub struct LiveCatalog {
    pub tools: std::collections::HashMap<String, Vec<String>>,
}

#[derive(Debug, thiserror::Error)]
pub enum SettingsPolicyError {
    #[error("Agent configuration not found")]
    UnknownAgent,
    #[error("{0}")]
    Invalid(String),
}

/// Tool picker facts; the HTTP adapter supplies labels and group wording.
pub struct ToolOption {
    pub selector: ToolSelector,
    pub name: String,
    pub server_name: Option<String>,
    pub available: bool,
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
