use super::{policy, *};

#[test]
fn patch_preserves_inheritance_and_explicit_empty_values() {
    let global = AgentOverride {
        model: Some(ModelChoice::Fixed {
            provider_id: "provider".into(),
            model_id: "model".into(),
        }),
        instructions: Some("global".into()),
        tools: Some(ToolChoice::Parent),
    };
    let mut local = AgentOverride::default();
    SettingsPatch {
        model: Patch::Set(ModelChoice::Parent),
        instructions: Patch::Set(String::new()),
        tools: Patch::Set(ToolChoice::Allowlist { items: vec![] }),
    }
    .apply(&mut local);
    let resolved = policy::resolve(AgentKey::Worker, global.clone(), Some(local.clone()));
    assert_eq!(resolved.effective.model, ModelChoice::Parent);
    assert!(resolved.effective.instructions.is_empty());
    assert_eq!(
        resolved.effective.tools,
        ToolChoice::Allowlist { items: vec![] }
    );
    assert_eq!(resolved.sources.model, "project");
    assert_eq!(resolved.global_effective.instructions, "global");

    SettingsPatch {
        instructions: Patch::Inherit,
        ..Default::default()
    }
    .apply(&mut local);
    let resolved = policy::resolve(AgentKey::Worker, global, Some(local));
    assert_eq!(resolved.effective.instructions, "global");
    assert_eq!(resolved.sources.instructions, "global");
    assert_eq!(resolved.overrides.instructions, None);
    assert_eq!(resolved.overrides.model, Some(ModelChoice::Parent));
    assert_eq!(resolved.sources.tools, "project");
}

#[test]
fn reference_facts_enforce_model_capabilities_and_server_scope() {
    let mut value = policy::resolve(AgentKey::Worker, AgentOverride::default(), None).effective;
    value.model = ModelChoice::Fixed {
        provider_id: "p".into(),
        model_id: "m".into(),
    };
    let mut facts = ReferenceSnapshot::default();
    assert!(policy::validate_references(&value, Some("project"), &facts).is_err());
    facts.model_tool_call = Some(false);
    assert!(policy::validate_references(&value, Some("project"), &facts).is_err());
    value.tools = ToolChoice::Allowlist { items: vec![] };
    assert!(policy::validate_references(&value, Some("project"), &facts).is_ok());
    facts.model_tool_call = Some(true);
    value.tools = ToolChoice::Allowlist {
        items: vec![ToolSelector::Mcp {
            server_id: "server".into(),
            tool: "search".into(),
        }],
    };
    assert!(policy::validate_references(&value, Some("project"), &facts).is_err());
    facts.servers.insert("server".into(), Some("other".into()));
    assert!(policy::validate_references(&value, Some("project"), &facts).is_err());
    facts
        .servers
        .insert("server".into(), Some("project".into()));
    assert!(policy::validate_references(&value, Some("project"), &facts).is_ok());
    assert!(policy::validate_references(&value, None, &facts).is_err());
    facts.servers.insert("server".into(), None);
    assert!(policy::validate_references(&value, None, &facts).is_ok());
}

#[test]
fn code_mode_checks_explicit_tool_changes_and_known_catalogs() {
    use crate::tool_contract::McpToolMode;
    let mut patch = SettingsPatch::default();
    assert!(policy::validate_code_mode_patch("worker", McpToolMode::Code, &patch).is_ok());
    patch.tools = Patch::Inherit;
    assert!(policy::validate_code_mode_patch("worker", McpToolMode::Code, &patch).is_ok());
    patch.tools = Patch::Set(ToolChoice::Allowlist { items: vec![] });
    assert!(policy::validate_code_mode_patch("worker", McpToolMode::Code, &patch).is_ok());
    patch.tools = Patch::Set(ToolChoice::Allowlist {
        items: vec![ToolSelector::Mcp {
            server_id: "s".into(),
            tool: "search".into(),
        }],
    });
    assert!(policy::validate_code_mode_patch("worker", McpToolMode::Direct, &patch).is_ok());
    assert!(policy::validate_code_mode_patch("worker", McpToolMode::Code, &patch).is_err());
    let mut catalog = LiveCatalog::default();
    assert!(policy::validate_catalog_patch(&patch, &catalog).is_ok());
    catalog.tools.insert("s".into(), vec!["renamed".into()]);
    assert!(policy::validate_catalog_patch(&patch, &catalog).is_err());
    catalog.tools.insert("s".into(), vec!["search".into()]);
    assert!(policy::validate_catalog_patch(&patch, &catalog).is_ok());
}

#[test]
fn title_and_worker_tool_constraints_are_independent_of_storage() {
    let mut title = policy::resolve(AgentKey::Title, AgentOverride::default(), None).effective;
    assert!(policy::validate_shape(AgentKey::Title, &title).is_ok());
    title.tools = ToolChoice::Parent;
    assert!(policy::validate_shape(AgentKey::Title, &title).is_err());
    let edit = ToolSelector::Builtin {
        name: "edit_file".into(),
    };
    let read = ToolSelector::Builtin {
        name: "read_file".into(),
    };
    assert!(validate_tool_list(std::slice::from_ref(&edit)).is_err());
    assert!(validate_tool_list(&[edit, read.clone()]).is_ok());
    assert!(validate_tool_list(&[read.clone(), read]).is_err());
}

#[cfg(test)]
mod snapshot_tests {
    use super::super::*;
    use crate::tool_contract::McpToolMode;
    use serde_json::json;

    #[test]
    fn legacy_worker_snapshots_keep_direct_tools() {
        let legacy = json!({
            "version":1, "agent_type":"worker", "instructions":"legacy",
            "prompt_template_version":1, "tools":[],
            "sources":{"model":"builtin","instructions":"builtin","tools":"builtin"}
        });
        let snapshot = WorkerSnapshot::decode(Some(&legacy.to_string()))
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.tool_mode, McpToolMode::Direct);
        assert!(snapshot.code_catalog_version.is_none());
        assert!(WorkerSnapshot::decode(None).unwrap().is_none());
    }

    #[test]
    fn code_worker_snapshots_require_a_matching_catalog_version() {
        let mut raw = json!({
            "version":2, "agent_type":"worker", "instructions":"",
            "prompt_template_version":1, "tools":[], "tool_mode":"code",
            "code_catalog_version":crate::tool_contract::CODE_CATALOG_VERSION,
            "sources":{"model":"builtin","instructions":"builtin","tools":"builtin"}
        });
        assert!(WorkerSnapshot::decode(Some(&raw.to_string())).is_ok());
        raw["code_catalog_version"] = json!(crate::tool_contract::CODE_CATALOG_VERSION + 1);
        assert!(WorkerSnapshot::decode(Some(&raw.to_string())).is_err());
        raw.as_object_mut().unwrap().remove("code_catalog_version");
        assert!(WorkerSnapshot::decode(Some(&raw.to_string())).is_err());
    }
}
