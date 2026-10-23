//! The `task_stop` tool: stop a background bash task. Cancels
//! the task's own token (SIGTERM → grace → SIGKILL on the process group —
//! see the runtime's `kill_group_graceful`), waits (bounded) for the
//! settlement and reports the settled row. Root runs only.

use anyhow::{Context, Result};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use std::sync::Arc;

use crate::tool_contract::AgentTool;
use crate::tool_contract::ToolContext;
use crate::tool_contract::ToolOutput;
use crate::tool_contract::ports::{BackgroundCommands, BackgroundStop};

#[derive(Debug, Deserialize, JsonSchema)]
struct TaskStopArgs {
    /// The background task id returned by execute_command.
    task_id: String,
}

pub struct TaskStopTool {
    commands: Arc<dyn BackgroundCommands>,
}

impl TaskStopTool {
    pub fn new(commands: Arc<dyn BackgroundCommands>) -> Self {
        Self { commands }
    }
}

#[async_trait]
impl AgentTool for TaskStopTool {
    fn name(&self) -> &str {
        "task_stop"
    }

    fn description(&self) -> &str {
        "Stop a running background command by task id. The whole process \
         group is terminated (graceful, then forced). Only stop tasks you no \
         longer need — there is no way to resume a stopped command."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(TaskStopArgs)).unwrap()
    }

    fn title(&self, args: &serde_json::Value) -> String {
        args.get("task_id")
            .and_then(|v| v.as_str())
            .map(|id| format!("task_stop {id}"))
            .unwrap_or_else(|| "task_stop".to_string())
    }

    async fn execute(&self, args: serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        let args: TaskStopArgs =
            serde_json::from_value(args).context("Invalid arguments for task_stop")?;

        match self.commands.stop(&args.task_id).await? {
            BackgroundStop::Unknown { known_ids } => Ok(ToolOutput::error(format!(
                "Unknown background task '{}' in this conversation. Known task ids: {}",
                args.task_id,
                if known_ids.is_empty() {
                    "(none)".to_string()
                } else {
                    known_ids.join(", ")
                }
            ))),
            BackgroundStop::Settling => Ok(ToolOutput::error(format!(
                "Task {} is settling; you will be notified when settlement finishes",
                args.task_id
            ))),
            BackgroundStop::Settled { status } => Ok(stop_result(&args.task_id, &status)),
        }
    }
}

fn stop_result(task_id: &str, status: &str) -> ToolOutput {
    if status == "cancelled" {
        ToolOutput::success(format!("Task {task_id} stopped (status: cancelled)"))
    } else {
        ToolOutput::success(format!(
            "Task {task_id} had already exited before the stop (status: {status})"
        ))
    }
}
