//! Wire logs are disposable diagnostics with a global disk budget.
use std::path::{Path, PathBuf};
const TOTAL: u64 = 256 * 1024 * 1024;
pub(super) const FILE: u64 = 16 * 1024 * 1024;
fn entries(dir: &Path) -> Vec<(PathBuf, std::fs::Metadata)> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.ends_with(".jsonl") || name.ends_with(".jsonl.1")
        })
        .filter_map(|e| {
            e.metadata()
                .ok()
                .filter(|m| m.is_file())
                .map(|m| (e.path(), m))
        })
        .collect()
}
pub(super) fn size(dir: &Path) -> u64 {
    entries(dir).iter().map(|(_, m)| m.len()).sum()
}
pub(super) fn over_budget(total: u64, next: u64) -> bool {
    total.saturating_add(next) > TOTAL
}
pub(super) fn prune(dir: &Path) {
    let mut files = entries(dir);
    files.sort_by_key(|(_, m)| m.modified().ok());
    let mut total: u64 = files.iter().map(|(_, m)| m.len()).sum();
    for (path, metadata) in files {
        if total <= TOTAL / 2 {
            break;
        }
        if std::fs::remove_file(path).is_ok() {
            total = total.saturating_sub(metadata.len());
        }
    }
}
