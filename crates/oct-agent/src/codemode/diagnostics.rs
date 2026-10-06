//! Bounded diagnostic reads; never execute source or reconstruct a VM.
use super::NestedCall;
use crate::tools::ToolOutput;
use serde_json::json;
use std::{
    collections::BTreeMap,
    io::{Read, Seek, SeekFrom},
    path::Path,
};

fn file(dir: &Path, name: &str, limit: u64) -> Option<std::fs::File> {
    let path = dir.join(name);
    let meta = std::fs::symlink_metadata(&path).ok()?;
    if !meta.is_file() || meta.is_symlink() || meta.len() > limit {
        return None;
    }
    std::fs::File::open(path).ok()
}

pub(crate) fn interrupted_cell(dir: &Path, call_id: &str, reason: &str) -> Option<ToolOutput> {
    // Reject symlinks in conversation/run/cells/cell. The configured storage
    // base may itself have a legitimate alias (e.g. macOS /var -> /private/var).
    if !intact_directory(dir) {
        return None;
    }
    let mut trace = String::new();
    file(dir, "calls.jsonl", 8 * 1024 * 1024)?
        .take(8 * 1024 * 1024 + 1)
        .read_to_string(&mut trace)
        .ok()?;
    if trace.len() > 8 * 1024 * 1024 {
        return None;
    }
    let prefix = format!("{call_id}/");
    let mut calls = BTreeMap::new();
    for line in trace.lines() {
        let Ok(mut call) = serde_json::from_str::<NestedCall>(line) else {
            continue;
        };
        let Some(seq) = call
            .id
            .strip_prefix(&prefix)
            .and_then(|n| n.parse::<u64>().ok())
        else {
            continue;
        };
        if !(1..=oct_codemode::MAX_CALLS).contains(&seq)
            || !matches!(
                call.status.as_str(),
                "running" | "completed" | "failed" | "outcome_unknown"
            )
        {
            continue;
        }
        if call.status == "running" {
            call.status = "outcome_unknown".into();
        }
        call.title = call.title.chars().take(2048).collect();
        call.error = call.error.map(|s| s.chars().take(2048).collect());
        calls.insert(seq, call);
    }
    let mut preview = Vec::new();
    let mut bytes = 0;
    if let Some(mut output) = file(dir, "output.log", oct_codemode::MAX_OUTPUT as u64) {
        bytes = output.metadata().ok()?.len();
        let head = bytes.min(16 * 1024);
        (&mut output).take(head).read_to_end(&mut preview).ok()?;
        if bytes > head {
            if bytes > 32 * 1024 {
                preview.extend_from_slice(b"\n[...truncated...]\n");
            }
            output
                .seek(SeekFrom::Start(bytes.saturating_sub(16 * 1024).max(head)))
                .ok()?;
            output.take(16 * 1024).read_to_end(&mut preview).ok()?;
        }
    }
    let completed = calls.values().filter(|c| c.status == "completed").count();
    let unknown = calls
        .values()
        .filter(|c| c.status == "outcome_unknown")
        .count();
    let mut content = format!(
        "Code execution interrupted ({reason}). {completed} calls completed; {unknown} calls have unknown outcomes. Do not retry side effects before inspecting the call ledger.\n"
    );
    content.push_str(&String::from_utf8_lossy(&preview));
    content.push_str(&format!(
        "\noutput_file: {}\ntrace_file: {}\nsource_file: {}",
        dir.join("output.log").display(),
        dir.join("calls.jsonl").display(),
        dir.join("main.ts").display()
    ));
    Some(ToolOutput::error_with_details(
        content,
        json!({"kind":"code_mode","interrupted":true,"error_code":reason,"calls":calls.into_values().collect::<Vec<_>>(),"output_file":dir.join("output.log"),"trace_file":dir.join("calls.jsonl"),"source_file":dir.join("main.ts"),"bytes":bytes,"original_bytes":bytes,"truncated":bytes>32*1024}),
    ))
}

pub(crate) fn source_matches(dir: &Path, code: &str) -> bool {
    if !intact_directory(dir) {
        return false;
    }
    let Some(source) = file(dir, "main.ts", oct_codemode::MAX_CODE as u64) else {
        return false;
    };
    let mut value = String::new();
    source
        .take(oct_codemode::MAX_CODE as u64 + 1)
        .read_to_string(&mut value)
        .is_ok()
        && value == code
}

fn intact_directory(dir: &Path) -> bool {
    dir.is_absolute()
        && dir.ancestors().take(4).all(|p| {
            std::fs::symlink_metadata(p).is_ok_and(|meta| meta.is_dir() && !meta.is_symlink())
        })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[test]
    fn diagnostic_reads_reject_substituted_paths_and_mismatched_source() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("conv/run/cells/cell");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("main.ts"), "text(42)").unwrap();
        std::fs::write(dir.join("calls.jsonl"), "").unwrap();
        assert!(source_matches(&dir, "text(42)"));
        assert!(!source_matches(&dir, "text(43)"));
        assert!(interrupted_cell(&dir, "outer", "backend_restarted").is_some());
        let outside = tmp.path().join("elsewhere");
        std::fs::rename(&dir, &outside).unwrap();
        std::os::unix::fs::symlink(&outside, &dir).unwrap();
        assert!(!source_matches(&dir, "text(42)"));
        assert!(interrupted_cell(&dir, "outer", "backend_restarted").is_none());
    }
}
