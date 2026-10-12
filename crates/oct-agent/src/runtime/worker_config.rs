//! Seal the effective tools of a worker and restore their stable dispatch identities.
use crate::agent_settings::{
    BUILTIN_TOOLS, SnapshotTool, ToolChoice, ToolSelector, validate_tool_list,
};
use crate::tools::{AgentTool, ToolContext, ToolOutput};
use anyhow::{Result, ensure};
use std::sync::Arc;

pub(crate) fn select_tools(
    parent: &[Arc<dyn AgentTool>],
    choice: &ToolChoice,
) -> Result<Vec<SnapshotTool>> {
    let candidates: Vec<_> = parent
        .iter()
        .filter(|t| match t.selector() {
            ToolSelector::Builtin { name } => BUILTIN_TOOLS.contains(&name.as_str()),
            ToolSelector::Mcp { server_id, .. } => !server_id.is_empty(),
        })
        .map(|t| SnapshotTool {
            selector: t.selector(),
            exposed_name: t.name().into(),
            exposed_description: t.description().to_string(),
        })
        .collect();
    let selected = match choice {
        ToolChoice::Parent => candidates,
        ToolChoice::Allowlist { items } => items
            .iter()
            .map(|selector| {
                candidates
                    .iter()
                    .find(|c| &c.selector == selector)
                    .cloned()
                    .ok_or_else(|| {
                        anyhow::anyhow!("Configured worker tool is unavailable: {selector:?}")
                    })
            })
            .collect::<Result<Vec<_>>>()?,
    };
    validate_tool_list(
        &selected
            .iter()
            .map(|t| t.selector.clone())
            .collect::<Vec<_>>(),
    )?;
    Ok(selected)
}

pub(crate) fn restore_tools(
    candidates: Vec<Arc<dyn AgentTool>>,
    sealed: &[SnapshotTool],
) -> Result<Vec<Arc<dyn AgentTool>>> {
    sealed.iter().map(|selected| {
        let tool = candidates.iter().find(|tool| tool.selector() == selected.selector)
            .ok_or_else(|| anyhow::anyhow!("Worker snapshot tool is unavailable: {:?}", selected.selector))?;
        ensure!(!matches!(&selected.selector, ToolSelector::Builtin { name } if !BUILTIN_TOOLS.contains(&name.as_str())), "Tool is not permitted for workers");
        Ok(Arc::new(AliasedTool {
            name: selected.exposed_name.clone(),
            description: frozen_presentation(selected),
            inner: tool.clone(),
        }) as Arc<dyn AgentTool>)
    }).collect()
}

/// Lenient variant of [`restore_tools`] for the frozen prompt environment
/// (MCP identities only). Differences from the strict worker version:
///
/// * a sealed identity MISSING from the live candidates is skipped and
///   reported in the returned count instead of failing the run — the caller
///   marks the environment dirty so the next run re-freezes (a disabled or
///   deleted server disappears gracefully; correctness beats cache here);
/// * candidates NOT in the sealed list are dropped: a server enabled after
///   the freeze waits for the next refresh point before joining the
///   conversation;
/// * survivors wear their SEALED exposed names and presentations, so a
///   server rename never changes the prompt's tool bytes mid-conversation
///   (the generated `[mcp:{server}]` description embeds the server name).
pub(crate) fn restore_tools_lenient(
    candidates: &[Arc<dyn AgentTool>],
    sealed: &[SnapshotTool],
) -> (Vec<Arc<dyn AgentTool>>, usize) {
    let mut restored = Vec::with_capacity(sealed.len());
    let mut missing = 0;
    for selected in sealed {
        match candidates
            .iter()
            .find(|tool| tool.selector() == selected.selector)
        {
            Some(tool) => restored.push(Arc::new(AliasedTool {
                name: selected.exposed_name.clone(),
                description: frozen_presentation(selected),
                inner: tool.clone(),
            }) as Arc<dyn AgentTool>),
            None => missing += 1,
        }
    }
    (restored, missing)
}

/// The frozen description for one sealed tool, or `None` when the snapshot
/// predates the field (falling back to the live description at render).
fn frozen_presentation(selected: &SnapshotTool) -> Option<String> {
    (!selected.exposed_description.is_empty()).then(|| selected.exposed_description.clone())
}

struct AliasedTool {
    name: String,
    /// Frozen presentation: rendered into the system prompt verbatim so a
    /// server rename (which regenerates `[mcp:{server}] …` on the live
    /// instance) cannot change the prompt bytes without a refresh point.
    /// `None` (snapshots predating the field) delegates to the live tool.
    description: Option<String>,
    inner: Arc<dyn AgentTool>,
}

#[async_trait::async_trait]
impl AgentTool for AliasedTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        self.description
            .as_deref()
            .unwrap_or_else(|| self.inner.description())
    }
    /// The gate must authorize by the LIVE name (rules are written against
    /// current server names), never by a frozen alias. Delegates through
    /// `authz_name`, not `name`: workers receive the parent's ALREADY-aliased
    /// instances and strict restore wraps them again, so the wrapper stack
    /// can be two AliasedTool layers deep — only recursive delegation reaches
    /// the live MCP name at the bottom.
    fn authz_name(&self) -> &str {
        self.inner.authz_name()
    }
    fn selector(&self) -> ToolSelector {
        self.inner.selector()
    }
    fn input_schema(&self) -> serde_json::Value {
        self.inner.input_schema()
    }
    fn validate_input(&self, args: &serde_json::Value) -> Result<()> {
        self.inner.validate_input(args)
    }
    fn output_schema(&self) -> Option<serde_json::Value> {
        self.inner.output_schema()
    }
    fn mcp_server_name(&self) -> Option<&str> {
        self.inner.mcp_server_name()
    }
    fn interrupted_output(&self, call_id: &str) -> Option<ToolOutput> {
        self.inner.interrupted_output(call_id)
    }
    fn title(&self, args: &serde_json::Value) -> String {
        self.inner.title(args)
    }
    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        self.inner.execute(args, ctx).await
    }
    async fn execute_for_code(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<crate::tools::CodeToolResult> {
        self.inner.execute_for_code(args, ctx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::EventHub;
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    struct Probe {
        name: String,
        selector: ToolSelector,
    }
    #[async_trait::async_trait]
    impl AgentTool for Probe {
        fn name(&self) -> &str {
            &self.name
        }
        fn selector(&self) -> ToolSelector {
            self.selector.clone()
        }
        fn description(&self) -> &str {
            "probe"
        }
        fn input_schema(&self) -> serde_json::Value {
            json!({"type":"object"})
        }
        fn validate_input(&self, args: &serde_json::Value) -> Result<()> {
            ensure!(
                args.get("query")
                    .and_then(serde_json::Value::as_str)
                    .is_some(),
                "query required"
            );
            Ok(())
        }
        async fn execute(&self, args: serde_json::Value, _: &ToolContext) -> Result<ToolOutput> {
            Ok(ToolOutput::success(format!("{:?}:{}", self.selector, args)))
        }
    }
    fn mcp(id: &str, name: &str) -> Arc<dyn AgentTool> {
        Arc::new(Probe {
            name: name.into(),
            selector: ToolSelector::Mcp {
                server_id: id.into(),
                tool: "search".into(),
            },
        })
    }

    #[test]
    fn aliased_tool_authorizes_by_the_live_name_not_the_alias() {
        // After a server rename the frozen conversation still exposes the
        // old alias to the model, but the permission gate must key on the
        // LIVE name — rules are written against current server names.
        let sealed = [SnapshotTool {
            selector: ToolSelector::Mcp {
                server_id: "server-a".into(),
                tool: "search".into(),
            },
            exposed_name: "mcp__old__search".into(),
            exposed_description: "[mcp:old] Search the index".into(),
        }];
        let (restored, missing) =
            restore_tools_lenient(&[mcp("server-a", "mcp__renamed__search")], &sealed);
        assert_eq!(missing, 0);
        assert_eq!(restored[0].name(), "mcp__old__search");
        assert_eq!(restored[0].authz_name(), "mcp__renamed__search");
        // The frozen PRESENTATION survives the rename: the generated
        // description embeds the server name, so the live instance would
        // read `[mcp:renamed] …`.
        assert_eq!(restored[0].description(), "[mcp:old] Search the index");

        // Workers stack a SECOND wrapper: strict restore wraps the parent's
        // already-aliased instance again. Recursive delegation must still
        // reach the live name — a `deny mcp__renamed__*` rule must block
        // parent and worker alike.
        let worker_sealed = [SnapshotTool {
            selector: sealed[0].selector.clone(),
            exposed_name: "mcp__old__search".into(),
            exposed_description: sealed[0].exposed_description.clone(),
        }];
        let worker_set = restore_tools(restored, &worker_sealed).unwrap();
        assert_eq!(worker_set[0].name(), "mcp__old__search");
        assert_eq!(worker_set[0].authz_name(), "mcp__renamed__search");
        assert_eq!(worker_set[0].description(), "[mcp:old] Search the index");
    }

    #[test]
    fn empty_frozen_description_falls_back_to_the_live_one() {
        // Snapshots predating the field (empty description) delegate to the
        // live tool's description at render time.
        let sealed = [SnapshotTool {
            selector: ToolSelector::Mcp {
                server_id: "server-a".into(),
                tool: "search".into(),
            },
            exposed_name: "mcp__old__search".into(),
            exposed_description: String::new(),
        }];
        let (restored, _) =
            restore_tools_lenient(&[mcp("server-a", "mcp__renamed__search")], &sealed);
        assert_eq!(restored[0].description(), "probe");
    }

    #[tokio::test]
    async fn renamed_mcp_keeps_sealed_alias_and_server_dispatch_without_new_tools() {
        let old = mcp("server-a", "mcp__old__search");
        let snapshot = select_tools(
            &[old, mcp("server-b", "mcp__other__search")],
            &ToolChoice::Allowlist {
                items: vec![ToolSelector::Mcp {
                    server_id: "server-a".into(),
                    tool: "search".into(),
                }],
            },
        )
        .unwrap();
        let restored = restore_tools(
            vec![
                mcp("server-b", "mcp__old__search"),
                mcp("server-a", "mcp__renamed__search"),
            ],
            &snapshot,
        )
        .unwrap();
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].name(), "mcp__old__search");
        assert!(restored[0].validate_input(&json!({})).is_err());
        assert!(
            restored[0]
                .validate_input(&json!({"query":"needle"}))
                .is_ok()
        );
        let ctx = ToolContext::new("call", Arc::new(EventHub::new()), CancellationToken::new());
        let output = restored[0]
            .execute(json!({"query":"needle"}), &ctx)
            .await
            .unwrap();
        assert!(output.content.contains("server-a"));
        assert!(output.content.contains("needle"));
        assert!(restore_tools(vec![mcp("server-b", "mcp__old__search")], &snapshot).is_err());
    }

    #[test]
    fn lenient_restore_filters_live_set_to_the_frozen_identities() {
        let frozen = vec![
            SnapshotTool {
                selector: ToolSelector::Mcp {
                    server_id: "a".into(),
                    tool: "search".into(),
                },
                exposed_name: "mcp__a__search".into(),
                exposed_description: String::new(),
            },
            SnapshotTool {
                selector: ToolSelector::Mcp {
                    server_id: "b".into(),
                    tool: "search".into(),
                },
                exposed_name: "mcp__b__search".into(),
                exposed_description: String::new(),
            },
        ];
        // The live set carries a NEW server (`c`, enabled after the freeze)
        // which must not join the conversation through this path.
        let live = vec![mcp("a", "mcp__a__search"), mcp("c", "mcp__c__search")];
        let (restored, missing) = restore_tools_lenient(&live, &frozen);
        assert_eq!(missing, 1, "frozen server-b is gone from the live set");
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].name(), "mcp__a__search");
        assert!(
            restored.iter().all(|t| t.name() != "mcp__c__search"),
            "post-freeze newcomers are excluded until a refresh point"
        );
    }

    #[test]
    fn lenient_restore_keeps_sealed_exposed_name_across_renames() {
        let frozen = vec![SnapshotTool {
            selector: ToolSelector::Mcp {
                server_id: "a".into(),
                tool: "search".into(),
            },
            // The exposed name at freeze time…
            exposed_name: "mcp__old-name__search".into(),
            exposed_description: String::new(),
        }];
        // …while the live connection now exposes the server under a new name.
        let (restored, missing) =
            restore_tools_lenient(&[mcp("a", "mcp__renamed__search")], &frozen);
        assert_eq!(missing, 0);
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].name(), "mcp__old-name__search");
        // Dispatch still reaches the live instance (same selector → same
        // server), and the schema/description ride along unchanged.
        assert_eq!(
            restored[0].selector(),
            ToolSelector::Mcp {
                server_id: "a".into(),
                tool: "search".into(),
            }
        );
        assert!(restored[0].validate_input(&json!({})).is_err());
    }
}
