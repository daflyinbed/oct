//! Worker inheritance captures presentation and reusable MCP instances only.
use std::sync::Arc;

use crate::agent_settings::{SnapshotTool, ToolChoice};
use crate::runtime::worker_config;
use crate::tool_contract::{AgentTool, ToolSelector};

pub struct WorkerInheritance {
    tools: Vec<SnapshotTool>,
    mcp: Vec<Arc<dyn AgentTool>>,
}

impl WorkerInheritance {
    /// Capture before appending task. Native instances and root-only controls
    /// are discarded; workers rebuild them with their own roots/read state.
    pub fn capture(parent: &[Arc<dyn AgentTool>]) -> Self {
        Self {
            tools: worker_config::tool_candidates(parent),
            mcp: parent
                .iter()
                .filter(|tool| matches!(tool.selector(), ToolSelector::Mcp { .. }))
                .cloned()
                .collect(),
        }
    }

    pub(crate) fn select(&self, choice: &ToolChoice) -> anyhow::Result<Vec<SnapshotTool>> {
        worker_config::select_candidates(&self.tools, choice)
    }

    pub(crate) fn mcp_tools(&self) -> &[Arc<dyn AgentTool>] {
        &self.mcp
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_contract::{ToolContext, ToolOutput};
    use async_trait::async_trait;

    struct Probe {
        name: &'static str,
        selector: ToolSelector,
    }

    #[async_trait]
    impl AgentTool for Probe {
        fn name(&self) -> &str {
            self.name
        }
        fn selector(&self) -> ToolSelector {
            self.selector.clone()
        }
        fn description(&self) -> &str {
            "frozen presentation"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type":"object"})
        }
        async fn execute(
            &self,
            _: serde_json::Value,
            _: &ToolContext,
        ) -> anyhow::Result<ToolOutput> {
            Ok(ToolOutput::success("dispatched"))
        }
    }

    #[test]
    fn inheritance_releases_native_and_task_instances_and_retains_exact_mcp_dispatch() {
        let native: Arc<dyn AgentTool> = Arc::new(Probe {
            name: "read_file",
            selector: ToolSelector::Builtin {
                name: "read_file".into(),
            },
        });
        let task: Arc<dyn AgentTool> = Arc::new(Probe {
            name: "task",
            selector: ToolSelector::Builtin {
                name: "task".into(),
            },
        });
        let mcp: Arc<dyn AgentTool> = Arc::new(Probe {
            name: "mcp__frozen__search",
            selector: ToolSelector::Mcp {
                server_id: "server".into(),
                tool: "search".into(),
            },
        });
        let native_weak = Arc::downgrade(&native);
        let task_weak = Arc::downgrade(&task);
        let inheritance = WorkerInheritance::capture(&[native, task, mcp.clone()]);
        assert!(native_weak.upgrade().is_none());
        assert!(task_weak.upgrade().is_none());
        assert!(Arc::ptr_eq(&inheritance.mcp_tools()[0], &mcp));
        let selected = inheritance.select(&ToolChoice::Parent).unwrap();
        assert_eq!(selected.len(), 2);
        assert_eq!(selected[0].exposed_name, "read_file");
        assert_eq!(selected[1].exposed_name, "mcp__frozen__search");
        assert_eq!(selected[1].exposed_description, "frozen presentation");
    }
}
