//! The `task_stop` tool: stop a background bash task. Cancels
//! the task's own token (SIGTERM → grace → SIGKILL on the process group —
//! see the runtime's `kill_group_graceful`), waits (bounded) for the
//! settlement and reports the settled row. Root runs only.

use anyhow::{Context, Result};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use std::sync::Arc;

use super::{AgentTool, ToolContext, ToolOutput};
use crate::db::background as task_db;
use crate::runtime::background::BackgroundTaskManager;

#[derive(Debug, Deserialize, JsonSchema)]
struct TaskStopArgs {
    /// The background task id returned by execute_command.
    task_id: String,
}

pub struct TaskStopTool {
    manager: Arc<BackgroundTaskManager>,
    conversation_id: String,
}

impl TaskStopTool {
    pub fn new(manager: Arc<BackgroundTaskManager>, conversation_id: String) -> Self {
        Self {
            manager,
            conversation_id,
        }
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

        // Scope check BEFORE any cancel: a task id from another conversation
        // must not be stopped from here.
        let Some(row) = task_db::get_visible_task(self.manager.pool(), &args.task_id)
            .await?
            .filter(|t| t.conversation_id == self.conversation_id)
        else {
            return Ok(unknown_task(&self.manager, &self.conversation_id, &args.task_id).await);
        };

        match row.status.as_str() {
            "running" => {
                // Active: cancel + bounded wait; report the settled row.
                if let Some(settled) = self.manager.cancel_and_wait(&args.task_id).await {
                    return Ok(stop_result(&settled.id, &settled.status));
                }
                // Not in the registry but still running: the supervisor is
                // mid-settlement (the DB write happens before the registry
                // removal) — report what the row says right now.
                Ok(ToolOutput::error(format!(
                    "Task {} is settling; you will be notified when settlement finishes",
                    row.id
                )))
            }
            "lost" => {
                // The backend restart already terminated the process; just
                // persist the intent so the notification says cancelled.
                task_db::cancel_lost_task(self.manager.pool(), &args.task_id).await?;
                Ok(stop_result(&row.id, "cancelled"))
            }
            status => Ok(stop_result(&row.id, status)),
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

async fn unknown_task(
    manager: &Arc<BackgroundTaskManager>,
    conversation_id: &str,
    task_id: &str,
) -> ToolOutput {
    let known = task_db::list_visible_tasks(manager.pool(), conversation_id)
        .await
        .unwrap_or_default();
    let ids: Vec<String> = known.iter().map(|t| t.id.clone()).collect();
    ToolOutput::error(format!(
        "Unknown background task '{task_id}' in this conversation. Known task ids: {}",
        if ids.is_empty() {
            "(none)".to_string()
        } else {
            ids.join(", ")
        }
    ))
}
