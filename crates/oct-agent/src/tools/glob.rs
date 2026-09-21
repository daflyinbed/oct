//! glob: find files by name pattern, implemented in-process with the ripgrep
//! library crates (`ignore` walker + `globset` matching) — no binary spawned.
//!
//! Results are sorted by modification time, most recent first (Claude Code's
//! `--sort=modified`), which helps the model find the files it is actively
//! working on. The walk respects the same ignore rules as grep (gitignore,
//! junk directories, no symlink following) so both tools agree on what exists.

use anyhow::{Context, Result};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use super::read::validate_path_within;
use super::search_common::{relativize, search_walk_builder, timeout_note};
use super::truncate::{truncate_middle, MAX_TOOL_OUTPUT_BYTES};
use super::{AgentTool, ToolContext, ToolOutput};

/// Wall-clock budget for one glob run.
const GLOB_TIMEOUT: Duration = Duration::from_secs(10);
/// Default and hard cap for the number of files returned.
const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 1_000;

pub struct GlobTool {
    working_dir: PathBuf,
}

impl GlobTool {
    pub fn new(working_dir: PathBuf) -> Self {
        Self { working_dir }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GlobArgs {
    /// Glob pattern to match file paths against, e.g. "**/*.ts". `*` does not cross "/"; use a "**/" prefix for recursive matching.
    pattern: String,
    /// Directory or file to search (relative to working directory or absolute). Defaults to working directory.
    path: Option<String>,
    /// Max number of files returned. Default 100, maximum 1000.
    limit: Option<usize>,
}

/// UI-only metadata for a glob call. `found` counts ALL matches discovered
/// during the walk, regardless of the result limit.
#[derive(Debug, Serialize)]
struct GlobDetails {
    title: String,
    pattern: String,
    found: usize,
    truncated: bool,
    timed_out: bool,
}

/// Result counts surfaced to the UI via [`GlobDetails`].
struct GlobStats {
    found: usize,
    truncated: bool,
    timed_out: bool,
}

#[async_trait]
impl AgentTool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }

    fn description(&self) -> &str {
        "Find files by glob pattern (e.g. \"**/*.ts\"), sorted by most recently modified. \
         `*` does not cross \"/\"; use a \"**/\" prefix for recursive matching. \
         Respects .gitignore and skips junk directories like node_modules. \
         Hidden dotfiles are skipped."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(GlobArgs)).unwrap()
    }

    fn title(&self, args: &serde_json::Value) -> String {
        args.get("pattern")
            .and_then(|v| v.as_str())
            .unwrap_or("glob")
            .to_string()
    }

    async fn execute(&self, args: serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        let args: GlobArgs =
            serde_json::from_value(args).context("Invalid arguments for glob")?;

        let root = match args.path.as_deref() {
            Some(path) => match validate_path_within(&self.working_dir, path) {
                Ok(resolved) => resolved,
                Err(output) => return Ok(output),
            },
            None => self
                .working_dir
                .canonicalize()
                .unwrap_or_else(|_| self.working_dir.clone()),
        };
        let working_canonical = self
            .working_dir
            .canonicalize()
            .unwrap_or_else(|_| self.working_dir.clone());

        // Literal separators: standard glob syntax where `*` does not cross
        // `/`, so `**/` is what makes a pattern recursive (e.g. `*.ts` is
        // top-level only while `**/*.ts` matches at any depth).
        let pattern = args.pattern;
        let limit = args.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);

        // The compile + walk is synchronous IO-bound work; keep it off the
        // async runtime.
        let output = tokio::task::spawn_blocking(move || {
            let matcher = match globset::GlobBuilder::new(&pattern)
                .literal_separator(true)
                .build()
            {
                Ok(glob) => glob.compile_matcher(),
                Err(e) => return ToolOutput::error(format!("Invalid glob pattern: {e}")),
            };
            let (mut output, stats) = run_glob(working_canonical, root, matcher, limit);
            output.details = serde_json::to_value(GlobDetails {
                title: pattern.clone(),
                pattern,
                found: stats.found,
                truncated: stats.truncated,
                timed_out: stats.timed_out,
            })
            .ok();
            output
        })
        .await
        .context("glob search task panicked")?;

        Ok(output)
    }
}

/// Walk the search root, collect paths matching the glob, and render them.
/// Runs entirely on a blocking thread. Also returns the UI-facing stats.
fn run_glob(
    working_canonical: PathBuf,
    root: PathBuf,
    matcher: globset::GlobMatcher,
    limit: usize,
) -> (ToolOutput, GlobStats) {
    let deadline = Instant::now() + GLOB_TIMEOUT;
    let mut timed_out = false;
    let mut matches: Vec<(SystemTime, String)> = Vec::new();

    for entry in search_walk_builder(&root).build() {
        let Ok(entry) = entry else { continue };
        if Instant::now() >= deadline {
            timed_out = true;
            break;
        }
        // Only regular files: skips directories, and symlinks never appear as
        // files because the walker does not follow links.
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let path = entry.path();
        let rel = path.strip_prefix(&root).unwrap_or(path);
        // When the root itself is a single file the relative path is empty;
        // match the pattern against the file name instead.
        let matched = if rel.as_os_str().is_empty() {
            path.file_name().is_some_and(|name| matcher.is_match(Path::new(name)))
        } else {
            matcher.is_match(rel)
        };
        if !matched {
            continue;
        }
        let mtime = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        matches.push((mtime, relativize(&working_canonical, path)));
    }

    // Most recently modified first. Ties keep walk order (stable sort).
    matches.sort_by_key(|(mtime, _)| std::cmp::Reverse(*mtime));

    let total = matches.len();
    let shown = total.min(limit);
    let truncated = total > shown;

    // "at least" because after truncation/timeout more matches may exist.
    let mut out = if truncated || timed_out {
        format!("Found at least {shown} files\n")
    } else {
        format!("Found {shown} files\n")
    };
    for (_, path) in matches.iter().take(shown) {
        out.push_str(path);
        out.push('\n');
    }
    if truncated {
        out.push_str(&format!(
            "... [{} more matches truncated] ...\n",
            total - shown
        ));
    }
    if timed_out {
        out.push_str(&timeout_note(GLOB_TIMEOUT));
        out.push('\n');
    }

    let stats = GlobStats {
        found: total,
        truncated: truncated || out.len() > MAX_TOOL_OUTPUT_BYTES,
        timed_out,
    };

    // Zero matches is a successful answer, not an error.
    (
        ToolOutput::success(truncate_middle(&out, MAX_TOOL_OUTPUT_BYTES)),
        stats,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// A ToolContext wired to a throwaway event hub; glob does not
    /// stream, so the context is only needed for the trait signature.
    fn noop_ctx() -> ToolContext {
        ToolContext::new("test", std::sync::Arc::new(crate::agent::EventHub::new()))
    }

    /// Unique temp directory per test (pid + nanos), like the other tool tests.
    fn temp_workspace(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "oct-glob-test-{}-{}-{tag}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Pin a file's mtime for deterministic sort order.
    fn set_mtime(path: &Path, mtime: SystemTime) {
        let file = std::fs::File::options().write(true).open(path).unwrap();
        file.set_times(std::fs::FileTimes::new().set_modified(mtime)).unwrap();
    }

    fn unix_time(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[tokio::test]
    async fn finds_files_sorted_by_mtime_descending() {
        let wd = temp_workspace("mtime");
        std::fs::write(wd.join("old.txt"), "old\n").unwrap();
        std::fs::write(wd.join("mid.txt"), "mid\n").unwrap();
        std::fs::write(wd.join("new.txt"), "new\n").unwrap();
        std::fs::create_dir_all(wd.join("sub")).unwrap();
        std::fs::write(wd.join("sub/deep.txt"), "deep\n").unwrap();
        set_mtime(&wd.join("old.txt"), unix_time(1_000));
        set_mtime(&wd.join("mid.txt"), unix_time(2_000));
        set_mtime(&wd.join("new.txt"), unix_time(3_000));
        set_mtime(&wd.join("sub/deep.txt"), unix_time(4_000));

        // `**/` matches at any depth; newest file first.
        let recursive = GlobTool::new(wd.clone())
            .execute(serde_json::json!({ "pattern": "**/*.txt" }), &noop_ctx())
            .await
            .unwrap();
        assert!(!recursive.is_error);
        assert!(recursive.content.contains("Found 4 files"));
        let lines: Vec<&str> = recursive.content.lines().collect();
        assert_eq!(lines[1], "sub/deep.txt");
        assert_eq!(lines[2], "new.txt");
        assert_eq!(lines[3], "mid.txt");
        assert_eq!(lines[4], "old.txt");

        // Literal separators: `*.txt` stays at the top level.
        let top_level = GlobTool::new(wd.clone())
            .execute(serde_json::json!({ "pattern": "*.txt" }), &noop_ctx())
            .await
            .unwrap();
        assert!(top_level.content.contains("Found 3 files"));
        assert!(!top_level.content.contains("sub/deep.txt"));

        std::fs::remove_dir_all(&wd).unwrap();
    }

    #[tokio::test]
    async fn gitignore_is_respected() {
        let wd = temp_workspace("gitignore");
        // The ignore crate only applies gitignore rules inside a git
        // repository (rg's `require_git` default), so init one.
        let status = std::process::Command::new("git")
            .arg("init")
            .current_dir(&wd)
            .status()
            .unwrap();
        assert!(status.success());

        std::fs::write(wd.join(".gitignore"), "ignored_file.txt\n").unwrap();
        std::fs::write(wd.join("ignored_file.txt"), "x").unwrap();
        std::fs::write(wd.join("keep_file.txt"), "x").unwrap();

        let out = GlobTool::new(wd.clone())
            .execute(serde_json::json!({ "pattern": "**/*.txt" }), &noop_ctx())
            .await
            .unwrap();

        assert!(!out.is_error);
        assert!(out.content.contains("Found 1 files"));
        assert!(out.content.contains("keep_file.txt"));
        assert!(!out.content.contains("ignored_file.txt"));

        std::fs::remove_dir_all(&wd).unwrap();
    }

    #[tokio::test]
    async fn limit_truncates_with_footer() {
        let wd = temp_workspace("limit");
        std::fs::write(wd.join("a.txt"), "a").unwrap();
        std::fs::write(wd.join("b.txt"), "b").unwrap();
        std::fs::write(wd.join("c.txt"), "c").unwrap();

        let out = GlobTool::new(wd.clone())
            .execute(serde_json::json!({ "pattern": "**/*.txt", "limit": 2 }), &noop_ctx())
            .await
            .unwrap();

        assert!(!out.is_error);
        assert!(out.content.contains("Found at least 2 files"));
        assert!(out.content.contains("... [1 more matches truncated] ..."));
        let listed = out
            .content
            .lines()
            .filter(|l| l.ends_with(".txt"))
            .count();
        assert_eq!(listed, 2);

        std::fs::remove_dir_all(&wd).unwrap();
    }

    #[tokio::test]
    async fn single_file_root_matches_against_file_name() {
        let wd = temp_workspace("fileroot");
        std::fs::write(wd.join("a.txt"), "x").unwrap();

        let out = GlobTool::new(wd.clone())
            .execute(serde_json::json!({ "pattern": "*.txt", "path": "a.txt" }), &noop_ctx())
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("Found 1 files"));
        assert!(out.content.contains("a.txt"));

        std::fs::remove_dir_all(&wd).unwrap();
    }

    #[tokio::test]
    async fn bad_pattern_is_an_error() {
        let wd = temp_workspace("badpattern");

        let out = GlobTool::new(wd.clone())
            .execute(serde_json::json!({ "pattern": "[" }), &noop_ctx())
            .await
            .unwrap();

        assert!(out.is_error);
        assert!(out.content.contains("Invalid glob pattern"));

        std::fs::remove_dir_all(&wd).unwrap();
    }
}
