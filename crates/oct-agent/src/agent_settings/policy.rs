//! Pure configuration inheritance, patching, validation and snapshot rules.
use super::types::*;
use crate::tool_contract::ToolSelector;

impl WorkerSnapshot {
    pub fn decode(raw: Option<&str>) -> anyhow::Result<Option<Self>> {
        raw.map(|raw| {
            let v: Self = serde_json::from_str(raw)?;
            anyhow::ensure!(
                matches!(v.version, 1 | 2)
                    && v.prompt_template_version == 1
                    && v.agent_type == "worker",
                "Unsupported worker configuration snapshot"
            );
            anyhow::ensure!(
                match (v.version, v.tool_mode, v.code_catalog_version) {
                    (1 | 2, crate::tool_contract::McpToolMode::Direct, None) => true,
                    (2, crate::tool_contract::McpToolMode::Code, Some(version)) =>
                        version == crate::tool_contract::CODE_CATALOG_VERSION,
                    _ => false,
                },
                "Unsupported worker code-mode snapshot"
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

fn defaults(key: AgentKey) -> AgentSettings {
    AgentSettings {
        model: ModelChoice::Parent,
        instructions: String::new(),
        tools: if key == AgentKey::Title {
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

pub fn resolve(
    key: AgentKey,
    global: AgentOverride,
    project: Option<AgentOverride>,
) -> ResolvedSettings {
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
    ResolvedSettings {
        key,
        overrides,
        effective,
        global_effective,
        sources,
    }
}

fn bad(message: impl Into<String>) -> SettingsPolicyError {
    SettingsPolicyError::Invalid(message.into())
}

pub fn validate_tool_list(items: &[ToolSelector]) -> Result<(), SettingsPolicyError> {
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

pub fn validate_shape(key: AgentKey, value: &AgentSettings) -> Result<(), SettingsPolicyError> {
    if value.instructions.len() > 32 * 1024 {
        return Err(bad("Agent instructions exceed 32 KiB"));
    }
    if key == AgentKey::Title
        && !matches!(&value.tools, ToolChoice::Allowlist { items } if items.is_empty())
    {
        return Err(bad("Title generation cannot use tools"));
    }
    if let ToolChoice::Allowlist { items } = &value.tools {
        validate_tool_list(items)?;
    }
    Ok(())
}

impl<T> Patch<T> {
    fn apply(self, target: &mut Option<T>) {
        match self {
            Self::Keep => {}
            Self::Inherit => *target = None,
            Self::Set(value) => *target = Some(value),
        }
    }
}
impl SettingsPatch {
    pub fn apply(self, value: &mut AgentOverride) {
        self.model.apply(&mut value.model);
        self.instructions.apply(&mut value.instructions);
        self.tools.apply(&mut value.tools);
    }
}

pub fn validate_references(
    value: &AgentSettings,
    project: Option<&str>,
    references: &ReferenceSnapshot,
) -> Result<(), SettingsPolicyError> {
    if matches!(value.model, ModelChoice::Fixed { .. }) {
        let tool_call = references
            .model_tool_call
            .ok_or_else(|| bad("Configured model does not exist"))?;
        if !tool_call
            && !matches!(&value.tools, ToolChoice::Allowlist { items } if items.is_empty())
        {
            return Err(bad("Selected model does not support tool calls"));
        }
    }
    if let ToolChoice::Allowlist { items } = &value.tools {
        for item in items {
            if let ToolSelector::Mcp { server_id, .. } = item {
                let scope = references
                    .servers
                    .get(server_id)
                    .ok_or_else(|| bad("Selected MCP server no longer exists"))?;
                if scope.is_some() && scope.as_deref() != project {
                    return Err(bad("MCP server is outside this configuration scope"));
                }
            }
        }
    }
    Ok(())
}

pub fn validate_catalog_patch(
    patch: &SettingsPatch,
    catalog: &LiveCatalog,
) -> Result<(), SettingsPolicyError> {
    if let Patch::Set(ToolChoice::Allowlist { items }) = &patch.tools {
        for item in items {
            if let ToolSelector::Mcp { server_id, tool } = item
                && let Some(tools) = catalog.tools.get(server_id)
                && !tools.contains(tool)
            {
                return Err(bad(format!("MCP tool no longer exists: {tool}")));
            }
        }
    }
    Ok(())
}

/// Code-mode workers browse their SDK with the regular read tools; a
/// non-empty allowlist without them would hand the worker a blind
/// execute_code. Empty allowlists stay valid (no code surface at all).
pub fn validate_code_mode_patch(
    key: &str,
    mode: crate::tool_contract::McpToolMode,
    patch: &SettingsPatch,
) -> Result<(), SettingsPolicyError> {
    if key != "worker" || mode != crate::tool_contract::McpToolMode::Code {
        return Ok(());
    }
    let Patch::Set(ToolChoice::Allowlist { items }) = &patch.tools else {
        return Ok(());
    };
    if items.is_empty() {
        return Ok(());
    }
    let missing: Vec<_> = ["read_file", "grep", "glob"]
        .into_iter()
        .filter(|name| {
            !items
                .iter()
                .any(|selector| matches!(selector, ToolSelector::Builtin { name: n } if n == name))
        })
        .collect();
    if !missing.is_empty() {
        return Err(bad(format!(
            "Code mode requires the read tools for SDK discovery in every non-empty worker allowlist; missing: {}",
            missing.join(", ")
        )));
    }
    Ok(())
}
