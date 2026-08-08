use anyhow::{Context, Result};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::PathBuf;

use super::{AgentTool, ToolOutput};

pub struct ListDirTool {
    working_dir: PathBuf,
}

impl ListDirTool {
    pub fn new(working_dir: PathBuf) -> Self {
        Self { working_dir }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ListDirArgs {
    /// Directory path to list (relative to working directory). Defaults to working directory root.
    path: Option<String>,
    /// Maximum depth to recurse. Default is 2.
    depth: Option<usize>,
}

fn should_ignore(name: &str) -> bool {
    matches!(
        name,
        ".git" | "node_modules" | "target" | "__pycache__" | ".DS_Store" | "dist" | "build"
    )
}

async fn list_recursive(
    dir: &std::path::Path,
    prefix: &str,
    depth: usize,
    max_depth: usize,
    output: &mut String,
) -> Result<()> {
    if depth > max_depth {
        return Ok(());
    }

    let mut entries = tokio::fs::read_dir(dir).await?;
    let mut items = Vec::new();

    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') && depth == 0 && should_ignore(&name) {
            continue;
        }
        if should_ignore(&name) {
            continue;
        }
        let file_type = entry.file_type().await?;
        items.push((name, entry.path(), file_type.is_dir()));
    }

    items.sort_by(|a, b| {
        // Directories first, then alphabetical
        b.2.cmp(&a.2).then(a.0.cmp(&b.0))
    });

    for (i, (name, path, is_dir)) in items.iter().enumerate() {
        let is_last = i == items.len() - 1;
        let connector = if is_last { "└── " } else { "├── " };
        let suffix = if *is_dir { "/" } else { "" };

        output.push_str(&format!("{prefix}{connector}{name}{suffix}\n"));

        if *is_dir && depth < max_depth {
            let next_prefix = if is_last {
                format!("{prefix}    ")
            } else {
                format!("{prefix}│   ")
            };
            Box::pin(list_recursive(path, &next_prefix, depth + 1, max_depth, output)).await?;
        }
    }

    Ok(())
}

#[async_trait]
impl AgentTool for ListDirTool {
    fn name(&self) -> &str {
        "list_dir"
    }

    fn description(&self) -> &str {
        "List files and directories in a tree-like format. \
         Ignores common non-essential directories (node_modules, .git, target, etc.)."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(ListDirArgs)).unwrap()
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let args: ListDirArgs =
            serde_json::from_value(args).context("Invalid arguments for list_dir")?;

        let target = if let Some(ref path) = args.path {
            match super::read::validate_path_within(&self.working_dir, path) {
                Ok(p) => p,
                Err(output) => return Ok(output),
            }
        } else {
            self.working_dir
                .canonicalize()
                .unwrap_or_else(|_| self.working_dir.clone())
        };

        if !target.is_dir() {
            return Ok(ToolOutput::error(format!(
                "{} is not a directory",
                args.path.as_deref().unwrap_or(".")
            )));
        }

        let max_depth = args.depth.unwrap_or(2);
        let dir_name = target
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| ".".to_string());

        let mut output = format!("{dir_name}/\n");
        list_recursive(&target, "", 0, max_depth, &mut output).await?;

        Ok(ToolOutput::success(output))
    }
}
