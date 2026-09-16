//! State shared between the file-manipulating tools within a single agent run.
//!
//! Two concerns are covered:
//! - **Read records**: the mtime of each file at the moment our tools last read
//!   or wrote it. `edit_file` uses this to enforce its read-before-edit rule and
//!   to detect that the file changed on disk since our last read.
//! - **Per-file async mutexes**: serialize read-modify-write cycles on the same
//!   file when the agent loop executes tool calls concurrently.
//!
//! All interior mutability uses `std::sync::Mutex` with very short critical
//! sections; a guard is never held across an `.await`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

/// Outcome of checking a file against our last read/write record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadCheck {
    /// No tool in this run has read or written the file yet.
    NotRead,
    /// The file's current mtime differs from the one recorded at our last
    /// read/write, so our view of it is out of date.
    Stale,
    /// The file is unchanged since our last read/write.
    Fresh,
}

pub struct ToolSharedState {
    /// File mtime recorded when a tool last read or wrote the file.
    read_records: Mutex<HashMap<PathBuf, SystemTime>>,
    /// Lazily created per-file async mutexes.
    file_locks: Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>,
}

impl ToolSharedState {
    pub fn new() -> Self {
        Self {
            read_records: Mutex::new(HashMap::new()),
            file_locks: Mutex::new(HashMap::new()),
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

impl Default for ToolSharedState {
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
        let state = ToolSharedState::new();
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
    fn lock_for_returns_the_same_lock_per_path() {
        let state = ToolSharedState::new();
        let a = Path::new("/tmp/a.txt");
        let b = Path::new("/tmp/b.txt");

        let lock1 = state.lock_for(a);
        let lock2 = state.lock_for(a);
        assert!(Arc::ptr_eq(&lock1, &lock2));

        let lock_b = state.lock_for(b);
        assert!(!Arc::ptr_eq(&lock1, &lock_b));
    }
}
