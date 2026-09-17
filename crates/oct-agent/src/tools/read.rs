use anyhow::{Context, Result};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::shared::{ToolSharedState, file_mtime};
use super::truncate::{MAX_LINE_CHARS, MAX_TOOL_OUTPUT_BYTES, truncate_line, truncate_middle};
use super::{AgentTool, ToolContext, ToolOutput};

pub struct ReadFileTool {
    working_dir: PathBuf,
    shared: Arc<ToolSharedState>,
}

impl ReadFileTool {
    pub fn new(working_dir: PathBuf, shared: Arc<ToolSharedState>) -> Self {
        Self {
            working_dir,
            shared,
        }
    }
}

/// UI-only metadata for a read_file call.
#[derive(Debug, Serialize)]
struct ReadFileDetails {
    title: String,
    path: String,
    start_line: Option<usize>,
    end_line: Option<usize>,
    total_lines: usize,
    shown_lines: usize,
    /// Per-line or middle truncation kicked in (best effort).
    truncated: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ReadFileArgs {
    /// Path to the file to read (relative to working directory or absolute)
    path: String,
    /// Start line number (1-indexed, inclusive). Omit to start from beginning.
    start_line: Option<usize>,
    /// End line number (1-indexed, inclusive). Omit to read until end.
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
        serde_json::to_value(schemars::schema_for!(ReadFileArgs)).unwrap()
    }

    fn title(&self, args: &serde_json::Value) -> String {
        args.get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("read_file")
            .to_string()
    }

    async fn execute(&self, args: serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        let args: ReadFileArgs =
            serde_json::from_value(args).context("Invalid arguments for read_file")?;

        let resolved = match validate_path(&self.working_dir, &args.path) {
            Ok(p) => p,
            Err(output) => return Ok(output),
        };

        if !resolved.is_file() {
            return Ok(ToolOutput::error(format!("{} is not a file", args.path)));
        }

        let content = match tokio::fs::read_to_string(&resolved).await {
            Ok(c) => c,
            Err(e) => return Ok(ToolOutput::error(format!("Failed to read file: {e}"))),
        };

        // Record the read (mtime at read time) so edit_file can enforce its
        // read-before-edit rule. Partial range reads count as reads too.
        if let Some(mtime) = file_mtime(&resolved) {
            self.shared.record_read(&resolved, mtime);
        }

        let lines: Vec<&str> = content.lines().collect();
        let total = lines.len();

        let start = args.start_line.unwrap_or(1).max(1);
        let end = args.end_line.unwrap_or(total).min(total);

        let details = ReadFileDetails {
            title: args.path.clone(),
            path: args.path.clone(),
            start_line: args.start_line,
            end_line: args.end_line,
            total_lines: total,
            shown_lines: end.saturating_sub(start - 1),
            truncated: false,
        };

        if start > total {
            return Ok(ToolOutput::success_with_details(
                format!("(File has {total} lines, requested start_line {start} is beyond end)"),
                details,
            ));
        }

        // Best-effort per-line truncation flag: an over-long line is capped by
        // truncate_line below.
        let mut per_line_truncated = false;
        let selected: Vec<String> = lines[start - 1..end]
            .iter()
            .enumerate()
            .map(|(i, line)| {
                if line.chars().count() > MAX_LINE_CHARS {
                    per_line_truncated = true;
                }
                let line = truncate_line(line, MAX_LINE_CHARS);
                format!("{}. {line}", start + i)
            })
            .collect();

        let header = if args.start_line.is_some() || args.end_line.is_some() {
            format!("({} total lines, showing {start}-{end})\n", total)
        } else {
            String::new()
        };

        // Line ranges are the primary defense against oversized output; the
        // middle truncation is a safety net for pathological files.
        let output = format!("{header}{}", selected.join("\n"));
        let truncated = per_line_truncated || output.len() > MAX_TOOL_OUTPUT_BYTES;

        let details = ReadFileDetails {
            truncated,
            ..details
        };

        Ok(ToolOutput::success_with_details(
            truncate_middle(&output, MAX_TOOL_OUTPUT_BYTES),
            details,
        ))
    }
}

// Re-export for other tools
pub(crate) use validate_path as validate_path_within;
