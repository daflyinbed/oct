//! The `task` tool: the model-facing delegation interface.
//!
//! First version: exactly two required arguments (`description` = short UI
//! title, `prompt` = full instructions). The role is fixed to `worker` by
//! the host — the model cannot choose agent types, cwd, background mode or
//! model overrides. The tool WAITS for the child's settlement (parallel
//! `task` calls in one round run concurrently through the loop's existing
//! parallel tool execution) and returns the envelope built from the settled
//! run row.

use anyhow::{Context, Result};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::agent::EventHub;
use crate::db::projects::Project;
use crate::tools::shared::WorkspaceFileLocks;
use crate::tools::workspace::WorkspaceRoots;
use crate::tools::{AgentTool, ToolContext, ToolOutput};

use super::manager::{SubagentManager, TaskError, build_task_envelope};

/// A run's toolset as seen through the task tool's slot: the `OnceLock` is
/// filled (with a WEAK reference) right after the run assembly builds the
/// full toolset — see [`TaskSpawnConfig::parent_toolset`] for why weak.
pub type ToolsetSlot = Arc<std::sync::OnceLock<std::sync::Weak<Vec<Arc<dyn AgentTool>>>>>;

/// Everything the manager needs to spawn a worker for ONE parent run. Built
/// by the RunService when it assembles the root run's toolset (see the
/// `parent_toolset` slot note there).
pub struct TaskSpawnConfig {
    /// The conversation whose run is delegating.
    pub parent_conversation_id: String,
    /// The delegating run row id.
    pub parent_run_id: String,
    pub root_conversation_id: String,
    pub root_run_id: String,
    /// Project snapshot at spawn time (working dir + additional dirs).
    pub project: Project,
    pub roots: WorkspaceRoots,
    /// The project's task-tree-shared file locks.
    pub locks: Arc<WorkspaceFileLocks>,
    /// The parent's effective model — the child inherits it in v1.
    pub provider_id: String,
    pub model_id: String,
    pub context_window: Option<u32>,
    /// The parent run's cancellation token: children derive `child_token()`
    /// from it, so cancelling the parent cascades to every worker.
    pub parent_cancel: CancellationToken,
    /// The parent run's hub: `subagent_updated` events land here.
    pub parent_hub: Arc<EventHub>,
    /// Filled by the run assembly right after the full toolset (task
    /// included) is built; children re-derive their set as
    /// parent-minus-task. Deliberately a Weak slot: a strong Arc here would
    /// form a toolset → task tool → slot → toolset reference cycle, leaking
    /// the toolset (and with it this run's event hub) past the run's end —
    /// the SSE streams would never reach EOF. While `task` executes, the
    /// loop's own toolset Arc keeps the upgrade valid.
    pub parent_toolset: ToolsetSlot,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct TaskArgs {
    /// Short UI title for the task (shown on the task card / agent tab).
    description: String,
    /// Full instructions for the worker: goal, necessary context, files in
    /// scope, acceptance criteria.
    prompt: String,
}

pub struct TaskTool {
    manager: Arc<SubagentManager>,
    spawn: Arc<TaskSpawnConfig>,
}

impl TaskTool {
    pub fn new(manager: Arc<SubagentManager>, spawn: Arc<TaskSpawnConfig>) -> Self {
        Self { manager, spawn }
    }
}

#[async_trait]
impl AgentTool for TaskTool {
    fn name(&self) -> &str {
        "task"
    }

    fn description(&self) -> &str {
        "Delegate a self-contained subtask to a fresh worker agent that runs in its own \
         conversation with the same workspace and tools. Waits for the result; multiple \
         task calls in the same round run concurrently."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(TaskArgs)).unwrap()
    }

    fn title(&self, args: &serde_json::Value) -> String {
        args.get("description")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| "task".to_string())
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let args: TaskArgs =
            serde_json::from_value(args).context("Invalid arguments for task")?;
        if args.description.trim().is_empty() || args.prompt.trim().is_empty() {
            return Ok(ToolOutput::error(
                "task requires non-empty 'description' and 'prompt' arguments",
            ));
        }

        let service = self.manager.service_handle();
        match self
            .manager
            .run_task(&self.spawn, ctx.call_id(), &args.description, &args.prompt)
            .await
        {
            Err(e) => Ok(error_output(&e)),
            Ok(run) => {
                let Some(service) = service else {
                    // An unwired service means the process is tearing down;
                    // the run already settled, only the envelope is missing.
                    return Ok(error_output(&TaskError {
                        message: "run service is shutting down".to_string(),
                        agent_id: Some(run.conversation_id.clone()),
                        run_id: Some(run.id.clone()),
                    }));
                };
                let envelope = match build_task_envelope(&service.pool, &run).await {
                    Ok(envelope) => envelope,
                    Err(e) => {
                        return Ok(error_output(&TaskError {
                            message: format!("failed to build the task envelope: {e:#}"),
                            agent_id: Some(run.conversation_id.clone()),
                            run_id: Some(run.id.clone()),
                        }))
                    }
                };
                Ok(ToolOutput {
                    content: envelope.content,
                    is_error: envelope.is_error,
                    images: Vec::new(),
                    details: Some(envelope.details),
                })
            }
        }
    }
}

/// Error output carrying the ids when they are known (the TaskCard can still
/// open the child conversation).
fn error_output(e: &TaskError) -> ToolOutput {
    let mut details = json!({ "kind": "subagent_task" });
    if let Some(agent_id) = &e.agent_id {
        details["agent_id"] = json!(agent_id);
    }
    if let Some(run_id) = &e.run_id {
        details["run_id"] = json!(run_id);
    }
    ToolOutput::error_with_details(format!("subagent task failed to start: {e}"), details)
}
