//! grep: regex content search over the working directory, implemented
//! in-process with the ripgrep library crates (`ignore` walker +
//! `grep-regex`/`grep-searcher` matchers) — no binary is spawned.
//!
//! Supported output modes mirror a subset of rg's: content (default),
//! files-with-matches, and count. Results are bounded by a head limit and a
//! wall-clock budget so a pathological search cannot flood the model's
//! context or hang the agent.

use anyhow::{Context, Result};
use async_trait::async_trait;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{
    BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::read::validate_path_within;
use super::search_common::{relativize, search_walk_builder, timeout_note};
use super::truncate::{truncate_line, truncate_middle, MAX_TOOL_OUTPUT_BYTES};
use super::{AgentTool, ToolContext, ToolOutput};

/// Wall-clock budget for one grep run.
const SEARCH_TIMEOUT: Duration = Duration::from_secs(20);
/// Cap for each matched/context line in the output.
const MAX_RESULT_LINE_CHARS: usize = 1_000;
/// Upper bound for the before/after context line counts.
const MAX_CONTEXT_LINES: usize = 5;
/// Files larger than this are skipped (bounded work per file).
const MAX_FILE_SIZE_BYTES: u64 = 5 * 1024 * 1024;
/// head_limit default / hard cap for content mode (matched lines).
const DEFAULT_CONTENT_LIMIT: usize = 200;
const MAX_CONTENT_LIMIT: usize = 2_000;
/// head_limit default / hard cap for files_with_matches and count mode (files).
const DEFAULT_FILES_LIMIT: usize = 500;
const MAX_FILES_LIMIT: usize = 10_000;
/// Byte budget for the collected content lines before the final output
/// truncation — bounds memory when head_limit is at its cap. Generous relative
/// to the model-visible 50KB so the middle-truncation head/tail survives.
const CONTENT_COLLECT_CAP_BYTES: usize = 400_000;

pub struct GrepTool {
    working_dir: PathBuf,
}

impl GrepTool {
    pub fn new(working_dir: PathBuf) -> Self {
        Self { working_dir }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GrepArgs {
    /// Regular expression to search for (Rust regex syntax)
    pattern: String,
    /// Directory or file to search (relative to working directory or absolute). Defaults to working directory.
    path: Option<String>,
    /// Glob filter for file paths, e.g. "*.rs" (matches at any depth, like rg). Defaults to all files.
    glob: Option<String>,
    /// Case-insensitive matching. Default false.
    case_insensitive: Option<bool>,
    /// Output mode: "content" (default), "files_with_matches", or "count".
    output_mode: Option<String>,
    /// Context lines before the match. Default 0, max 5.
    before_context: Option<usize>,
    /// Context lines after the match. Default 0, max 5.
    after_context: Option<usize>,
    /// Max number of result lines/files returned. Defaults: 200 (content), 500 (files_with_matches/count). Hard caps: 2000 / 10000.
    head_limit: Option<usize>,
}

/// The requested output shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputMode {
    Content,
    FilesWithMatches,
    Count,
}

impl OutputMode {
    fn as_str(self) -> &'static str {
        match self {
            OutputMode::Content => "content",
            OutputMode::FilesWithMatches => "files_with_matches",
            OutputMode::Count => "count",
        }
    }
}

/// UI-only metadata for a grep call. `matched` counts matched lines in
/// content mode, matching files in files_with_matches mode, and total
/// matched lines across files in count mode.
#[derive(Debug, Serialize)]
struct GrepDetails {
    title: String,
    pattern: String,
    output_mode: String,
    matched: usize,
    truncated: bool,
    timed_out: bool,
}

/// Result counts surfaced to the UI via [`GrepDetails`].
struct GrepStats {
    matched: usize,
    truncated: bool,
    timed_out: bool,
}

/// Knobs validated and clamped in `execute` and consumed by `run_search`.
struct SearchConfig {
    mode: OutputMode,
    before_context: usize,
    after_context: usize,
    head_limit: usize,
}

#[async_trait]
impl AgentTool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "Search file contents with a regular expression (rg-like). \
         Returns matching lines as path:line: text, or just file paths / per-file counts \
         via output_mode. Respects .gitignore and skips junk directories. \
         Hidden dotfiles are skipped unless given explicitly via path."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(GrepArgs)).unwrap()
    }

    fn title(&self, args: &serde_json::Value) -> String {
        args.get("pattern")
            .and_then(|v| v.as_str())
            .unwrap_or("grep")
            .to_string()
    }

    async fn execute(&self, args: serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        let args: GrepArgs =
            serde_json::from_value(args).context("Invalid arguments for grep")?;

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

        let mode = match args.output_mode.as_deref() {
            None | Some("content") => OutputMode::Content,
            Some("files_with_matches") => OutputMode::FilesWithMatches,
            Some("count") => OutputMode::Count,
            Some(other) => {
                return Ok(ToolOutput::error(format!(
                    "Invalid output_mode: {other} (expected one of: content, files_with_matches, count)"
                )));
            }
        };

        let (default_limit, hard_cap) = match mode {
            OutputMode::Content => (DEFAULT_CONTENT_LIMIT, MAX_CONTENT_LIMIT),
            OutputMode::FilesWithMatches | OutputMode::Count => {
                (DEFAULT_FILES_LIMIT, MAX_FILES_LIMIT)
            }
        };

        let config = SearchConfig {
            mode,
            before_context: args.before_context.unwrap_or(0).min(MAX_CONTEXT_LINES),
            after_context: args.after_context.unwrap_or(0).min(MAX_CONTEXT_LINES),
            head_limit: args.head_limit.unwrap_or(default_limit).clamp(1, hard_cap),
        };

        let pattern = args.pattern;
        let glob = args.glob;
        let case_insensitive = args.case_insensitive.unwrap_or(false);

        // The compile + walk + search is synchronous CPU/IO-bound work; keep it
        // off the async runtime (regex compilation can take milliseconds on
        // large patterns).
        let output = tokio::task::spawn_blocking(move || {
            let matcher = match RegexMatcherBuilder::new()
                .case_insensitive(case_insensitive)
                .build(&pattern)
            {
                Ok(matcher) => matcher,
                Err(e) => return ToolOutput::error(format!("Invalid regex pattern: {e}")),
            };
            // globset defaults: `*` also matches across `/`, so an rg-style
            // filter like `*.rs` applies at any depth.
            let glob_matcher = match glob.as_deref() {
                Some(glob) => match globset::GlobBuilder::new(glob).build() {
                    Ok(compiled) => Some(compiled.compile_matcher()),
                    Err(e) => return ToolOutput::error(format!("Invalid glob pattern: {e}")),
                },
                None => None,
            };
            let mode_str = config.mode.as_str().to_string();
            let (mut output, stats) =
                run_search(working_canonical, root, matcher, glob_matcher, config);
            output.details = serde_json::to_value(GrepDetails {
                title: pattern.clone(),
                pattern,
                output_mode: mode_str,
                matched: stats.matched,
                truncated: stats.truncated,
                timed_out: stats.timed_out,
            })
            .ok();
            output
        })
        .await
        .context("grep search task panicked")?;

        Ok(output)
    }
}

/// Accumulated state for a content-mode search.
struct ContentResults {
    lines: Vec<String>,
    matched_lines: usize,
    collected_bytes: usize,
    truncated: bool,
    timed_out: bool,
}

/// Sink for content mode: collects `path:line: text` for matches and
/// `path:line- text` for context lines (rg's `-` separator).
struct ContentSink<'a> {
    rel: &'a str,
    results: &'a mut ContentResults,
    deadline: Instant,
    head_limit: usize,
}

impl ContentSink<'_> {
    /// True when the wall-clock budget is spent; marks the run incomplete.
    fn deadline_hit(&mut self) -> bool {
        if Instant::now() >= self.deadline {
            self.results.timed_out = true;
            true
        } else {
            false
        }
    }
}

impl Sink for ContentSink<'_> {
    type Error = io::Error;

    fn matched(
        &mut self,
        _searcher: &Searcher,
        mat: &SinkMatch<'_>,
    ) -> Result<bool, io::Error> {
        if self.deadline_hit() {
            return Ok(false);
        }
        // Stop (without recording) as soon as a match beyond the limit is
        // seen: truncation is detected exactly, without collecting more than
        // the limit or claiming truncation when there are no more matches.
        if self.results.matched_lines >= self.head_limit {
            self.results.truncated = true;
            return Ok(false);
        }
        // Byte budget bounds memory when head_limit is at its cap; the final
        // output still goes through truncate_middle.
        if self.results.collected_bytes >= CONTENT_COLLECT_CAP_BYTES {
            self.results.truncated = true;
            return Ok(false);
        }
        let line = format!(
            "{}:{}: {}",
            self.rel,
            mat.line_number().unwrap_or(0),
            line_text(mat.bytes()),
        );
        self.results.collected_bytes += line.len();
        self.results.lines.push(line);
        self.results.matched_lines += 1;
        Ok(true)
    }

    fn context(
        &mut self,
        _searcher: &Searcher,
        ctx: &SinkContext<'_>,
    ) -> Result<bool, io::Error> {
        if self.deadline_hit() {
            return Ok(false);
        }
        // Once the limit is hit the search is stopping anyway; context lines
        // past the last recorded match are dropped.
        if self.results.matched_lines >= self.head_limit {
            return Ok(false);
        }
        if self.results.collected_bytes >= CONTENT_COLLECT_CAP_BYTES {
            self.results.truncated = true;
            return Ok(false);
        }
        let line = format!(
            "{}:{}- {}",
            self.rel,
            ctx.line_number().unwrap_or(0),
            line_text(ctx.bytes()),
        );
        self.results.collected_bytes += line.len();
        self.results.lines.push(line);
        Ok(true)
    }
}

/// Sink for files_with_matches mode: stops at the first match per file.
#[derive(Default)]
struct FoundSink {
    found: bool,
}

impl Sink for FoundSink {
    type Error = io::Error;

    fn matched(
        &mut self,
        _searcher: &Searcher,
        _mat: &SinkMatch<'_>,
    ) -> Result<bool, io::Error> {
        self.found = true;
        Ok(false)
    }
}

/// Sink for count mode: counts matched lines per file.
#[derive(Default)]
struct CountSink {
    count: usize,
}

impl Sink for CountSink {
    type Error = io::Error;

    fn matched(
        &mut self,
        _searcher: &Searcher,
        _mat: &SinkMatch<'_>,
    ) -> Result<bool, io::Error> {
        self.count += 1;
        Ok(true)
    }
}

/// Decode a matched/context line and cap its length.
fn line_text(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim_end_matches(['\n', '\r']);
    truncate_line(text, MAX_RESULT_LINE_CHARS)
}

/// Walk the search root and collect results for the requested output mode.
/// Runs entirely on a blocking thread. Also returns the UI-facing stats.
fn run_search(
    working_canonical: PathBuf,
    root: PathBuf,
    matcher: grep_regex::RegexMatcher,
    glob_matcher: Option<globset::GlobMatcher>,
    config: SearchConfig,
) -> (ToolOutput, GrepStats) {
    let deadline = Instant::now() + SEARCH_TIMEOUT;
    let mut timed_out = false;
    let mut truncated = false;

    let mut content = ContentResults {
        lines: Vec::new(),
        matched_lines: 0,
        collected_bytes: 0,
        truncated: false,
        timed_out: false,
    };
    // File paths (files_with_matches) or `path: count` lines (count mode).
    let mut files: Vec<String> = Vec::new();
    let mut total_matches = 0usize;

    let mut searcher = SearcherBuilder::new()
        // rg behavior: stop searching a file at the first NUL byte.
        .binary_detection(BinaryDetection::quit(0))
        .line_number(true)
        .before_context(config.before_context)
        .after_context(config.after_context)
        .build();

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
        if let Some(glob) = &glob_matcher {
            let rel = path.strip_prefix(&root).unwrap_or(path);
            // When the root itself is a single file the relative path is
            // empty; match the filter against the file name instead.
            let matched = if rel.as_os_str().is_empty() {
                path.file_name().is_some_and(|name| glob.is_match(Path::new(name)))
            } else {
                glob.is_match(rel)
            };
            if !matched {
                continue;
            }
        }
        if entry.metadata().map_or(true, |m| m.len() > MAX_FILE_SIZE_BYTES) {
            continue;
        }

        let display = relativize(&working_canonical, path);
        match config.mode {
            OutputMode::Content => {
                let mut sink = ContentSink {
                    rel: &display,
                    results: &mut content,
                    deadline,
                    head_limit: config.head_limit,
                };
                // Read errors (permissions, file vanished mid-walk) skip the
                // file, like a quiet rg.
                let _ = searcher.search_path(&matcher, path, &mut sink);
                if content.truncated || content.timed_out {
                    truncated = content.truncated;
                    timed_out = content.timed_out;
                    break;
                }
            }
            OutputMode::FilesWithMatches => {
                let mut sink = FoundSink::default();
                let _ = searcher.search_path(&matcher, path, &mut sink);
                if sink.found {
                    if files.len() >= config.head_limit {
                        truncated = true;
                        break;
                    }
                    files.push(display);
                }
            }
            OutputMode::Count => {
                let mut sink = CountSink::default();
                let _ = searcher.search_path(&matcher, path, &mut sink);
                if sink.count > 0 {
                    if files.len() >= config.head_limit {
                        truncated = true;
                        break;
                    }
                    total_matches += sink.count;
                    files.push(format!("{display}:{}", sink.count));
                }
            }
        }
    }

    // After an early stop (limit or timeout) the counts shown are lower
    // bounds, hence the "at least" phrasing.
    let incomplete = truncated || timed_out;
    let mut out = String::new();
    match config.mode {
        OutputMode::Content => {
            if incomplete {
                out.push_str(&format!(
                    "Found at least {} matching lines\n",
                    content.matched_lines
                ));
            } else {
                out.push_str(&format!("Found {} matching lines\n", content.matched_lines));
            }
            for line in &content.lines {
                out.push_str(line);
                out.push('\n');
            }
        }
        OutputMode::FilesWithMatches => {
            if incomplete {
                out.push_str(&format!("Found at least {} files\n", files.len()));
            } else {
                out.push_str(&format!("Found {} files\n", files.len()));
            }
            for file in &files {
                out.push_str(file);
                out.push('\n');
            }
        }
        OutputMode::Count => {
            if incomplete {
                out.push_str(&format!(
                    "Found at least {total_matches} matches across {} files\n",
                    files.len()
                ));
            } else {
                out.push_str(&format!(
                    "Found {total_matches} matches across {} files\n",
                    files.len()
                ));
            }
            for file in &files {
                out.push_str(file);
                out.push('\n');
            }
        }
    }
    if truncated {
        out.push_str("... [more matches truncated] ...\n");
    }
    if timed_out {
        out.push_str(&timeout_note(SEARCH_TIMEOUT));
        out.push('\n');
    }

    let stats = GrepStats {
        // What "matched" means depends on the mode (see GrepDetails).
        matched: match config.mode {
            OutputMode::Content => content.matched_lines,
            OutputMode::FilesWithMatches => files.len(),
            OutputMode::Count => total_matches,
        },
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
    use std::time::SystemTime;

    /// A ToolContext wired to a throwaway event hub; grep does not
    /// stream, so the context is only needed for the trait signature.
    fn noop_ctx() -> ToolContext {
        ToolContext::new("test", std::sync::Arc::new(crate::agent::EventHub::new()))
    }

    /// Unique temp directory per test (pid + nanos), like the other tool tests.
    fn temp_workspace(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "oct-grep-test-{}-{}-{tag}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn content_mode_reports_matching_lines() {
        let wd = temp_workspace("content");
        std::fs::write(wd.join("a.rs"), "fn main() {}\n// TODO: fix me\n").unwrap();
        std::fs::create_dir_all(wd.join("sub")).unwrap();
        std::fs::write(wd.join("sub/b.rs"), "let x = 1; // TODO: here\n").unwrap();

        let out = GrepTool::new(wd.clone())
            .execute(serde_json::json!({ "pattern": "TODO" }), &noop_ctx())
            .await
            .unwrap();

        assert!(!out.is_error);
        assert!(out.content.contains("Found 2 matching lines"));
        assert!(out.content.contains("a.rs:2: // TODO: fix me"));
        assert!(out.content.contains("sub/b.rs:1: let x = 1; // TODO: here"));

        std::fs::remove_dir_all(&wd).unwrap();
    }

    #[tokio::test]
    async fn case_insensitive_flag_toggles_matching() {
        let wd = temp_workspace("case");
        std::fs::write(wd.join("greet.txt"), "Hello World\n").unwrap();

        let sensitive = GrepTool::new(wd.clone())
            .execute(serde_json::json!({ "pattern": "hello world" }), &noop_ctx())
            .await
            .unwrap();
        assert!(!sensitive.is_error);
        assert!(sensitive.content.contains("Found 0 matching lines"));

        let insensitive = GrepTool::new(wd.clone())
            .execute(serde_json::json!({
                "pattern": "hello world",
                "case_insensitive": true
            }), &noop_ctx())
            .await
            .unwrap();
        assert!(!insensitive.is_error);
        assert!(insensitive.content.contains("Found 1 matching lines"));
        assert!(insensitive.content.contains("greet.txt:1: Hello World"));

        std::fs::remove_dir_all(&wd).unwrap();
    }

    #[tokio::test]
    async fn glob_filter_limits_files_matched() {
        let wd = temp_workspace("globfilter");
        std::fs::write(wd.join("a.rs"), "NEEDLE\n").unwrap();
        std::fs::write(wd.join("b.txt"), "NEEDLE\n").unwrap();
        std::fs::create_dir_all(wd.join("sub")).unwrap();
        std::fs::write(wd.join("sub/c.rs"), "NEEDLE\n").unwrap();

        let out = GrepTool::new(wd.clone())
            .execute(serde_json::json!({ "pattern": "NEEDLE", "glob": "*.rs" }), &noop_ctx())
            .await
            .unwrap();

        assert!(!out.is_error);
        // globset defaults let `*` cross `/`, so the filter applies at any
        // depth (rg-style): a.rs and sub/c.rs match, b.txt does not.
        assert!(out.content.contains("Found 2 matching lines"));
        assert!(out.content.contains("a.rs:1: NEEDLE"));
        assert!(out.content.contains("sub/c.rs:1: NEEDLE"));
        assert!(!out.content.contains("b.txt"));

        std::fs::remove_dir_all(&wd).unwrap();
    }

    #[tokio::test]
    async fn files_with_matches_lists_paths_only() {
        let wd = temp_workspace("files");
        std::fs::write(wd.join("a.txt"), "NEEDLE\n").unwrap();
        std::fs::create_dir_all(wd.join("sub")).unwrap();
        std::fs::write(wd.join("sub/b.txt"), "NEEDLE\nelsewhere\n").unwrap();
        std::fs::write(wd.join("c.txt"), "nothing here\n").unwrap();

        let out = GrepTool::new(wd.clone())
            .execute(serde_json::json!({
                "pattern": "NEEDLE",
                "output_mode": "files_with_matches"
            }), &noop_ctx())
            .await
            .unwrap();

        assert!(!out.is_error);
        assert!(out.content.contains("Found 2 files"));
        assert!(out.content.contains("\na.txt\n"));
        assert!(out.content.contains("\nsub/b.txt\n"));
        assert!(!out.content.contains("c.txt"));
        // No content lines in this mode.
        assert!(!out.content.contains(":1:"));

        std::fs::remove_dir_all(&wd).unwrap();
    }

    #[tokio::test]
    async fn count_mode_reports_per_file_counts() {
        let wd = temp_workspace("count");
        std::fs::write(wd.join("a.txt"), "one\ntwo\none\n").unwrap();
        std::fs::write(wd.join("b.txt"), "one\n").unwrap();

        let out = GrepTool::new(wd.clone())
            .execute(serde_json::json!({ "pattern": "one", "output_mode": "count" }), &noop_ctx())
            .await
            .unwrap();

        assert!(!out.is_error);
        assert!(out.content.contains("Found 3 matches across 2 files"));
        assert!(out.content.contains("\na.txt:2\n"));
        assert!(out.content.contains("\nb.txt:1\n"));

        std::fs::remove_dir_all(&wd).unwrap();
    }

    #[tokio::test]
    async fn head_limit_truncates_with_footer() {
        let wd = temp_workspace("headlimit");
        for i in 0..5 {
            std::fs::write(wd.join(format!("f{i}.txt")), "NEEDLE\n").unwrap();
        }

        let out = GrepTool::new(wd.clone())
            .execute(serde_json::json!({ "pattern": "NEEDLE", "head_limit": 2 }), &noop_ctx())
            .await
            .unwrap();

        assert!(!out.is_error);
        assert!(out.content.contains("Found at least 2 matching lines"));
        assert!(out.content.contains("... [more matches truncated] ..."));
        let result_lines = out
            .content
            .lines()
            .filter(|l| l.contains("NEEDLE"))
            .count();
        assert_eq!(result_lines, 2);

        std::fs::remove_dir_all(&wd).unwrap();
    }

    #[tokio::test]
    async fn gitignore_and_junk_dirs_are_skipped() {
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
        std::fs::write(wd.join("ignored_file.txt"), "NEEDLE\n").unwrap();
        std::fs::write(wd.join("keep_file.txt"), "NEEDLE\n").unwrap();
        // Junk dirs are skipped even when not gitignored, at any depth.
        std::fs::create_dir_all(wd.join("node_modules")).unwrap();
        std::fs::write(wd.join("node_modules/pkg.js"), "NEEDLE\n").unwrap();
        std::fs::create_dir_all(wd.join("sub/node_modules")).unwrap();
        std::fs::write(wd.join("sub/node_modules/nested.js"), "NEEDLE\n").unwrap();

        let out = GrepTool::new(wd.clone())
            .execute(serde_json::json!({
                "pattern": "NEEDLE",
                "output_mode": "files_with_matches"
            }), &noop_ctx())
            .await
            .unwrap();

        assert!(!out.is_error);
        assert!(out.content.contains("Found 1 files"));
        assert!(out.content.contains("keep_file.txt"));
        assert!(!out.content.contains("ignored_file.txt"));
        assert!(!out.content.contains("node_modules"));

        std::fs::remove_dir_all(&wd).unwrap();
    }

    #[tokio::test]
    async fn binary_file_with_nul_bytes_yields_no_matches() {
        let wd = temp_workspace("binary");
        std::fs::write(wd.join("blob.bin"), b"\x00NEEDLE\x00binary data\n").unwrap();
        std::fs::write(wd.join("text.txt"), "NEEDLE\n").unwrap();

        let out = GrepTool::new(wd.clone())
            .execute(serde_json::json!({
                "pattern": "NEEDLE",
                "output_mode": "files_with_matches"
            }), &noop_ctx())
            .await
            .unwrap();

        assert!(!out.is_error);
        assert!(out.content.contains("Found 1 files"));
        assert!(out.content.contains("text.txt"));
        assert!(!out.content.contains("blob.bin"));

        std::fs::remove_dir_all(&wd).unwrap();
    }

    #[tokio::test]
    async fn path_outside_working_dir_is_denied() {
        let wd = temp_workspace("outside");
        let outside = wd.parent().unwrap().to_path_buf();

        let out = GrepTool::new(wd.clone())
            .execute(serde_json::json!({
                "pattern": "anything",
                "path": outside.display().to_string()
            }), &noop_ctx())
            .await
            .unwrap();

        assert!(out.is_error);
        assert!(out.content.contains("outside the working directory"));

        std::fs::remove_dir_all(&wd).unwrap();
    }

    #[tokio::test]
    async fn single_file_root_with_glob_filter_matches_file_name() {
        let wd = temp_workspace("fileglob");
        std::fs::write(wd.join("a.rs"), "NEEDLE\n").unwrap();

        let out = GrepTool::new(wd.clone())
            .execute(serde_json::json!({
                "pattern": "NEEDLE", "path": "a.rs", "glob": "*.rs"
            }), &noop_ctx())
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("Found 1 matching lines"));

        // A filter that does not match the file name yields zero results.
        let out = GrepTool::new(wd.clone())
            .execute(serde_json::json!({
                "pattern": "NEEDLE", "path": "a.rs", "glob": "*.txt"
            }), &noop_ctx())
            .await
            .unwrap();
        assert!(out.content.contains("Found 0 matching lines"));

        std::fs::remove_dir_all(&wd).unwrap();
    }

    #[tokio::test]
    async fn invalid_regex_is_an_error() {
        let wd = temp_workspace("badregex");

        let out = GrepTool::new(wd.clone())
            .execute(serde_json::json!({ "pattern": "([" }), &noop_ctx())
            .await
            .unwrap();

        assert!(out.is_error);
        assert!(out.content.contains("Invalid regex pattern"));

        std::fs::remove_dir_all(&wd).unwrap();
    }
}
