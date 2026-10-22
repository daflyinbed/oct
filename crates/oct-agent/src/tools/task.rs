//! The `task` tool: the model-facing delegation interface.
//!
//! First version: exactly two required arguments (`description` = short UI
//! title, `prompt` = full instructions). The role is fixed to `worker` by
//! the host — the model cannot choose agent types, cwd, background mode or
//! model overrides. The tool WAITS for the child's settlement (parallel
//! `task` calls in one round run concurrently through the loop's existing
//! parallel tool execution) and returns the envelope built from the settled
//! run row.

use crate::tool_contract::{
    AgentTool, ToolContext, ToolOutput,
    ports::{TaskDelegation, TaskError},
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

#[derive(Debug, Deserialize, JsonSchema)]
struct TaskArgs {
    /// Short UI title for the task (shown on the task card / agent tab).
    description: String,
    /// Full instructions for the worker: goal, necessary context, files in
    /// scope, acceptance criteria.
    prompt: String,
}

pub struct TaskTool {
    delegation: Arc<dyn TaskDelegation>,
}

impl TaskTool {
    pub fn new(delegation: Arc<dyn TaskDelegation>) -> Self {
        Self { delegation }
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
        let args: TaskArgs = serde_json::from_value(args).context("Invalid arguments for task")?;
        if args.description.trim().is_empty() || args.prompt.trim().is_empty() {
            return Ok(ToolOutput::error(
                "task requires non-empty 'description' and 'prompt' arguments",
            ));
        }

        match self
            .delegation
            .delegate(ctx.call_id(), &args.description, &args.prompt)
            .await
        {
            Err(e) => Ok(error_output(&e)),
            Ok(result) => Ok(ToolOutput {
                content: result.content,
                is_error: result.is_error,
                images: Vec::new(),
                details: Some(result.details),
            }),
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
