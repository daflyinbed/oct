//! The `task_output` tool: the model's read/wait surface for
//! background bash tasks started via `execute_command(run_in_background)`.
//! Reads bounded slices of the task's output FILE (the only durable output
//! store), optionally blocking until the task settles. Root runs only —
//! children never see it (derive_child_toolset excludes it).

use anyhow::{Context, Result};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;

use super::{AgentTool, ToolContext, ToolOutput};
use crate::db::background as task_db;
use crate::runtime::background::{BackgroundTaskManager, read_output_chunk};

/// Bytes returned when the caller does not request more.
const DEFAULT_READ_BYTES: u64 = 32 * 1024;
/// Upper bound for a requested read.
const MAX_READ_BYTES: u64 = 1024 * 1024;
/// Bounded wait default (only used with block = true).
const DEFAULT_BLOCK_SECS: u64 = 30;
/// Upper bound for a bounded wait.
const MAX_BLOCK_SECS: u64 = 600;

#[derive(Debug, Deserialize, JsonSchema)]
struct TaskOutputArgs {
    /// The background task id returned by execute_command.
    task_id: String,
    /// Wait for the task to reach a terminal state first (bounded by
    /// timeout_secs). Default false: return the current snapshot.
    block: Option<bool>,
    /// Bound for block = true, in seconds. Default 30, maximum 600.
    timeout_secs: Option<u64>,
    /// Byte offset to read from (paging from the start). Omit to read the
    /// TAIL of the output instead.
    offset: Option<u64>,
    /// Maximum bytes to return. Default 32768, maximum 1048576.
    max_bytes: Option<u64>,
}

pub struct TaskOutputTool {
    manager: Arc<BackgroundTaskManager>,
    conversation_id: String,
}

impl TaskOutputTool {
    pub fn new(manager: Arc<BackgroundTaskManager>, conversation_id: String) -> Self {
        Self {
            manager,
            conversation_id,
        }
    }

    /// Error listing this conversation's task ids when the requested id is
    /// unknown (mirrors the subagent tool's known-ids trick: the model can
    /// self-correct instead of guessing).
    async fn not_found(&self, task_id: &str) -> ToolOutput {
        let known = task_db::list_visible_tasks(self.manager.pool(), &self.conversation_id)
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
}

#[async_trait]
impl AgentTool for TaskOutputTool {
    fn name(&self) -> &str {
        "task_output"
    }

    fn description(&self) -> &str {
        "Read the status and output of a background command started with \
         execute_command(run_in_background: true). Returns a bounded slice of \
         the output: by default the TAIL; pass offset to page from the start \
         (the result carries the next offset). With block: true it waits \
         (bounded by timeout_secs) for the task to finish first. Tasks notify \
         you on completion — polling is only for when you actively need the \
         interim output."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(TaskOutputArgs)).unwrap()
    }

    fn title(&self, args: &serde_json::Value) -> String {
        args.get("task_id")
            .and_then(|v| v.as_str())
            .map(|id| format!("task_output {id}"))
            .unwrap_or_else(|| "task_output".to_string())
    }

    async fn execute(&self, args: serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        let args: TaskOutputArgs =
            serde_json::from_value(args).context("Invalid arguments for task_output")?;

        let Some(mut task) = task_db::get_visible_task(&self.manager.pool(), &args.task_id)
            .await
            .map_err(anyhow::Error::from)?
            .filter(|t| t.conversation_id == self.conversation_id)
        else {
            return Ok(self.not_found(&args.task_id).await);
        };

        // Bounded wait for a running task: watch the registry's done
        // channel, then re-read the settled row (the DB is the truth).
        if task.status == "running" && args.block.unwrap_or(false) {
            let timeout = Duration::from_secs(
                args.timeout_secs
                    .unwrap_or(DEFAULT_BLOCK_SECS)
                    .clamp(1, MAX_BLOCK_SECS),
            );
            if let Some(mut done) = self.manager.active_task(&args.task_id).map(|(d, _)| d) {
                let _ = tokio::time::timeout(timeout, async move {
                    let _ = done.wait_for(|v| *v).await;
                })
                .await;
                if let Some(refreshed) =
                    task_db::get_visible_task(&self.manager.pool(), &args.task_id)
                        .await
                        .map_err(anyhow::Error::from)?
                {
                    task = refreshed;
                }
            }
        }

        // A missing exit code does not identify a signal: running and lost
        // tasks also have no exit status recorded.
        let exit_code = task
            .exit_code
            .map(|code| code.to_string())
            .unwrap_or_else(|| {
                if task.status == "running" {
                    "pending (still running)".into()
                } else {
                    "unknown".into()
                }
            });

        let max_bytes = args
            .max_bytes
            .unwrap_or(DEFAULT_READ_BYTES)
            .clamp(1, MAX_READ_BYTES);
        let chunk = match read_output_chunk(
            std::path::Path::new(&task.output_path),
            args.offset,
            max_bytes,
        )
        .await
        {
            Ok(chunk) => chunk,
            // A missing/unreadable file (e.g. the output directory was
            // cleaned) still answers with the task's status.
            Err(e) => {
                return Ok(ToolOutput::success(format!(
                    "task_id: {}\nstatus: {}\nexit_code: {}\noutput unavailable: {e}",
                    task.id, task.status, exit_code,
                )));
            }
        };

        let mut content = format!(
            "task_id: {}\nstatus: {}\nexit_code: {}\noutput_bytes: {}\n",
            task.id, task.status, exit_code, chunk.total_bytes,
        );
        if let Some(error) = &task.error {
            content.push_str(&format!("error: {error}\n"));
        }
        if chunk.start > 0 {
            content.push_str(&format!(
                "[{} bytes of earlier output omitted]\n",
                chunk.start
            ));
        }
        content.push_str("output:\n");
        content.push_str(&chunk.content);
        if let Some(next) = chunk.next_offset {
            content.push_str(&format!(
                "\n[more output follows: use offset={next} to continue]"
            ));
        }
        if chunk.content.is_empty() && chunk.total_bytes == 0 {
            content.push_str("(no output yet)");
        }
        let details = serde_json::json!({
            "kind": "background_task_output",
            "task_id": task.id,
            "status": task.status,
            "exit_code": task.exit_code,
            "output_bytes": chunk.total_bytes,
        });
        Ok(ToolOutput::success_with_details(content, details))
    }
}
