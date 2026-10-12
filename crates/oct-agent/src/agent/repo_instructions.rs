//! Repository instruction files (`AGENTS.md` / `CLAUDE.md`): discovery,
//! size budgets and the rendered `<repo-instructions>` system-prompt
//! section, plus the conversation-level frozen prompt environment built
//! around them.
//!
//! Discovery walks the project's `working_dir` up the ancestor chain to the
//! git root (inclusive; no git repository means the working directory
//! alone), then each additional working directory's own root (project
//! order, no chain). Every directory loads the first hit of the candidate
//! chain `AGENTS.md` → `CLAUDE.md` — never both. Contents are capped per
//! file and overall (shallow files truncated first), and the block wraps
//! everything in a fixed envelope with absolute paths.
//!
//! Everything here is deterministic by construction — file order (shallow →
//! deep, then project order), the wrapper text and the truncation behavior
//! never vary between renders of the same inputs. That determinism is what
//! lets [`FrozenPromptEnv`] reuse a conversation's rendered block
//! byte-for-byte across runs: the block sits at the head of every request,
//! so any nondeterminism would break the provider prefix cache per turn.
//!
//! [`FrozenPromptEnv`] generalizes the freeze to the run's MCP tool
//! identities: the prompt's tool list stays stable while servers reconnect
//! or rename underneath, and refreshes happen only at explicit points (the
//! first assembly of a process, the compaction boundaries, `/reload` —
//! see `runtime::prompt_env`).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;

use crate::agent_settings::SnapshotTool;

/// Candidate chain per directory: the first file that exists wins, one file
/// per directory — never both. `AGENTS.md` is the de-facto convention;
/// `CLAUDE.md` is the same-directory fallback for repositories that only
/// carry it.
pub const CANDIDATE_CHAIN: [&str; 2] = ["AGENTS.md", "CLAUDE.md"];

/// Per-file content budget, aligned with the agent settings instructions
/// cap (the same constant philosophy).
pub const PER_FILE_MAX_BYTES: usize = 32 * 1024;

/// Total budget across all loaded files. When exceeded, SHALLOW files
/// (earlier in the list) are cut first — deeper files are the more specific
/// ones and stay complete.
pub const TOTAL_MAX_BYTES: usize = 64 * 1024;

/// Explicit end-of-content marker appended to any truncated file.
const TRUNCATION_MARKER: &str = "\n[truncated]\n";

// ======================================================================
// Loaded files and rendering
// ======================================================================

/// One loaded instruction file: canonical path, (budgeted) content, and
/// whether any budget touched it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstructionFile {
    /// Canonicalized absolute path — what the block tags and the manifest
    /// show (relative paths would be ambiguous across multiple roots).
    pub path: PathBuf,
    pub content: String,
    pub truncated: bool,
}

/// Manifest entry of one loaded file, for log lines and the `/reload`
/// response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstructionFileMeta {
    pub path: String,
    /// Content length in bytes, AFTER budgeting.
    pub bytes: usize,
    pub truncated: bool,
}

/// The rendered `<repo-instructions>` block plus its file manifest. `block`
/// is empty when no files were found — no section header is added.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RenderedRepoInstructions {
    pub block: String,
    pub files: Vec<InstructionFileMeta>,
}

impl RenderedRepoInstructions {
    /// Compact `path(bytes[,truncated])` summary, one entry per file, for
    /// the assembly log lines.
    pub fn file_summary(&self) -> String {
        self.files
            .iter()
            .map(|f| {
                if f.truncated {
                    format!("{}({} bytes, truncated)", f.path, f.bytes)
                } else {
                    format!("{}({} bytes)", f.path, f.bytes)
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// The two header sentences every non-empty block carries: each file's
/// scope (its own directory subtree — an additional root's conventions do
/// NOT override the main repository's) and the subdirectory hint (v1 has no
/// recursive scan; the model reads deeper files itself).
const BLOCK_HEADER: &str = "\
Instruction files loaded from this workspace. Each file applies within its own directory subtree; when files from overlapping scopes conflict, the deeper (more specific) file wins.\n\
Subdirectories may contain their own AGENTS.md — read it before working extensively inside one.\n";

/// Render the `<repo-instructions>` block. An empty list yields an empty
/// string — a workspace without instruction files gets no section at all.
pub fn render_block(files: &[InstructionFile]) -> RenderedRepoInstructions {
    let manifest = files
        .iter()
        .map(|f| InstructionFileMeta {
            path: f.path.display().to_string(),
            bytes: f.content.len(),
            truncated: f.truncated,
        })
        .collect();
    if files.is_empty() {
        return RenderedRepoInstructions {
            block: String::new(),
            files: manifest,
        };
    }
    let mut block = String::from("\n<repo-instructions>\n");
    block.push_str(BLOCK_HEADER);
    for file in files {
        block.push_str(&format!("<file path=\"{}\">\n", file.path.display()));
        block.push_str(&file.content);
        block.push_str("\n</file>\n");
    }
    block.push_str("</repo-instructions>\n");
    RenderedRepoInstructions {
        block,
        files: manifest,
    }
}

// ======================================================================
// Discovery
// ======================================================================

/// Discover and load the instruction files for one project layout: the
/// working directory's ancestor chain up to `git_root` (inclusive, shallow
/// → deep), then each additional directory's own root in project order (no
/// chain — an additional root's parent directory is not part of this
/// project). Synchronous and pure apart from file reads, so unit tests need
/// no git binary.
pub fn discover_chain(
    working_dir: &Path,
    git_root: Option<&Path>,
    additional_dirs: &[PathBuf],
) -> Vec<InstructionFile> {
    let working = working_dir
        .canonicalize()
        .unwrap_or_else(|_| working_dir.to_path_buf());

    // The ancestor chain, shallow → deep: git root first, working directory
    // last. A git root that is not a proper ancestor of the working
    // directory (a mismatched detection) degrades to the working directory
    // alone.
    let mut dirs: Vec<PathBuf> = vec![working.clone()];
    if let Some(root) = git_root
        && let Ok(root) = root.canonicalize()
        && root != working
        && working.starts_with(&root)
    {
        let mut current = working.parent().map(Path::to_path_buf);
        while let Some(dir) = current {
            if dir == root {
                break;
            }
            dirs.push(dir.clone());
            current = dir.parent().map(Path::to_path_buf);
        }
        dirs.push(root);
        dirs.reverse();
    }
    dirs.extend(additional_dirs.iter().cloned());

    let mut files: Vec<InstructionFile> = Vec::new();
    let mut seen_dirs: HashSet<PathBuf> = HashSet::new();
    let mut seen_files: HashSet<PathBuf> = HashSet::new();
    for dir in &dirs {
        // Canonicalized directories dedupe across roots: an additional
        // directory that coincides with a chain directory (or another
        // additional root) is visited — and loads at most one file — once.
        let Ok(dir) = dir.canonicalize() else {
            continue;
        };
        if !seen_dirs.insert(dir.clone()) {
            continue;
        }
        for name in CANDIDATE_CHAIN {
            if let Some(file) = load_candidate(&dir, name) {
                // Canonical FILE dedup: a root AGENTS.md symlinked to a
                // nested chain file must not double-load when the chain
                // reaches the nested directory.
                if seen_files.insert(file.path.clone()) {
                    files.push(file);
                }
                // First hit in the candidate chain wins; the same directory
                // never contributes a second file.
                break;
            }
        }
    }
    enforce_total_budget(&mut files);
    files
}

/// Load one candidate from `dir` when it exists, is a regular file INSIDE
/// the directory after canonicalization (a symlink escaping the directory
/// is rejected), and reads as UTF-8. Any failure skips the candidate
/// silently — a missing or unreadable instruction file is a normal state,
/// not an error.
fn load_candidate(dir: &Path, name: &str) -> Option<InstructionFile> {
    let canonical = dir.join(name).canonicalize().ok()?;
    if !canonical.starts_with(dir) {
        return None; // symlink escaping the expected directory
    }
    if !canonical.is_file() {
        return None;
    }
    // Bounded read: the budget plus one UTF-8 character of lookahead, so a
    // character starting before the budget is complete inside the window
    // (everything past the budget is dropped by truncation anyway). A
    // repository-controlled file can be arbitrarily large — never read it
    // whole. Invalid bytes inside the window load as replacement characters
    // (deterministic) instead of skipping the file whole.
    let mut file = std::fs::File::open(&canonical).ok()?;
    let mut bytes = Vec::with_capacity(PER_FILE_MAX_BYTES.min(64 * 1024));
    {
        use std::io::Read as _;
        std::io::Read::take(&mut file, PER_FILE_MAX_BYTES as u64 + 4)
            .read_to_end(&mut bytes)
            .ok()?;
    }
    let mut content = String::from_utf8_lossy(&bytes).into_owned();
    let mut truncated = false;
    if content.len() > PER_FILE_MAX_BYTES {
        content = truncate_at_char_boundary(&content, PER_FILE_MAX_BYTES).to_owned();
        content.push_str(TRUNCATION_MARKER);
        truncated = true;
    }
    Some(InstructionFile {
        path: canonical,
        content,
        truncated,
    })
}

/// Enforce the total budget: when the loaded files exceed it, SHALLOW files
/// (earlier in the list) are cut first. A file can be cut to nothing but
/// keeps its tag and truncation marker, so the model still learns the file
/// exists.
fn enforce_total_budget(files: &mut [InstructionFile]) {
    let total: usize = files.iter().map(|f| f.content.len()).sum();
    let mut excess = total.saturating_sub(TOTAL_MAX_BYTES);
    if excess == 0 {
        return;
    }
    for file in files.iter_mut() {
        if excess == 0 {
            break;
        }
        let keep = file.content.len() - excess.min(file.content.len());
        let kept = truncate_at_char_boundary(&file.content, keep);
        // The char-boundary backoff can remove a few bytes MORE than the
        // target cut; saturate so multi-byte content cannot underflow here.
        excess = excess.saturating_sub(file.content.len() - kept.len());
        let mut content = kept.to_owned();
        content.push_str(TRUNCATION_MARKER);
        file.content = content;
        file.truncated = true;
    }
}

/// Largest byte prefix of `s` that is at most `max` bytes long and does not
/// split a multi-byte character.
fn truncate_at_char_boundary(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Detect the git repository root of `working_dir` via
/// `git rev-parse --show-toplevel`. Any failure — not a repository, no git
/// binary, non-UTF-8 output, or the 2s timeout — degrades to `None`
/// (discovery then looks at the working directory alone). Follows the git
/// API layer's command pattern: `--no-pager`, terminal prompts disabled,
/// `kill_on_drop`.
pub async fn detect_git_root(working_dir: &Path) -> Option<PathBuf> {
    let output = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::process::Command::new("git")
            .current_dir(working_dir)
            .args(["--no-pager", "rev-parse", "--show-toplevel"])
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GCM_INTERACTIVE", "never")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .output()
            .await
    })
    .await
    .ok()?
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    let line = stdout.lines().next()?.trim();
    if line.is_empty() {
        return None;
    }
    PathBuf::from(line).canonicalize().ok()
}

// ======================================================================
// Frozen prompt environment
// ======================================================================

/// Conversation-level frozen prompt-head environment, keyed by ROOT
/// conversation id and shared by the root run and every subagent run of the
/// tree (see `runtime::prompt_env` for the assembly and refresh policy).
///
/// * `repo` — the rendered block, reused byte-for-byte while clean so the
///   system prompt head never varies between runs;
/// * `mcp_tools` — the MCP tool identities (server id + original tool) and
///   exposed names the prompt's tool list was built from: dispatch maps
///   these onto live instances, so a server rename keeps its frozen
///   exposed name and a disabled server is skipped (dirtying the entry).
#[derive(Debug, Clone)]
pub struct FrozenPromptEnv {
    pub repo: RenderedRepoInstructions,
    pub mcp_tools: Vec<SnapshotTool>,
    dirty: bool,
    /// Identity of this entry's content: a fresh nonzero value per (re)build
    /// (nanosecond timestamp — unique in practice). Anchors stamp the
    /// generation of the prompt their request actually RAN under, so an
    /// in-flight request completing after a `/reload` still carries the OLD
    /// generation and is rejected by the new entry — see
    /// [`Self::accepts_anchor`].
    pub generation: u64,
}

impl FrozenPromptEnv {
    pub(crate) fn new(repo: RenderedRepoInstructions, mcp_tools: Vec<SnapshotTool>) -> Self {
        Self {
            repo,
            mcp_tools,
            dirty: false,
            generation: fresh_generation(),
        }
    }

    /// Whether the next assembly must re-read the files and re-snapshot the
    /// MCP identities instead of reusing this entry.
    pub fn dirty(&self) -> bool {
        self.dirty
    }

    pub(crate) fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Whether a usage anchor stamped with `anchor_generation` may drive the
    /// anchored estimate on this entry: anchored estimates EXCLUDE the system
    /// prompt (the anchor's provider usage already contains it), so an anchor
    /// from a different generation — measured under a different prompt —
    /// would hide an enlarged instruction block from the compaction
    /// watermarks. Generation `0` means unknown (rows written before this
    /// feature) and is always rejected. NOT consumed: every run re-checks its
    /// own view, so a run dying before reporting usage — or concurrent
    /// siblings — cannot leave a stale anchor in charge.
    pub(crate) fn accepts_anchor(&self, anchor_generation: u64) -> bool {
        anchor_generation != 0 && anchor_generation == self.generation
    }
}

/// A fresh per-build generation value: nonzero and unique in practice.
fn fresh_generation() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(1)
        .max(1)
}

/// Handle for marking one root conversation's frozen environment dirty.
/// Carried by the agent context and the compaction requests so the shared
/// compaction/microcompaction transactions can flag their own conversation
/// without a `RunService` reference. Marking only sets the flag — the
/// refresh itself always lands at the NEXT run's assembly, keeping a run's
/// prompt constant for its whole lifetime.
#[derive(Clone)]
pub struct PromptEnvRefresh {
    envs: Arc<DashMap<String, FrozenPromptEnv>>,
    root_conversation_id: String,
}

impl PromptEnvRefresh {
    pub(crate) fn new(
        envs: Arc<DashMap<String, FrozenPromptEnv>>,
        root_conversation_id: &str,
    ) -> Self {
        Self {
            envs,
            root_conversation_id: root_conversation_id.to_string(),
        }
    }

    /// Flag the conversation's entry dirty. A no-op when the entry is gone
    /// (conversation deleted) — nothing left to refresh.
    pub fn mark_dirty(&self) {
        if let Some(mut entry) = self.envs.get_mut(&self.root_conversation_id) {
            entry.value_mut().mark_dirty();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn dir_of(env: &tempfile::TempDir, sub: &[&str]) -> PathBuf {
        let mut dir = env.path().to_path_buf();
        for part in sub {
            dir.push(part);
        }
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn paths_of(files: &[InstructionFile]) -> Vec<PathBuf> {
        files.iter().map(|f| f.path.clone()).collect()
    }

    #[test]
    fn candidate_chain_falls_back_to_claude_md_only_without_agents_md() {
        let env = tempfile::tempdir().unwrap();
        write(&env.path().join("CLAUDE.md"), "claude rules");

        let files = discover_chain(env.path(), None, &[]);
        assert_eq!(files.len(), 1);
        assert!(files[0].path.ends_with("CLAUDE.md"));
        assert_eq!(files[0].content, "claude rules");
    }

    #[test]
    fn agents_md_present_means_claude_md_is_not_loaded() {
        let env = tempfile::tempdir().unwrap();
        write(&env.path().join("AGENTS.md"), "agents rules");
        write(&env.path().join("CLAUDE.md"), "claude rules");

        let files = discover_chain(env.path(), None, &[]);
        assert_eq!(files.len(), 1, "first hit wins; no double load");
        assert!(files[0].path.ends_with("AGENTS.md"));
        assert_eq!(files[0].content, "agents rules");
    }

    #[test]
    fn chain_runs_shallow_to_deep_from_git_root_to_working_dir() {
        let env = tempfile::tempdir().unwrap();
        let root = env.path().join("repo");
        let nested = root.join("crates").join("app");
        write(&root.join("AGENTS.md"), "root-body");
        write(&root.join("crates").join("AGENTS.md"), "crates-body");
        write(&nested.join("AGENTS.md"), "app-body");

        let files = discover_chain(&nested, Some(&root), &[]);
        assert_eq!(
            paths_of(&files),
            vec![
                root.join("AGENTS.md").canonicalize().unwrap(),
                root.join("crates")
                    .join("AGENTS.md")
                    .canonicalize()
                    .unwrap(),
                nested.join("AGENTS.md").canonicalize().unwrap(),
            ],
            "shallow → deep, git root first, working dir last"
        );
        // The rendered block lists the files in the same order (distinctive
        // bodies: header words like "applies" must not collide with finds).
        let rendered = render_block(&files);
        let root_pos = rendered.block.find("root-body").unwrap();
        let crates_pos = rendered.block.find("crates-body").unwrap();
        let app_pos = rendered.block.find("app-body").unwrap();
        assert!(root_pos < crates_pos && crates_pos < app_pos);
    }

    #[test]
    fn no_git_repository_degrades_to_working_dir_alone() {
        let env = tempfile::tempdir().unwrap();
        let root = env.path().join("repo");
        let nested = root.join("inner");
        // An ancestor file exists but no git root is given: it must NOT load.
        write(&root.join("AGENTS.md"), "root");
        write(&nested.join("AGENTS.md"), "inner");

        let files = discover_chain(&nested, None, &[]);
        assert_eq!(
            paths_of(&files),
            vec![nested.join("AGENTS.md").canonicalize().unwrap()]
        );
    }

    #[test]
    fn mismatched_git_root_degrades_to_working_dir_alone() {
        let env = tempfile::tempdir().unwrap();
        let elsewhere = env.path().join("elsewhere");
        let nested = env.path().join("repo").join("inner");
        write(&elsewhere.join("AGENTS.md"), "elsewhere");
        write(&nested.join("AGENTS.md"), "inner");

        // A git root that is not an ancestor of the working dir is ignored.
        let files = discover_chain(&nested, Some(&elsewhere), &[]);
        assert_eq!(
            paths_of(&files),
            vec![nested.join("AGENTS.md").canonicalize().unwrap()]
        );
    }

    #[test]
    fn additional_roots_load_their_own_root_only_after_the_main_chain() {
        let env = tempfile::tempdir().unwrap();
        let working = dir_of(&env, &["work"]);
        let extra = dir_of(&env, &["extra"]);
        // A file in the additional root's PARENT must not be picked up: the
        // parent directory is not part of this project.
        write(&env.path().join("AGENTS.md"), "parent-of-extra");
        write(&extra.join("AGENTS.md"), "extra");
        write(&working.join("AGENTS.md"), "work");

        let files = discover_chain(&working, None, std::slice::from_ref(&extra));
        assert_eq!(
            paths_of(&files),
            vec![
                working.join("AGENTS.md").canonicalize().unwrap(),
                extra.join("AGENTS.md").canonicalize().unwrap(),
            ],
            "main chain first, additional root after; no chain walk for it"
        );
    }

    #[test]
    fn additional_root_equal_to_a_chain_directory_loads_once() {
        let env = tempfile::tempdir().unwrap();
        let root = env.path().join("repo");
        let working = root.join("inner");
        write(&root.join("AGENTS.md"), "root");
        write(&working.join("AGENTS.md"), "inner");

        // The additional root IS the git root — a chain directory already
        // visited: its file must not load a second time.
        let files = discover_chain(&working, Some(&root), std::slice::from_ref(&root));
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].content, "root");
        assert_eq!(files[1].content, "inner");
    }

    #[test]
    fn oversized_file_is_truncated_at_the_per_file_budget() {
        let env = tempfile::tempdir().unwrap();
        let big = "x".repeat(PER_FILE_MAX_BYTES + 5_000);
        write(&env.path().join("AGENTS.md"), &big);

        let files = discover_chain(env.path(), None, &[]);
        assert_eq!(files.len(), 1);
        assert!(files[0].truncated);
        assert!(files[0].content.len() < big.len());
        assert!(files[0].content.ends_with("[truncated]\n"));
        // Kept content is within the budget (plus the marker line).
        assert!(files[0].content.len() <= PER_FILE_MAX_BYTES + TRUNCATION_MARKER.len());
    }

    #[test]
    fn total_budget_truncates_shallow_files_first() {
        let env = tempfile::tempdir().unwrap();
        let root = env.path().join("repo");
        let working = root.join("pkg").join("deep");
        // Three files along the chain, each comfortably under the per-file
        // budget but over the total budget together.
        let body = "y".repeat(28 * 1024);
        write(&root.join("AGENTS.md"), &body);
        write(&root.join("pkg").join("AGENTS.md"), &body);
        write(&working.join("AGENTS.md"), &body);

        let files = discover_chain(&working, Some(&root), &[]);
        assert_eq!(files.len(), 3);
        let total: usize = files.iter().map(|f| f.content.len()).sum();
        assert!(
            total <= TOTAL_MAX_BYTES + 3 * TRUNCATION_MARKER.len(),
            "total {total} must land at the budget (markers aside)"
        );
        // The SHALLOWEST file took the whole cut…
        assert!(files[0].truncated, "git-root file is cut first");
        assert!(files[0].content.len() < body.len());
        // …while the deeper files stay complete.
        assert!(!files[1].truncated);
        assert!(!files[2].truncated);
        assert_eq!(files[2].content, body);
    }

    #[test]
    fn total_budget_with_multibyte_content_never_underflows() {
        let env = tempfile::tempdir().unwrap();
        let root = env.path().join("repo");
        let working = root.join("pkg").join("deep");
        // Multi-byte bodies make the char-boundary backoff remove MORE than
        // the target cut; this used to underflow the excess counter and (in
        // release builds) empty the deeper files entirely.
        let body = "汉".repeat(10 * 1024);
        write(&root.join("AGENTS.md"), &body);
        write(&root.join("pkg").join("AGENTS.md"), &body);
        write(&working.join("AGENTS.md"), &body);

        let files = discover_chain(&working, Some(&root), &[]);
        assert_eq!(files.len(), 3);
        let total: usize = files.iter().map(|f| f.content.len()).sum();
        assert!(
            total <= TOTAL_MAX_BYTES + 3 * TRUNCATION_MARKER.len(),
            "total {total} must land at the budget (markers aside)"
        );
        // The cut lands on the shallow file only; deeper files stay whole.
        assert!(files[0].truncated);
        assert_eq!(files[1].content, body);
        assert_eq!(files[2].content, body);
    }

    #[test]
    fn symlinked_duplicate_across_chain_dirs_loads_once() {
        let env = tempfile::tempdir().unwrap();
        let root = env.path().join("repo");
        let working = root.join("pkg");
        write(&working.join("AGENTS.md"), "shared body");
        // The root's AGENTS.md is a symlink to the nested file: both chain
        // directories resolve to the same canonical file, which must load
        // exactly once.
        #[cfg(unix)]
        std::os::unix::fs::symlink(working.join("AGENTS.md"), root.join("AGENTS.md")).unwrap();

        let files = discover_chain(&working, Some(&root), &[]);
        assert_eq!(
            files.len(),
            1,
            "a file symlinked into two chain directories loads once"
        );
        assert_eq!(files[0].content, "shared body");
    }

    #[test]
    fn oversized_file_with_multibyte_tail_truncates_at_the_window() {
        let env = tempfile::tempdir().unwrap();
        let working = dir_of(&env, &["work"]);
        // Budget of ASCII then multibyte characters straddling the read
        // window's end: the bounded read must not choke on the partial
        // character and the cut must land on a boundary at the budget.
        let mut body = "a".repeat(PER_FILE_MAX_BYTES);
        body.push_str("汉字汉字");
        write(&working.join("AGENTS.md"), &body);

        let files = discover_chain(&working, None, &[]);
        assert_eq!(files.len(), 1);
        assert!(files[0].truncated);
        assert_eq!(
            files[0].content,
            format!("{}{}", "a".repeat(PER_FILE_MAX_BYTES), TRUNCATION_MARKER)
        );
    }

    #[test]
    fn invalid_utf8_inside_the_window_loads_lossily() {
        let env = tempfile::tempdir().unwrap();
        let working = dir_of(&env, &["work"]);
        // Deterministic replacement characters instead of skipping the
        // whole file (the old read_to_string behavior).
        let mut bytes = b"valid prefix ".to_vec();
        bytes.push(0xff);
        bytes.extend_from_slice(b" suffix");
        std::fs::write(working.join("AGENTS.md"), bytes).unwrap();

        let files = discover_chain(&working, None, &[]);
        assert_eq!(files.len(), 1);
        assert!(files[0].content.starts_with("valid prefix"));
        assert!(files[0].content.contains('\u{fffd}'));
        assert!(!files[0].truncated);
    }

    #[test]
    fn anchor_validity_is_generation_equality_not_a_consumed_flag() {
        let entry = FrozenPromptEnv::new(RenderedRepoInstructions::default(), vec![]);
        // Unknown (pre-feature rows) and foreign-generation anchors are
        // rejected — and RE-CHECKED (not consumed) on every assembly, so a
        // first run dying before reporting usage cannot re-enable a stale
        // anchor.
        assert!(!entry.accepts_anchor(0));
        assert!(!entry.accepts_anchor(entry.generation - 1));
        assert!(!entry.accepts_anchor(entry.generation - 1));
        // Usage stamped by a run assembled on THIS entry passes.
        assert!(entry.accepts_anchor(entry.generation));
        // A rebuilt entry is a new identity: even anchors that "look newer"
        // (the in-flight request completing after a /reload) are rejected.
        let rebuilt = FrozenPromptEnv::new(RenderedRepoInstructions::default(), vec![]);
        assert_ne!(rebuilt.generation, entry.generation);
        assert!(!rebuilt.accepts_anchor(entry.generation));
    }

    #[test]
    fn symlink_escaping_the_directory_is_rejected() {
        let env = tempfile::tempdir().unwrap();
        let working = dir_of(&env, &["work"]);
        let outside = dir_of(&env, &["outside"]);
        write(&outside.join("secrets.md"), "do not load me");
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.join("secrets.md"), working.join("AGENTS.md")).unwrap();

        let files = discover_chain(&working, None, &[]);
        assert!(
            files.is_empty(),
            "a symlink out of the directory must not load"
        );
    }

    #[test]
    fn unreadable_or_missing_candidates_are_skipped_silently() {
        let env = tempfile::tempdir().unwrap();
        // Nothing exists at all.
        assert!(discover_chain(env.path(), None, &[]).is_empty());
        // A nonexistent additional root is skipped without an error.
        let files = discover_chain(env.path(), None, &[env.path().join("does-not-exist")]);
        assert!(files.is_empty());
    }

    #[test]
    fn empty_file_list_renders_an_empty_block() {
        let rendered = render_block(&[]);
        assert_eq!(rendered.block, "");
        assert!(rendered.files.is_empty());
    }

    #[test]
    fn rendered_block_wraps_files_with_fixed_header() {
        let env = tempfile::tempdir().unwrap();
        write(&env.path().join("AGENTS.md"), "line1\nline2");
        let files = discover_chain(env.path(), None, &[]);
        let rendered = render_block(&files);

        assert!(rendered.block.starts_with("\n<repo-instructions>\n"));
        assert!(
            rendered
                .block
                .contains("Each file applies within its own directory subtree")
        );
        assert!(
            rendered
                .block
                .contains("the deeper (more specific) file wins")
        );
        // Scope discipline: an additional root's file must NOT be told to
        // override the main repository's.
        assert!(!rendered.block.contains("Later files"));
        assert!(
            rendered
                .block
                .contains("Subdirectories may contain their own AGENTS.md")
        );
        let path = files[0].path.display().to_string();
        assert!(
            rendered
                .block
                .contains(&format!("<file path=\"{path}\">\nline1\nline2\n</file>"))
        );
        assert!(rendered.block.ends_with("</repo-instructions>\n"));
        // The manifest mirrors the loaded file.
        assert_eq!(rendered.files.len(), 1);
        assert_eq!(rendered.files[0].path, path);
        assert_eq!(rendered.files[0].bytes, "line1\nline2".len());
        assert!(!rendered.files[0].truncated);
    }

    #[test]
    fn rendering_is_deterministic_for_identical_inputs() {
        let env = tempfile::tempdir().unwrap();
        write(&env.path().join("AGENTS.md"), "stable content");
        let files = discover_chain(env.path(), None, &[]);
        assert_eq!(render_block(&files), render_block(&files));
    }

    #[test]
    fn truncation_never_splits_a_multibyte_character() {
        let s = "你好世界".repeat(10_000);
        let cut = truncate_at_char_boundary(&s, PER_FILE_MAX_BYTES);
        assert!(cut.len() <= PER_FILE_MAX_BYTES);
        // All bytes before the cut form complete characters.
        assert_eq!(cut.chars().count() * 3, cut.len());
    }

    #[tokio::test]
    async fn git_root_detection_reads_a_real_repository() {
        let env = tempfile::tempdir().unwrap();
        let repo = env.path().join("repo").join("nested");
        std::fs::create_dir_all(&repo).unwrap();
        let status = std::process::Command::new("git")
            .arg("init")
            .arg("--quiet")
            .current_dir(env.path())
            .arg(&repo)
            .status()
            .unwrap();
        assert!(status.success(), "git init must succeed for the test");

        let root = detect_git_root(&repo).await.expect("git root detected");
        assert_eq!(
            root,
            repo.canonicalize().unwrap(),
            "nested dir maps to the repo root"
        );

        // Outside any repository: None (the temp dir's parent chain has no
        // .git — tempfile places it under the system temp root).
        let plain = tempfile::tempdir().unwrap();
        assert_eq!(detect_git_root(plain.path()).await, None);
    }
}
