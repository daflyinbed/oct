use anyhow::{Context, Result};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use std::sync::Arc;

use super::shared::{AgentReadState, WorkspaceFileLocks, file_mtime};
use super::workspace::WorkspaceRoots;
use super::{AgentTool, ToolContext, ToolOutput};

pub struct WriteFileTool {
    roots: WorkspaceRoots,
    /// Task-tree-shared per-file locks (root + subagents share one instance).
    locks: Arc<WorkspaceFileLocks>,
    /// This agent's own read records.
    reads: Arc<AgentReadState>,
}

impl WriteFileTool {
    pub fn new(
        roots: WorkspaceRoots,
        locks: Arc<WorkspaceFileLocks>,
        reads: Arc<AgentReadState>,
    ) -> Self {
        Self {
            roots,
            locks,
            reads,
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

/// Resolution for write targets lives on [`WorkspaceRoots`]:
/// `resolve_write_path` doesn't require the file to exist already — for
/// paths that don't exist yet, the deepest existing ancestor is resolved
/// via canonicalize (following symlinks to their real targets) and the
/// remaining components are re-appended. Nothing is rejected at this layer:
/// outside-workspace targets and symlinked destinations are the permission
/// layer's call.

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

        // Resolution no longer rejects outside-workspace targets or symlink
        // traversal; the outside classification feeds the permission gate.
        let resolved = match self.roots.resolve_write_path(&args.path) {
            Ok(resolved) => resolved.path,
            Err(output) => return Ok(output),
        };

        // Serialize with edit_file's read-modify-write on the same file so a
        // concurrent edit cannot be clobbered (or vice versa). The locks are
        // shared across the whole task tree, so another agent's edit/write on
        // the same path queues here too.
        let lock = self.locks.lock_for(&resolved);
        let guard = lock.lock_owned().await;

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
        match super::atomic_file::write(&resolved, &args.content, guard).await {
            Ok(_guard) => {
                // Record the post-write mtime so a subsequent edit_file by
                // the SAME agent is not blocked as stale.
                if let Some(mtime) = file_mtime(&resolved) {
                    self.reads.record_write(&resolved, mtime);
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
    use std::path::PathBuf;

    /// A ToolContext wired to a throwaway event hub; write_file does
    /// not stream, so the context is only needed for the trait signature.
    fn noop_ctx() -> ToolContext {
        ToolContext::new(
            "test",
            std::sync::Arc::new(crate::agent::EventHub::new()),
            tokio_util::sync::CancellationToken::new(),
        )
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
            let write = WriteFileTool::new(
                WorkspaceRoots::new(dir.clone(), vec![]),
                Arc::new(WorkspaceFileLocks::new()),
                Arc::new(AgentReadState::new()),
            );
            Self { write, dir }
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
        assert_eq!(
            std::fs::read_to_string(fx.dir.join("a.txt")).unwrap(),
            "hello"
        );

        let out = fx.write("a.txt", "world").await;
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(
            std::fs::read_to_string(fx.dir.join("a.txt")).unwrap(),
            "world"
        );
    }

    #[tokio::test]
    async fn write_to_absolute_path_outside_the_roots_succeeds() {
        // Outside-workspace targets are no longer rejected here; allowing or
        // asking is the permission layer's call.
        let fx = Fixture::new("abs-outside");
        let target = std::env::temp_dir().join(format!(
            "oct-write-test-outside-{}-{}.txt",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let out = fx.write(&target.display().to_string(), "x").await;
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "x");
        let _ = std::fs::remove_file(&target);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_through_symlinked_dir_lands_at_real_target() {
        // The ancestor canonicalization follows the symlink, so the write
        // lands in the link's real destination (what an approval card would
        // show); no symlink-traversal rejection anymore.
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

        let tool = WriteFileTool::new(
            WorkspaceRoots::new(base.join("wd"), vec![]),
            Arc::new(WorkspaceFileLocks::new()),
            Arc::new(AgentReadState::new()),
        );
        let out = tool
            .execute(
                json!({ "path": "link/new.txt", "content": "x" }),
                &noop_ctx(),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(
            std::fs::read_to_string(base.join("outside/new.txt")).unwrap(),
            "x"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_through_dangling_symlink_creates_its_target() {
        // A dangling link cannot be canonicalized, so it stays literal in
        // the resolved path and write(2) follows it, creating the target.
        let fx = Fixture::new("dangling");
        std::os::unix::fs::symlink("no-such-target", fx.dir.join("dang")).unwrap();

        let out = fx.write("dang", "x").await;
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(
            std::fs::read_to_string(fx.dir.join("no-such-target")).unwrap(),
            "x"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_via_existing_symlink_lands_at_real_target() {
        // An existing symlinked file canonicalizes to its real target before
        // writing: the destination outside the roots is the permission
        // layer's call, and the write lands on the target file, not the link.
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

        let tool = WriteFileTool::new(
            WorkspaceRoots::new(base.join("wd"), vec![]),
            Arc::new(WorkspaceFileLocks::new()),
            Arc::new(AgentReadState::new()),
        );
        let out = tool
            .execute(json!({ "path": "f.txt", "content": "x" }), &noop_ctx())
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        // The real target was overwritten, through the still-intact link.
        assert_eq!(
            std::fs::read_to_string(base.join("outside/f.txt")).unwrap(),
            "x"
        );
        assert!(
            std::fs::symlink_metadata(base.join("wd/f.txt"))
                .unwrap()
                .file_type()
                .is_symlink()
        );

        let _ = std::fs::remove_dir_all(&base);
    }
}
