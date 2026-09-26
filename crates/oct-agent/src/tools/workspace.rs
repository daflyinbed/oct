//! Path resolution and workspace classification for the file-manipulating
//! tools.
//!
//! A project has one primary working directory (the anchor for all relative
//! paths) plus, optionally, additional working directories that the agent may
//! access through absolute paths only (mirroring Claude Code's additional
//! working directories). This type is a **resolver + classifier**, not a
//! sandbox:
//!
//! - Relative paths are always joined onto the PRIMARY root; additional roots
//!   are reachable only via absolute paths.
//! - `resolve_path` / `resolve_write_path` canonicalize the request (following
//!   symlinks to their real targets) but do NOT reject paths that land outside
//!   the roots. The `outside` flag on the result is an input for the
//!   permissions layer's decision table and approval display — the permission
//!   gate is the only gate.
//! - [`WorkspaceRoots::validate_path`] keeps the old hard containment check
//!   for the REST files API (`api/files.rs`), a surface that has no permission
//!   gate of its own.

use std::path::{Path, PathBuf};

use super::ToolOutput;

/// The workspace roots shared by all tools of one agent run.
#[derive(Debug, Clone)]
pub struct WorkspaceRoots {
    primary: PathBuf,
    additional: Vec<PathBuf>,
}

/// A resolved tool path plus its workspace classification.
///
/// `outside` says whether the path lands inside ANY workspace root
/// (additional roots included). Tools themselves ignore it — the permissions
/// layer consumes it as a decision-table input.
#[derive(Debug, Clone)]
pub struct ResolvedPath {
    /// The resolved path (canonicalized, symlinks followed).
    pub path: PathBuf,
    /// True when the path is not inside any workspace root.
    pub outside: bool,
}

impl WorkspaceRoots {
    pub fn new(primary: PathBuf, additional: Vec<PathBuf>) -> Self {
        Self {
            primary,
            additional,
        }
    }

    /// The primary working directory; the anchor for relative paths and the
    /// default search root / cwd for tools that take no explicit path.
    pub fn primary(&self) -> &Path {
        &self.primary
    }

    /// Canonicalized roots for classification, primary first. Best effort:
    /// roots that fail to canonicalize (e.g. do not exist yet) are skipped —
    /// a path cannot be inside a root that does not exist, and resolution
    /// must never fail just because a root is missing.
    fn canonical_roots(&self) -> Vec<PathBuf> {
        std::iter::once(&self.primary)
            .chain(&self.additional)
            .filter_map(|root| root.canonicalize().ok())
            .collect()
    }

    /// Canonicalized roots for DISPLAY purposes, best effort: roots that
    /// cannot be canonicalized keep their raw form so relativize still works
    /// for paths built on them. Primary first.
    pub fn display_roots(&self) -> Vec<PathBuf> {
        std::iter::once(self.primary.clone())
            .chain(self.additional.iter().cloned())
            .map(|root| root.canonicalize().unwrap_or(root))
            .collect()
    }

    /// Resolve a request for READ-style use: the path must exist (it is
    /// canonicalized, so symlinks resolve to their real targets). Paths
    /// outside every root resolve fine — classify with [`ResolvedPath`]'s
    /// `outside` flag and let the permissions layer decide.
    ///
    /// Absolute requests are taken as-is; relative requests are joined onto
    /// the primary root.
    pub fn resolve_path(&self, requested: &str) -> Result<ResolvedPath, ToolOutput> {
        let candidate = if Path::new(requested).is_absolute() {
            PathBuf::from(requested)
        } else {
            self.primary.join(requested)
        };

        let path = candidate
            .canonicalize()
            .map_err(|e| ToolOutput::error(format!("Path not found: {requested} ({e})")))?;

        Ok(self.classify(path))
    }

    /// Resolve a request for WRITE-style use. Unlike [`Self::resolve_path`],
    /// this doesn't require the file to exist already: for paths that don't
    /// exist yet, the deepest existing ancestor is resolved via canonicalize
    /// (following symlinks, so a symlinked directory resolves to its real
    /// target — what the user approves on the permission card is the real
    /// destination) and the remaining components are re-appended as-is.
    /// Symlinked tails that cannot resolve (e.g. dangling links) stay
    /// literal, matching plain write(2) semantics.
    ///
    /// Nothing is rejected here — outside-workspace writes and writes through
    /// symlinks are the permission layer's call.
    pub fn resolve_write_path(&self, requested: &str) -> Result<ResolvedPath, ToolOutput> {
        let candidate = if Path::new(requested).is_absolute() {
            PathBuf::from(requested)
        } else {
            self.primary.join(requested)
        };

        let path = if candidate.exists() {
            // Existing target (symlinks followed by canonicalize): the real
            // path, so approval and rule matching see the true destination.
            candidate
                .canonicalize()
                .map_err(|e| ToolOutput::error(format!("Path resolution error: {e}")))?
        } else {
            // For files that don't exist yet, walk up to the deepest existing
            // ancestor and canonicalize it; the non-existent tail is appended
            // back component by component. A tail component that is a
            // symlink to an existing directory makes the ancestor walk stop
            // there (Path::exists follows links), so the canonicalize lands
            // on the link's real target.
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
            for name in rest.iter().rev() {
                resolved.push(name);
            }
            resolved
        };

        Ok(self.classify(path))
    }

    /// Classify a resolved path: true when it is inside NO workspace root
    /// (checked against every root, additional ones included).
    pub fn is_outside(&self, resolved: &Path) -> bool {
        !self
            .canonical_roots()
            .iter()
            .any(|root| resolved.starts_with(root))
    }

    /// Wrap a resolved path with its workspace classification.
    fn classify(&self, path: PathBuf) -> ResolvedPath {
        let outside = self.is_outside(&path);
        ResolvedPath { path, outside }
    }

    /// Hard containment boundary retained for the REST files API
    /// (`api/files.rs` — project file tree and preview), which has no
    /// permission gate. Agent tools use [`Self::resolve_path`] instead and
    /// leave the allow/deny decision to the permissions layer.
    pub fn validate_path(&self, requested: &str) -> Result<PathBuf, ToolOutput> {
        let resolved = self.resolve_path(requested)?;
        if resolved.outside {
            return Err(ToolOutput::error(format!(
                "Access denied: {requested} is outside the working directory"
            )));
        }
        Ok(resolved.path)
    }

    /// Render `path` for tool output: relative to whichever root contains it
    /// (primary first), otherwise absolute. Paths under an additional root
    /// therefore read relative to THAT root, keeping match lines short.
    pub fn relativize(&self, path: &Path) -> String {
        relativize_roots(&self.display_roots(), path)
    }
}

/// Strip `path` against pre-canonicalized `roots` (primary first); falls back
/// to the absolute path when no root contains it. Split out of
/// [`WorkspaceRoots::relativize`] so search tools can canonicalize the roots
/// once per run instead of once per match.
pub(crate) fn relativize_roots(roots: &[PathBuf], path: &Path) -> String {
    for root in roots {
        if let Ok(rel) = path.strip_prefix(root) {
            return rel.display().to_string();
        }
    }
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        primary: tempfile::TempDir,
        additional: tempfile::TempDir,
        outside: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                primary: tempfile::tempdir().expect("primary temp dir"),
                additional: tempfile::tempdir().expect("additional temp dir"),
                outside: tempfile::tempdir().expect("outside temp dir"),
            }
        }

        fn roots(&self) -> WorkspaceRoots {
            WorkspaceRoots::new(
                self.primary.path().to_path_buf(),
                vec![self.additional.path().to_path_buf()],
            )
        }

        fn roots_empty(&self) -> WorkspaceRoots {
            WorkspaceRoots::new(self.primary.path().to_path_buf(), vec![])
        }
    }

    #[test]
    fn read_inside_additional_root_is_allowed() {
        let fx = Fixture::new();
        let file = fx.additional.path().join("notes.txt");
        std::fs::write(&file, "x").unwrap();

        let resolved = fx.roots().resolve_path(&file.display().to_string());
        let resolved = resolved.unwrap();
        assert_eq!(resolved.path, file.canonicalize().unwrap());
        assert!(!resolved.outside);
    }

    #[test]
    fn paths_outside_every_root_resolve_as_outside() {
        let fx = Fixture::new();
        let file = fx.outside.path().join("secret.txt");
        std::fs::write(&file, "x").unwrap();

        // Outside every root resolves fine; the outside flag classifies it.
        let resolved = fx
            .roots()
            .resolve_path(&file.display().to_string())
            .unwrap();
        assert_eq!(resolved.path, file.canonicalize().unwrap());
        assert!(resolved.outside);
        assert!(fx.roots().is_outside(&resolved.path));

        // Relative escape via `..` resolves to the same honest classification.
        let escape = fx
            .primary
            .path()
            .parent()
            .unwrap()
            .join(format!(
                "oct-ws-outside-{}-{}.txt",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
        std::fs::write(&escape, "x").unwrap();
        let rel = format!("../{}", escape.file_name().unwrap().to_string_lossy());
        let resolved = fx.roots().resolve_path(&rel).unwrap();
        assert_eq!(resolved.path, escape.canonicalize().unwrap());
        assert!(resolved.outside);
        let _ = std::fs::remove_file(&escape);
    }

    #[test]
    fn write_creates_missing_file_inside_additional_root() {
        let fx = Fixture::new();
        // The file (and its parent) do not exist yet; the ancestor walk must
        // resolve against the additional root.
        let target = fx.additional.path().join("new-dir/new.txt");
        let resolved = fx
            .roots()
            .resolve_write_path(&target.display().to_string())
            .unwrap();
        assert_eq!(
            resolved.path,
            fx.additional
                .path()
                .canonicalize()
                .unwrap()
                .join("new-dir/new.txt")
        );
        assert!(!resolved.outside);

        // Writing through a non-existent parent is the tool's concern; the
        // resolver only resolves and classifies. A direct child works too:
        let direct = fx.additional.path().join("top.txt");
        let resolved = fx
            .roots()
            .resolve_write_path(&direct.display().to_string())
            .unwrap();
        assert_eq!(
            resolved.path,
            fx.additional.path().canonicalize().unwrap().join("top.txt")
        );
        assert!(!resolved.outside);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_resolves_to_the_real_target() {
        let fx = Fixture::new();
        let link = fx.additional.path().join("escape");
        std::os::unix::fs::symlink(fx.outside.path(), &link).unwrap();

        // Reading through the symlink resolves to the link's real target,
        // honestly classified as outside every root.
        let resolved = fx
            .roots()
            .resolve_path(&link.display().to_string())
            .unwrap();
        assert_eq!(resolved.path, fx.outside.path().canonicalize().unwrap());
        assert!(resolved.outside);

        // Creating a new file THROUGH the symlink: the ancestor walk
        // canonicalizes the symlink to its target outside every root, so the
        // approval surface (and the outside flag) see the real destination.
        let through = fx.additional.path().join("escape/new.txt");
        let resolved = fx
            .roots()
            .resolve_write_path(&through.display().to_string())
            .unwrap();
        assert_eq!(
            resolved.path,
            fx.outside.path().canonicalize().unwrap().join("new.txt")
        );
        assert!(resolved.outside);

        // A dangling symlink in the tail cannot be canonicalized (its target
        // does not exist), so the deepest EXISTING ancestor wins and the
        // dangling component is re-appended literally, like write(2) would
        // see it.
        let dangling = fx.additional.path().join("dang");
        std::os::unix::fs::symlink("no-such-target", &dangling).unwrap();
        let through = fx.additional.path().join("dang/new.txt");
        let resolved = fx
            .roots()
            .resolve_write_path(&through.display().to_string())
            .unwrap();
        assert_eq!(
            resolved.path,
            fx.additional
                .path()
                .canonicalize()
                .unwrap()
                .join("dang/new.txt")
        );
        assert!(!resolved.outside);
    }

    #[test]
    fn rest_files_api_validation_still_rejects_outside() {
        // api/files.rs (no permission gate of its own) keeps the hard
        // containment boundary via validate_path.
        let fx = Fixture::new();
        let file = fx.outside.path().join("secret.txt");
        std::fs::write(&file, "x").unwrap();

        let err = fx
            .roots()
            .validate_path(&file.display().to_string())
            .unwrap_err();
        assert!(err.content.contains("outside the working directory"));
    }

    #[test]
    fn missing_primary_root_does_not_break_resolution() {
        // canonical_roots is best effort: a root that cannot be canonicalized
        // is skipped for classification instead of failing the resolve.
        let existing = tempfile::tempdir().unwrap();
        let file = existing.path().join("f.txt");
        std::fs::write(&file, "x").unwrap();

        let roots = WorkspaceRoots::new(
            PathBuf::from("/definitely-not-an-existing-root-oct"),
            vec![],
        );
        let resolved = roots.resolve_path(&file.display().to_string()).unwrap();
        assert_eq!(resolved.path, file.canonicalize().unwrap());
        assert!(resolved.outside);

        // Relative requests still anchor at the (missing) primary and fail
        // with the read-style existence error.
        let err = roots.resolve_path("f.txt").unwrap_err();
        assert!(err.content.contains("Path not found"));
    }

    #[test]
    fn relativize_prefers_primary_and_falls_back_to_absolute() {
        let fx = Fixture::new();
        let roots = fx.roots();

        let in_primary = fx
            .primary
            .path()
            .canonicalize()
            .unwrap()
            .join("src/main.rs");
        assert_eq!(roots.relativize(&in_primary), "src/main.rs");

        let in_additional = fx
            .additional
            .path()
            .canonicalize()
            .unwrap()
            .join("docs/readme.md");
        assert_eq!(roots.relativize(&in_additional), "docs/readme.md");

        // Outside every root: the canonical absolute path.
        let elsewhere = fx.outside.path().join("x");
        std::fs::write(&elsewhere, "x").unwrap();
        let elsewhere = elsewhere.canonicalize().unwrap();
        assert_eq!(
            roots.relativize(&elsewhere),
            elsewhere.display().to_string()
        );
    }

    #[test]
    fn empty_additional_list_matches_single_root_behavior() {
        let fx = Fixture::new();
        let file = fx.primary.path().join("a.txt");
        std::fs::write(&file, "x").unwrap();

        // Inside the primary: resolved and classified as inside.
        let resolved = fx.roots_empty().resolve_path("a.txt").unwrap();
        assert_eq!(resolved.path, file.canonicalize().unwrap());
        assert!(!resolved.outside);

        // A file that would be inside an additional root is now OUTSIDE the
        // (empty) root set — resolved, flagged, not rejected.
        let extra = fx.additional.path().join("b.txt");
        std::fs::write(&extra, "x").unwrap();
        let resolved = fx
            .roots_empty()
            .resolve_path(&extra.display().to_string())
            .unwrap();
        assert_eq!(resolved.path, extra.canonicalize().unwrap());
        assert!(resolved.outside);

        // Missing path keeps the "Path not found" wording.
        let err = fx.roots_empty().resolve_path("missing.txt").unwrap_err();
        assert!(err.content.contains("Path not found"));
    }
}
