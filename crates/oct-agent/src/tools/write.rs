use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use std::path::{Path, PathBuf};

use super::{AgentTool, ToolOutput};

pub struct WriteFileTool {
    working_dir: PathBuf,
}

impl WriteFileTool {
    pub fn new(working_dir: PathBuf) -> Self {
        Self { working_dir }
    }
}

#[derive(Debug, Deserialize)]
struct WriteFileArgs {
    path: String,
    content: String,
    create_dirs: Option<bool>,
}

/// Validate that the target path would be within the working directory.
/// Unlike read's validate_path, this doesn't require the file to exist already.
fn validate_write_path(working_dir: &Path, requested: &str) -> Result<PathBuf, ToolOutput> {
    let candidate = if Path::new(requested).is_absolute() {
        PathBuf::from(requested)
    } else {
        working_dir.join(requested)
    };

    let working_resolved = working_dir
        .canonicalize()
        .map_err(|e| ToolOutput::error(format!("Working directory error: {e}")))?;

    // For new files, we check the parent directory
    let check_path = if candidate.exists() {
        candidate
            .canonicalize()
            .map_err(|e| ToolOutput::error(format!("Path resolution error: {e}")))?
    } else {
        // For files that don't exist yet, resolve as much of the path as possible
        let mut resolved = working_resolved.clone();
        let relative = candidate
            .strip_prefix(working_dir)
            .unwrap_or(candidate.as_path());
        for component in relative.components() {
            match component {
                std::path::Component::Normal(c) => resolved.push(c),
                std::path::Component::ParentDir => {
                    resolved.pop();
                }
                _ => {}
            }
        }
        resolved
    };

    if !check_path.starts_with(&working_resolved) {
        return Err(ToolOutput::error(format!(
            "Access denied: {requested} is outside the working directory"
        )));
    }

    Ok(check_path)
}

#[async_trait]
impl AgentTool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        "Write content to a file, overwriting any existing content. \
         Set create_dirs to true to create parent directories if they don't exist."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to write to (relative to working directory or absolute)"
                },
                "content": {
                    "type": "string",
                    "description": "Content to write to the file"
                },
                "create_dirs": {
                    "type": "boolean",
                    "description": "Create parent directories if they don't exist. Default false."
                }
            },
            "required": ["path", "content"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let args: WriteFileArgs =
            serde_json::from_value(args).context("Invalid arguments for write_file")?;

        let resolved = match validate_write_path(&self.working_dir, &args.path) {
            Ok(p) => p,
            Err(output) => return Ok(output),
        };

        if args.create_dirs.unwrap_or(false) {
            if let Some(parent) = resolved.parent() {
                if let Err(e) = tokio::fs::create_dir_all(parent).await {
                    return Ok(ToolOutput::error(format!(
                        "Failed to create directories: {e}"
                    )));
                }
            }
        } else if let Some(parent) = resolved.parent() {
            if !parent.exists() {
                return Ok(ToolOutput::error(format!(
                    "Parent directory does not exist: {}. Set create_dirs to true to create it.",
                    parent.display()
                )));
            }
        }

        let bytes = args.content.len();
        match tokio::fs::write(&resolved, &args.content).await {
            Ok(()) => Ok(ToolOutput::success(format!(
                "Successfully wrote {bytes} bytes to {}",
                args.path
            ))),
            Err(e) => Ok(ToolOutput::error(format!("Failed to write file: {e}"))),
        }
    }
}
