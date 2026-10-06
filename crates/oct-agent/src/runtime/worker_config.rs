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
        Ok(Arc::new(AliasedTool { name: selected.exposed_name.clone(), inner: tool.clone() }) as Arc<dyn AgentTool>)
    }).collect()
}

struct AliasedTool {
    name: String,
    inner: Arc<dyn AgentTool>,
}

#[async_trait::async_trait]
impl AgentTool for AliasedTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn selector(&self) -> ToolSelector {
        self.inner.selector()
    }
    fn description(&self) -> &str {
        self.inner.description()
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
}
