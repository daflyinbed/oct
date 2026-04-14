use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use std::path::{Path, PathBuf};

use super::{AgentTool, ToolOutput};

pub struct ReadFileTool {
    working_dir: PathBuf,
}

impl ReadFileTool {
    pub fn new(working_dir: PathBuf) -> Self {
        Self { working_dir }
    }
}

#[derive(Debug, Deserialize)]
struct ReadFileArgs {
    path: String,
    start_line: Option<usize>,
    end_line: Option<usize>,
}

/// Validate that the resolved path is within the working directory.
pub(crate) fn validate_path(working_dir: &Path, requested: &str) -> Result<PathBuf, ToolOutput> {
    let candidate = if Path::new(requested).is_absolute() {
        PathBuf::from(requested)
    } else {
        working_dir.join(requested)
    };

    let resolved = candidate
        .canonicalize()
        .map_err(|e| ToolOutput::error(format!("Path not found: {requested} ({e})")))?;

    let working_resolved = working_dir
        .canonicalize()
        .map_err(|e| ToolOutput::error(format!("Working directory error: {e}")))?;

    if !resolved.starts_with(&working_resolved) {
        return Err(ToolOutput::error(format!(
            "Access denied: {requested} is outside the working directory"
        )));
    }

    Ok(resolved)
}

#[async_trait]
impl AgentTool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Read the contents of a file. Returns the file content with line numbers. \
         Use start_line and end_line to read a specific range."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to read (relative to working directory or absolute)"
                },
                "start_line": {
                    "type": "integer",
                    "description": "Start line number (1-indexed, inclusive). Omit to start from beginning."
                },
                "end_line": {
                    "type": "integer",
                    "description": "End line number (1-indexed, inclusive). Omit to read until end."
                }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let args: ReadFileArgs =
            serde_json::from_value(args).context("Invalid arguments for read_file")?;

        let resolved = match validate_path(&self.working_dir, &args.path) {
            Ok(p) => p,
            Err(output) => return Ok(output),
        };

        if !resolved.is_file() {
            return Ok(ToolOutput::error(format!(
                "{} is not a file",
                args.path
            )));
        }

        let content = match tokio::fs::read_to_string(&resolved).await {
            Ok(c) => c,
            Err(e) => return Ok(ToolOutput::error(format!("Failed to read file: {e}"))),
        };

        let lines: Vec<&str> = content.lines().collect();
        let total = lines.len();

        let start = args.start_line.unwrap_or(1).max(1);
        let end = args.end_line.unwrap_or(total).min(total);

        if start > total {
            return Ok(ToolOutput::success(format!(
                "(File has {total} lines, requested start_line {start} is beyond end)"
            )));
        }

        let selected: Vec<String> = lines[start - 1..end]
            .iter()
            .enumerate()
            .map(|(i, line)| format!("{}. {line}", start + i))
            .collect();

        let header = if args.start_line.is_some() || args.end_line.is_some() {
            format!("({} total lines, showing {start}-{end})\n", total)
        } else {
            String::new()
        };

        Ok(ToolOutput::success(format!(
            "{header}{}",
            selected.join("\n")
        )))
    }
}

// Re-export for other tools
pub(crate) use validate_path as validate_path_within;
