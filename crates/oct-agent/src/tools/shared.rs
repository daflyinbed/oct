//! State shared between the file-manipulating tools — split by ownership
//! scope (ADR 0002 §5.3):
//!
//! - [`WorkspaceFileLocks`]: per-file async mutexes serializing
//!   read-modify-write cycles on the same path. Shared across EVERY agent of
//!   a project's task tree (root + subagents): a run service hands the same
//!   `Arc` to parent and children, so cross-agent writes to one file cannot
//!   interleave.
//! - [`AgentReadState`]: the mtime of each file at the moment THIS agent last
//!   read or wrote it. One per agent: A having read a file never authorizes
//!   B's edit, and A's write never refreshes B's record — `edit_file` uses it
//!   for the read-before-edit rule and the stale-on-external-change check.
//!
//! Both use `std::sync::Mutex` with very short critical sections; a guard is
//! never held across an `.await`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

/// Outcome of checking a file against our last read/write record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadCheck {
    /// No tool of this agent has read or written the file yet.
    NotRead,
    /// The file's current mtime differs from the one recorded at our last
    /// read/write, so our view of it is out of date.
    Stale,
    /// The file is unchanged since our last read/write.
    Fresh,
}

/// Per-agent read/write records (ADR 0002 §5.3): one instance per agent —
/// the parent and every subagent keep their own, so an edit by one never
/// counts as a read by another.
pub struct AgentReadState {
    /// File mtime recorded when one of this agent's tools last read or wrote
    /// the file.
    read_records: Mutex<HashMap<PathBuf, SystemTime>>,
}

impl AgentReadState {
    pub fn new() -> Self {
        Self {
            read_records: Mutex::new(HashMap::new()),
        }
    }

    /// Record that we read `path` when its mtime was `mtime`.
    pub fn record_read(&self, path: &Path, mtime: SystemTime) {
        self.record(path, mtime);
    }

    /// Record that we wrote `path` and its mtime became `mtime`.
    pub fn record_write(&self, path: &Path, mtime: SystemTime) {
        self.record(path, mtime);
    }

    fn record(&self, path: &Path, mtime: SystemTime) {
        self.read_records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(path.to_path_buf(), mtime);
    }

    /// Compare the file's current mtime against our last read/write record.
    ///
    /// Best-effort: if the current mtime cannot be read (e.g. the file just
    /// vanished), report `Fresh` so a transient stat failure does not
    /// deadlock the model.
    pub fn check_read(&self, path: &Path) -> ReadCheck {
        let recorded = self
            .read_records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(path)
            .copied();

        let Some(recorded) = recorded else {
            return ReadCheck::NotRead;
        };

        match file_mtime(path) {
            Some(current) if current != recorded => ReadCheck::Stale,
            _ => ReadCheck::Fresh,
        }
    }
}

impl Default for AgentReadState {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-path async mutexes shared across the whole task tree (root run plus
/// its subagents): serialize read-modify-write cycles on the same file when
/// tool calls execute concurrently — including calls issued by different
/// agents of the same project.
pub struct WorkspaceFileLocks {
    /// Lazily created per-file async mutexes.
    file_locks: Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>,
}

impl WorkspaceFileLocks {
    pub fn new() -> Self {
        Self {
            file_locks: Mutex::new(HashMap::new()),
        }
    }

    /// Get (creating and caching on first use) the async mutex for `path`,
    /// used to serialize read-modify-write cycles on the same file.
    pub fn lock_for(&self, path: &Path) -> Arc<tokio::sync::Mutex<()>> {
        self.file_locks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(path.to_path_buf())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }
}

impl Default for WorkspaceFileLocks {
    fn default() -> Self {
        Self::new()
    }
}

/// Best-effort current mtime of a file (`None` if it cannot be stat'ed).
pub(crate) fn file_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_read_reports_not_read_stale_fresh() {
        let state = AgentReadState::new();
        let dir = std::env::temp_dir().join(format!(
            "oct-shared-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("f.txt");
        std::fs::write(&file, "hello").unwrap();

        // No record yet.
        assert_eq!(state.check_read(&file), ReadCheck::NotRead);

        // Record the current mtime: now fresh.
        let mtime = file_mtime(&file).unwrap();
        state.record_read(&file, mtime);
        assert_eq!(state.check_read(&file), ReadCheck::Fresh);

        // A different recorded mtime means the file changed: stale.
        state.record_write(&file, mtime + std::time::Duration::from_secs(1));
        assert_eq!(state.check_read(&file), ReadCheck::Stale);

        // A path we never touched (and that does not exist) is simply not read.
        assert_eq!(
            state.check_read(&dir.join("missing.txt")),
            ReadCheck::NotRead
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn read_state_is_per_agent() {
        // A's records must not authorize or refresh B's view: two separate
        // AgentReadState instances never see each other's reads (the split
        // ToolSharedState guarantees in ADR 0002 §5.3). A REAL file is needed:
        // a stat failure degrades to Fresh by design, so a path that never
        // exists cannot exercise the Stale branch.
        let dir = std::env::temp_dir().join(format!(
            "oct-shared-split-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("f.txt");
        std::fs::write(&file, "hello").unwrap();

        let a = AgentReadState::new();
        let b = AgentReadState::new();
        let mtime = file_mtime(&file).unwrap();

        a.record_read(&file, mtime);
        assert_eq!(a.check_read(&file), ReadCheck::Fresh);
        assert_eq!(b.check_read(&file), ReadCheck::NotRead);

        // A's write refreshes only A's record; B still has no record of its
        // own (B must re-read before editing).
        a.record_write(&file, mtime + std::time::Duration::from_secs(1));
        assert_eq!(a.check_read(&file), ReadCheck::Stale);
        assert_eq!(b.check_read(&file), ReadCheck::NotRead);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn lock_for_returns_the_same_lock_per_path() {
        let locks = WorkspaceFileLocks::new();
        let a = Path::new("/tmp/a.txt");
        let b = Path::new("/tmp/b.txt");

        let lock1 = locks.lock_for(a);
        let lock2 = locks.lock_for(a);
        assert!(Arc::ptr_eq(&lock1, &lock2));

        let lock_b = locks.lock_for(b);
        assert!(!Arc::ptr_eq(&lock1, &lock_b));
    }
}
