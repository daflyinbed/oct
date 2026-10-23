//! Repository reads and diff construction.
use super::parse::{parse_stats, parse_status};
use super::process::{OUTPUT_LIMIT, output};
use super::{DiffRequest, GitEntry, GitFile, GitStatus};
use crate::error::{AppError, AppResult};

pub(super) async fn files(root: &std::path::Path) -> AppResult<Vec<GitFile>> {
    parse_status(
        &output(
            root,
            &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
            false,
        )
        .await?,
    )
}
pub(super) async fn has_head(root: &std::path::Path) -> bool {
    output(root, &["rev-parse", "--verify", "HEAD"], false)
        .await
        .is_ok()
}
async fn patch(
    root: &std::path::Path,
    file: &GitFile,
    staged: Option<bool>,
    head: bool,
    stat: bool,
) -> AppResult<String> {
    let format = if stat { "--numstat" } else { "--patch" };
    let nul = if stat { "-z" } else { "--patch" };
    if file.untracked {
        if root.join(&file.path).is_dir() {
            return Ok(if stat {
                "-\t-\t\0".into()
            } else {
                format!("Nested repository: {} (preview unavailable)\n", file.path)
            });
        }
        if staged == Some(true) {
            return Ok(String::new());
        }
        return output(
            root,
            &[
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--no-index",
                format,
                nul,
                "--",
                "/dev/null",
                &file.path,
            ],
            true,
        )
        .await;
    }
    let mut args = vec![
        "diff",
        "--no-ext-diff",
        "--no-textconv",
        "--no-renames",
        format,
        nul,
    ];
    let base = if staged.is_none() {
        if head {
            "HEAD".to_owned()
        } else {
            output(root, &["hash-object", "-t", "tree", "--stdin"], false)
                .await?
                .trim()
                .to_owned()
        }
    } else {
        String::new()
    };
    if staged == Some(true) {
        args.push("--cached");
    } else if staged.is_none() {
        args.push(&base);
    }
    args.extend(["--", &file.path]);
    if let Some(old) = &file.original_path {
        args.push(old);
    }
    output(root, &args, false).await
}
pub(super) async fn status(root: &std::path::Path) -> AppResult<GitStatus> {
    let mut files = files(root).await?;
    let head = has_head(root).await;
    for file in &mut files {
        let stats = patch(root, file, None, head, true).await?;
        (file.additions, file.deletions, file.binary) = parse_stats(&stats);
        if !file.untracked && file.index != " " {
            let stats = patch(root, file, Some(true), head, true).await?;
            (file.staged_additions, file.staged_deletions, _) = parse_stats(&stats);
        }
        if file.worktree != " " {
            let stats = patch(root, file, Some(false), head, true).await?;
            (file.unstaged_additions, file.unstaged_deletions, _) = parse_stats(&stats);
        }
    }
    let branch = output(root, &["symbolic-ref", "--quiet", "--short", "HEAD"], false)
        .await
        .unwrap_or_else(|_| "Detached HEAD".into())
        .trim()
        .to_owned();
    let upstream = output(root, &["rev-parse", "--abbrev-ref", "@{upstream}"], false)
        .await
        .ok()
        .map(|s| s.trim().to_owned());
    let counts = if upstream.is_some() {
        output(
            root,
            &["rev-list", "--left-right", "--count", "HEAD...@{upstream}"],
            false,
        )
        .await?
    } else {
        String::new()
    };
    let mut counts = counts.split_whitespace();
    Ok(GitStatus {
        root: root.display().to_string(),
        branch,
        upstream,
        ahead: counts.next().and_then(|s| s.parse().ok()).unwrap_or(0),
        behind: counts.next().and_then(|s| s.parse().ok()).unwrap_or(0),
        remotes: output(root, &["remote"], false)
            .await?
            .lines()
            .map(str::to_owned)
            .collect(),
        files,
    })
}

pub(super) async fn diff(root: &std::path::Path, query: DiffRequest) -> AppResult<String> {
    let mut result = String::new();
    if let Some(revision) = query.revision {
        if !matches!(revision.len(), 40 | 64) || !revision.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Err(AppError::BadRequest("Invalid revision".into()));
        }
        let args = if query.stash == Some(true) {
            vec![
                "stash",
                "show",
                "--include-untracked",
                "--patch",
                "--no-ext-diff",
                "--no-textconv",
                &revision,
            ]
        } else {
            vec![
                "show",
                "--format=fuller",
                "--no-ext-diff",
                "--no-textconv",
                &revision,
                "--",
            ]
        };
        result = output(root, &args, false).await?;
    } else {
        let files = files(root).await?;
        if let Some(path) = &query.path
            && !files.iter().any(|f| &f.path == path)
        {
            return Err(AppError::NotFound(
                "Changed file not found; refresh Changes".into(),
            ));
        }
        let head = has_head(root).await;
        for file in files
            .iter()
            .filter(|f| query.path.as_ref().is_none_or(|p| p == &f.path))
        {
            result.push_str(&patch(root, file, query.staged, head, false).await?);
            if result.len() as u64 > OUTPUT_LIMIT {
                return Err(AppError::BadRequest(
                    "Diff exceeds 4 MiB; select an individual file".into(),
                ));
            }
        }
    }
    Ok(result)
}
pub(super) async fn entries(root: &std::path::Path, stash: bool) -> AppResult<Vec<GitEntry>> {
    if !stash && !has_head(root).await {
        return Ok(vec![]);
    }
    let args = if stash {
        vec![
            "stash",
            "list",
            "-z",
            "--format=%H%x00%gd%x00%an%x00%aI%x00%s",
        ]
    } else {
        vec!["log", "-100", "-z", "--format=%H%x00%h%x00%an%x00%aI%x00%s"]
    };
    let raw = output(root, &args, false).await?;
    Ok(raw
        .split_terminator('\0')
        .collect::<Vec<_>>()
        .as_chunks::<5>()
        .0
        .iter()
        .map(|p| GitEntry {
            id: p[0].into(),
            short_id: p[1].into(),
            author: p[2].into(),
            date: p[3].into(),
            subject: p[4].into(),
        })
        .collect())
}
