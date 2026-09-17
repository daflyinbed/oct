//! Shared plumbing for the search tools (`grep` and `glob`): a directory
//! walker with sandbox-safe, rg-like ignore defaults plus a path display
//! helper. Both tools walk the same way so they agree on what is searchable.

use std::path::Path;
use std::time::Duration;
use ignore::overrides::OverrideBuilder;
use ignore::WalkBuilder;

/// Junk directory names that are always skipped by the search walker, even
/// when they are not covered by a gitignore file. Keep in sync with
/// [`super::listdir::should_ignore`]; the hidden entries there (`.git`,
/// `.DS_Store`) are already covered by the walker's `hidden(true)` setting.
const JUNK_DIR_NAMES: &[&str] = &["node_modules", "target", "__pycache__", "dist", "build"];

/// Build a walker over `root` with standard rg-ish defaults:
///
/// - skip hidden files/directories (covers `.git`),
/// - respect `.gitignore` / global gitignore / `.git/info/exclude`
///   (gitignore rules only apply inside a git repository, like rg),
/// - read ignore files from parent directories,
/// - never follow symlinks (sandbox safety: a link could point outside the
///   working directory).
///
/// On top of that, the junk directories listed above are excluded via an
/// override so they are skipped regardless of gitignore state. Override globs
/// use gitignore semantics: a glob without a `/` matches the name at any
/// depth, so `!node_modules` also prunes `sub/node_modules`.
pub(crate) fn search_walk_builder(root: &Path) -> WalkBuilder {
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .parents(true)
        .follow_links(false);

    let mut overrides = OverrideBuilder::new(root);
    for name in JUNK_DIR_NAMES {
        overrides
            .add(&format!("!{name}"))
            .expect("static junk directory glob is always valid");
    }
    if let Ok(ov) = overrides.build() {
        builder.overrides(ov);
    }

    builder
}

/// Render `path` for tool output: relative to `working_dir` when it lives
/// under it, otherwise absolute. This matches read_file's habit of echoing
/// user-relative paths. `working_dir` must be canonicalized, and `path` is
/// expected to be canonical too (both hold for search-tool results).
pub(crate) fn relativize(working_dir: &Path, path: &Path) -> String {
    match path.strip_prefix(working_dir) {
        Ok(rel) => rel.display().to_string(),
        Err(_) => path.display().to_string(),
    }
}

/// Trailing line appended when a search exceeds its wall-clock budget.
pub(crate) fn timeout_note(timeout: Duration) -> String {
    format!(
        "Search timed out after {} seconds. Results may be incomplete - try a more specific path or pattern.",
        timeout.as_secs()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relativize_strips_working_dir_prefix() {
        let wd = Path::new("/tmp/oct-project");
        assert_eq!(relativize(wd, Path::new("/tmp/oct-project/src/main.rs")), "src/main.rs");
        assert_eq!(relativize(wd, Path::new("/tmp/oct-project")), "");

        // Paths outside the working directory stay absolute.
        assert_eq!(relativize(wd, Path::new("/etc/hosts")), "/etc/hosts");
    }
}
