use anyhow::{Context, Result};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::shared::{ToolSharedState, file_mtime};
use super::{AgentTool, ToolContext, ToolOutput};

pub struct WriteFileTool {
    working_dir: PathBuf,
    shared: Arc<ToolSharedState>,
}

impl WriteFileTool {
    pub fn new(working_dir: PathBuf, shared: Arc<ToolSharedState>) -> Self {
        Self {
            working_dir,
            shared,
        }
    }
}

/// UI-only metadata for a write_file call.
#[derive(Debug, Serialize)]
struct WriteFileDetails {
    title: String,
    path: String,
    bytes: usize,
    create_dirs: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WriteFileArgs {
    /// Path to write to (relative to working directory or absolute)
    path: String,
    /// Content to write to the file
    content: String,
    /// Create parent directories if they don't exist. Default false.
    create_dirs: Option<bool>,
}

/// Validate that the target path would be within the working directory.
/// Unlike read's validate_path, this doesn't require the file to exist already:
/// for paths that don't exist yet, the deepest existing ancestor is resolved
/// via canonicalize (following symlinks) and the remaining components are
/// re-appended, refusing to traverse any symlink in the non-existent tail (it
/// could redirect the write outside the sandbox).
pub(crate) fn validate_write_path(
    working_dir: &Path,
    requested: &str,
) -> Result<PathBuf, ToolOutput> {
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
        // For files that don't exist yet, walk up to the deepest existing
        // ancestor and canonicalize it so symlinked directories cannot point
        // outside the sandbox.
        let mut ancestor = candidate.clone();
        let mut rest: Vec<std::ffi::OsString> = Vec::new();
        while !ancestor.exists() {
            let Some(name) = ancestor.file_name().map(|n| n.to_os_string()) else {
                break;
            };
            let Some(parent) = ancestor.parent().map(Path::to_path_buf) else {
                break;
            };
            rest.push(name);
            ancestor = parent;
        }
        let resolved_ancestor = ancestor
            .canonicalize()
            .map_err(|e| ToolOutput::error(format!("Path resolution error: {e}")))?;
        let mut resolved = resolved_ancestor;
        // Once a component does not exist, deeper ones cannot either, so only
        // symlink-check the existing prefix but keep appending the rest.
        let mut exists_so_far = true;
        for name in rest.iter().rev() {
            resolved.push(name);
            if !exists_so_far {
                continue;
            }
            match std::fs::symlink_metadata(&resolved) {
                // Refuse to write through a symlink (existing or dangling);
                // its target could be outside the working directory.
                Ok(m) if m.file_type().is_symlink() => {
                    return Err(ToolOutput::error(format!(
                        "Access denied: {requested} traverses a symbolic link"
                    )));
                }
                Ok(_) => {}
                Err(_) => exists_so_far = false,
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
        serde_json::to_value(schemars::schema_for!(WriteFileArgs)).unwrap()
    }

    fn title(&self, args: &serde_json::Value) -> String {
        args.get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("write_file")
            .to_string()
    }

    async fn execute(&self, args: serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        let args: WriteFileArgs =
            serde_json::from_value(args).context("Invalid arguments for write_file")?;

        let resolved = match validate_write_path(&self.working_dir, &args.path) {
            Ok(p) => p,
            Err(output) => return Ok(output),
        };

        // Serialize with edit_file's read-modify-write on the same file so a
        // concurrent edit cannot be clobbered (or vice versa).
        let lock = self.shared.lock_for(&resolved);
        let _guard = lock.lock().await;

        if args.create_dirs.unwrap_or(false) {
            if let Some(parent) = resolved.parent()
                && let Err(e) = tokio::fs::create_dir_all(parent).await
            {
                return Ok(ToolOutput::error(format!(
                    "Failed to create directories: {e}"
                )));
            }
        } else if let Some(parent) = resolved.parent()
            && !parent.exists()
        {
            return Ok(ToolOutput::error(format!(
                "Parent directory does not exist: {}. Set create_dirs to true to create it.",
                parent.display()
            )));
        }

        let bytes = args.content.len();
        match tokio::fs::write(&resolved, &args.content).await {
            Ok(()) => {
                // Record the post-write mtime so a subsequent edit_file in the
                // same run is not blocked as stale.
                if let Some(mtime) = file_mtime(&resolved) {
                    self.shared.record_write(&resolved, mtime);
                }
                let details = WriteFileDetails {
                    title: args.path.clone(),
                    path: args.path.clone(),
                    bytes,
                    create_dirs: args.create_dirs.unwrap_or(false),
                };
                Ok(ToolOutput::success_with_details(
                    format!("Successfully wrote {bytes} bytes to {}", args.path),
                    details,
                ))
            }
            Err(e) => Ok(ToolOutput::error(format!("Failed to write file: {e}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A ToolContext wired to a throwaway event hub; write_file does
    /// not stream, so the context is only needed for the trait signature.
    fn noop_ctx() -> ToolContext {
        ToolContext::new("test", std::sync::Arc::new(crate::agent::EventHub::new()))
    }

    struct Fixture {
        dir: PathBuf,
        write: WriteFileTool,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "oct-write-test-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let shared = Arc::new(ToolSharedState::new());
            Self {
                write: WriteFileTool::new(dir.clone(), shared),
                dir,
            }
        }

        async fn write(&self, path: &str, content: &str) -> ToolOutput {
            self.write
                .execute(json!({ "path": path, "content": content }), &noop_ctx())
                .await
                .unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[tokio::test]
    async fn write_and_overwrite_succeed() {
        let fx = Fixture::new("basic");
        let out = fx.write("a.txt", "hello").await;
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(std::fs::read_to_string(fx.dir.join("a.txt")).unwrap(), "hello");

        let out = fx.write("a.txt", "world").await;
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(std::fs::read_to_string(fx.dir.join("a.txt")).unwrap(), "world");
    }

    #[tokio::test]
    async fn create_path_denies_absolute_path_outside() {
        let fx = Fixture::new("abs-outside");
        let out = fx
            .write("/oct-write-test-definitely-not-inside/a.txt", "x")
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("outside the working directory"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn create_path_denies_symlinked_dir_escape() {
        let base = std::env::temp_dir().join(format!(
            "oct-write-test-symdir-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(base.join("wd")).unwrap();
        std::fs::create_dir_all(base.join("outside")).unwrap();
        std::os::unix::fs::symlink("../outside", base.join("wd/link")).unwrap();

        let shared = Arc::new(ToolSharedState::new());
        let tool = WriteFileTool::new(base.join("wd"), shared);
        let out = tool
            .execute(json!({ "path": "link/new.txt", "content": "x" }), &noop_ctx())
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("outside the working directory"));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn create_path_denies_dangling_symlink_target() {
        let fx = Fixture::new("dangling");
        std::os::unix::fs::symlink("no-such-target", fx.dir.join("dang")).unwrap();

        let out = fx.write("dang", "x").await;
        assert!(out.is_error);
        assert!(out.content.contains("symbolic link"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn existing_symlink_pointing_outside_is_denied() {
        let base = std::env::temp_dir().join(format!(
            "oct-write-test-symfile-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(base.join("wd")).unwrap();
        std::fs::create_dir_all(base.join("outside")).unwrap();
        std::fs::write(base.join("outside/f.txt"), "secret").unwrap();
        std::os::unix::fs::symlink("../outside/f.txt", base.join("wd/f.txt")).unwrap();

        let shared = Arc::new(ToolSharedState::new());
        let tool = WriteFileTool::new(base.join("wd"), shared);
        let out = tool
            .execute(json!({ "path": "f.txt", "content": "x" }), &noop_ctx())
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("outside the working directory"));

        let _ = std::fs::remove_dir_all(&base);
    }
}
