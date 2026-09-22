//! Multi-root workspace sandbox for the file-manipulating tools.
//!
//! A project has one primary working directory (the anchor for all relative
//! paths) plus, optionally, additional working directories that the agent may
//! access through absolute paths only (mirroring Claude Code's additional
//! working directories). Validation semantics:
//!
//! - Relative paths are always joined onto the PRIMARY root; additional roots
//!   are reachable only via absolute paths.
//! - A resolved path is allowed when it lands inside ANY root; roots that do
//!   not exist are skipped for validation (nothing can live inside them).
//! - With an empty additional list every check degrades to the original
//!   single-root behavior, including the error messages.

use std::path::{Path, PathBuf};

use super::ToolOutput;

/// The sandbox roots shared by all tools of one agent run.
#[derive(Debug, Clone)]
pub struct WorkspaceRoots {
    primary: PathBuf,
    additional: Vec<PathBuf>,
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

    /// Canonicalized roots for membership checks, primary first. The primary
    /// root must canonicalize (callers rely on the working-directory error);
    /// additional roots that fail (e.g. do not exist yet) are skipped —
    /// nothing can be inside a root that does not exist.
    fn canonical_roots(&self) -> Result<Vec<PathBuf>, ToolOutput> {
        let primary = self
            .primary
            .canonicalize()
            .map_err(|e| ToolOutput::error(format!("Working directory error: {e}")))?;
        let mut roots = Vec::with_capacity(self.additional.len() + 1);
        roots.push(primary);
        for root in &self.additional {
            if let Ok(canonical) = root.canonicalize() {
                roots.push(canonical);
            }
        }
        Ok(roots)
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

    /// Validate that the resolved path is inside a workspace root.
    ///
    /// Absolute requests are taken as-is; relative requests are joined onto
    /// the primary root. The requested path must exist (it is canonicalized),
    /// which keeps the read-style semantics of the original single-root
    /// validator.
    pub fn validate_path(&self, requested: &str) -> Result<PathBuf, ToolOutput> {
        let candidate = if Path::new(requested).is_absolute() {
            PathBuf::from(requested)
        } else {
            self.primary.join(requested)
        };

        let resolved = candidate
            .canonicalize()
            .map_err(|e| ToolOutput::error(format!("Path not found: {requested} ({e})")))?;

        let roots = self.canonical_roots()?;
        if !roots.iter().any(|root| resolved.starts_with(root)) {
            return Err(ToolOutput::error(format!(
                "Access denied: {requested} is outside the working directory"
            )));
        }

        Ok(resolved)
    }

    /// Validate that the target path WOULD be inside a workspace root.
    /// Unlike [`Self::validate_path`], this doesn't require the file to exist
    /// already: for paths that don't exist yet, the deepest existing ancestor
    /// is resolved via canonicalize (following symlinks) and the remaining
    /// components are re-appended, refusing to traverse any symlink in the
    /// non-existent tail (it could redirect the write outside the sandbox).
    /// The ancestor walk resolves against ANY root, so a brand-new file can
    /// be created inside an additional root as well.
    pub fn validate_write_path(&self, requested: &str) -> Result<PathBuf, ToolOutput> {
        let candidate = if Path::new(requested).is_absolute() {
            PathBuf::from(requested)
        } else {
            self.primary.join(requested)
        };

        let roots = self.canonical_roots()?;

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

        if !roots.iter().any(|root| check_path.starts_with(root)) {
            return Err(ToolOutput::error(format!(
                "Access denied: {requested} is outside the working directory"
            )));
        }

        Ok(check_path)
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

        let resolved = fx.roots().validate_path(&file.display().to_string());
        assert_eq!(resolved.unwrap(), file.canonicalize().unwrap());
    }

    #[test]
    fn paths_outside_every_root_are_denied() {
        let fx = Fixture::new();
        let file = fx.outside.path().join("secret.txt");
        std::fs::write(&file, "x").unwrap();

        let err = fx
            .roots()
            .validate_path(&file.display().to_string())
            .unwrap_err();
        assert!(err.content.contains("outside the working directory"));

        // Relative escape via .. also stays denied.
        let err = fx
            .roots()
            .validate_path("../definitely-not-inside")
            .unwrap_err();
        assert!(err.is_error);
    }

    #[test]
    fn write_creates_missing_file_inside_additional_root() {
        let fx = Fixture::new();
        // The file (and its parent) do not exist yet; the ancestor walk must
        // resolve against the additional root.
        let target = fx.additional.path().join("new-dir/new.txt");
        let resolved = fx
            .roots()
            .validate_write_path(&target.display().to_string())
            .unwrap();
        assert_eq!(
            resolved,
            fx.additional
                .path()
                .canonicalize()
                .unwrap()
                .join("new-dir/new.txt")
        );

        // Writing through a non-existent parent is the tool's concern; the
        // validator only checks containment. A direct child works too:
        let direct = fx.additional.path().join("top.txt");
        assert_eq!(
            fx.roots()
                .validate_write_path(&direct.display().to_string())
                .unwrap(),
            fx.additional.path().canonicalize().unwrap().join("top.txt")
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_through_additional_root_is_denied() {
        let fx = Fixture::new();
        let link = fx.additional.path().join("escape");
        std::os::unix::fs::symlink(fx.outside.path(), &link).unwrap();

        // Reading through the symlink lands outside every root.
        let err = fx
            .roots()
            .validate_path(&link.display().to_string())
            .unwrap_err();
        assert!(err.content.contains("outside the working directory"));

        // Creating a new file THROUGH the symlink: the ancestor walk
        // canonicalizes the symlink to its target outside every root, so
        // containment denies it (denial is what matters; the message names
        // the outside-the-workspace case, not the symlink).
        let through = fx.additional.path().join("escape/new.txt");
        let err = fx
            .roots()
            .validate_write_path(&through.display().to_string())
            .unwrap_err();
        assert!(err.content.contains("outside the working directory"));
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

        // Inside the primary: allowed, same messages as before.
        assert_eq!(
            fx.roots_empty().validate_path("a.txt").unwrap(),
            file.canonicalize().unwrap()
        );

        // A file that would be inside an additional root is now OUTSIDE the
        // (empty) sandbox.
        let extra = fx.additional.path().join("b.txt");
        std::fs::write(&extra, "x").unwrap();
        let err = fx
            .roots_empty()
            .validate_path(&extra.display().to_string())
            .unwrap_err();
        assert!(err.content.contains("outside the working directory"));

        // Missing path keeps the "Path not found" wording.
        let err = fx.roots_empty().validate_path("missing.txt").unwrap_err();
        assert!(err.content.contains("Path not found"));
    }
}
