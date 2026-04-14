use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use std::path::PathBuf;

use super::{AgentTool, ToolOutput};

pub struct ExecuteCommandTool {
    working_dir: PathBuf,
}

impl ExecuteCommandTool {
    pub fn new(working_dir: PathBuf) -> Self {
        Self { working_dir }
    }
}

#[derive(Debug, Deserialize)]
struct ExecuteCommandArgs {
    command: String,
    working_dir: Option<String>,
    timeout_secs: Option<u64>,
}

#[async_trait]
impl AgentTool for ExecuteCommandTool {
    fn name(&self) -> &str {
        "execute_command"
    }

    fn description(&self) -> &str {
        "Execute a shell command and return its output. \
         Commands run via `sh -c` on Unix. Use timeout_secs to limit execution time."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to execute"
                },
                "working_dir": {
                    "type": "string",
                    "description": "Working directory for the command (relative to project root). Defaults to project root."
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Timeout in seconds. Default is 30."
                }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let args: ExecuteCommandArgs =
            serde_json::from_value(args).context("Invalid arguments for execute_command")?;

        let cwd = if let Some(ref dir) = args.working_dir {
            match super::read::validate_path_within(&self.working_dir, dir) {
                Ok(p) => p,
                Err(output) => return Ok(output),
            }
        } else {
            self.working_dir.clone()
        };

        let timeout = std::time::Duration::from_secs(args.timeout_secs.unwrap_or(30));

        let child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&args.command)
            .current_dir(&cwd)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn();

        let child = match child {
            Ok(c) => c,
            Err(e) => return Ok(ToolOutput::error(format!("Failed to spawn command: {e}"))),
        };

        // Wait with timeout, but handle kill separately since wait_with_output takes ownership
        let result = tokio::time::timeout(timeout, async {
            child.wait_with_output().await
        }).await;

        match result {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                let exit_code = output.status.code().unwrap_or(-1);

                let mut result = String::new();
                if !stdout.is_empty() {
                    result.push_str(&stdout);
                }
                if !stderr.is_empty() {
                    if !result.is_empty() {
                        result.push('\n');
                    }
                    result.push_str("[stderr]\n");
                    result.push_str(&stderr);
                }
                if result.is_empty() {
                    result = format!("(no output, exit code: {exit_code})");
                } else {
                    result.push_str(&format!("\n[exit code: {exit_code}]"));
                }

                if exit_code == 0 {
                    Ok(ToolOutput::success(result))
                } else {
                    Ok(ToolOutput::error(result))
                }
            }
            Ok(Err(e)) => Ok(ToolOutput::error(format!("Command failed: {e}"))),
            Err(_) => {
                // Timeout - process was consumed by wait_with_output so we can't kill it directly
                // The process will be cleaned up when dropped
                Ok(ToolOutput::error(format!(
                    "Command timed out after {} seconds",
                    timeout.as_secs()
                )))
            }
        }
    }
}
