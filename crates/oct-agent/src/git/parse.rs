//! Parsers for NUL-delimited Git output.
use super::GitFile;
use crate::error::{AppError, AppResult};

pub(super) fn parse_status(raw: &str) -> AppResult<Vec<GitFile>> {
    let mut records = raw.split('\0').filter(|s| !s.is_empty());
    let mut files = Vec::new();
    while let Some(record) = records.next() {
        if record.len() < 4 || !record.is_char_boundary(3) {
            return Err(AppError::Internal(anyhow::anyhow!("Invalid Git status")));
        }
        let code = &record[..2];
        let original_path = if code.contains('R') || code.contains('C') {
            records.next().map(str::to_owned)
        } else {
            None
        };
        files.push(GitFile {
            path: record[3..].into(),
            original_path,
            index: code[..1].into(),
            worktree: code[1..].into(),
            untracked: code == "??",
            conflicted: matches!(code, "DD" | "AU" | "UD" | "UA" | "DU" | "AA" | "UU"),
            additions: 0,
            deletions: 0,
            binary: false,
            staged_additions: 0,
            staged_deletions: 0,
            unstaged_additions: 0,
            unstaged_deletions: 0,
        });
    }
    Ok(files)
}
pub(super) fn parse_stats(raw: &str) -> (u64, u64, bool) {
    let mut result = (0, 0, false);
    for record in raw.split('\0').filter(|s| !s.is_empty()) {
        let mut fields = record.splitn(3, '\t');
        let a = fields.next().unwrap_or("0");
        let d = fields.next().unwrap_or("0");
        result.0 += a.parse::<u64>().unwrap_or(0);
        result.1 += d.parse::<u64>().unwrap_or(0);
        result.2 |= a == "-";
    }
    result
}
