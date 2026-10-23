//! Validated repository mutations; the caller holds the repository lock.
use super::process::output;
use super::queries::{files, has_head};
use super::{GitAction, GitActionRequest};
use crate::error::{AppError, AppResult};
use std::time::Duration;
use tokio::process::Command;

pub(super) async fn action(root: &std::path::Path, request: GitActionRequest) -> AppResult<String> {
    use GitAction::*;
    let current = files(root).await?;
    if let Some(paths) = &request.paths
        && (paths.is_empty() || paths.iter().any(|p| !current.iter().any(|f| &f.path == p)))
    {
        return Err(AppError::BadRequest(
            "Selection is empty or stale; refresh Changes".into(),
        ));
    }
    let selected: Vec<_> = current
        .iter()
        .filter(|f| request.paths.as_ref().is_none_or(|p| p.contains(&f.path)))
        .collect();
    let mut paths = Vec::new();
    for file in &selected {
        if matches!(request.action, Stage) && !file.untracked && file.worktree == " " {
            continue;
        }
        if matches!(request.action, Unstage) && (file.untracked || file.index == " ") {
            continue;
        }
        paths.push(file.path.as_str());
        if !matches!(request.action, Stage)
            && let Some(old) = &file.original_path
        {
            paths.push(old.as_str());
        }
    }
    let mut args = match request.action {
        Stage => vec!["add", "--all"],
        Unstage => {
            if has_head(root).await {
                vec!["restore", "--staged"]
            } else {
                vec!["rm", "--cached", "-r", "--ignore-unmatch"]
            }
        }
        Restore => {
            if !has_head(root).await {
                return Err(AppError::BadRequest(
                    "No commit to restore from; unstage files instead".into(),
                ));
            }
            paths = selected
                .iter()
                .filter(|f| !f.untracked)
                .flat_map(|f| std::iter::once(f.path.as_str()).chain(f.original_path.as_deref()))
                .collect();
            vec!["restore", "--source=HEAD", "--staged", "--worktree"]
        }
        DiscardTracked => {
            paths = selected
                .iter()
                .filter(|f| !f.untracked && f.worktree != " ")
                .map(|f| f.path.as_str())
                .collect();
            vec!["restore", "--worktree"]
        }
        TrashUntracked => {
            // gio implements the desktop trash protocol (including restore metadata),
            // unlike git clean, which irreversibly deletes files.
            for file in selected.iter().filter(|f| f.untracked) {
                let mut command = Command::new("gio");
                command
                    .args(["trash", "--"])
                    .arg(root.join(&file.path))
                    .kill_on_drop(true);
                let result = tokio::time::timeout(Duration::from_secs(30), command.output())
                    .await
                    .map_err(|_| AppError::Conflict("Moving to Trash timed out".into()))?
                    .map_err(|_| {
                        AppError::BadRequest("Desktop Trash requires gio to be installed".into())
                    })?;
                if !result.status.success() {
                    return Err(AppError::Conflict(
                        String::from_utf8_lossy(&result.stderr).into_owned(),
                    ));
                }
            }
            return Ok("Untracked files moved to Trash".into());
        }
        StashAll => vec!["stash", "push", "--include-untracked", "-m", "Oct Changes"],
        StashTracked => vec!["stash", "push", "-m", "Oct Changes"],
        StashPop => vec!["stash", "pop"],
        Fetch | Pull | PullRebase | Push | ForcePush => {
            let remotes = output(root, &["remote"], false).await?;
            if let Some(remote) = &request.remote {
                if !remotes.lines().any(|r| r == remote) || remote.starts_with('-') {
                    return Err(AppError::BadRequest("Unknown remote".into()));
                }
            } else if remotes.is_empty() {
                return Err(AppError::BadRequest("No Git remote configured".into()));
            }
            let mut args = match request.action {
                Fetch => vec!["fetch"],
                Pull => vec!["pull", "--ff-only"],
                PullRebase => vec!["pull", "--rebase"],
                ForcePush => vec!["push", "--force-with-lease"],
                _ => vec!["push"],
            };
            if let Some(remote) = &request.remote {
                args.push(remote);
            }
            return output(root, &args, false).await;
        }
    };
    if matches!(request.action, Stage | Unstage | Restore | DiscardTracked) {
        if paths.is_empty() {
            return Ok("No eligible changes".into());
        }
        args.push("--");
        args.extend(paths);
    }
    output(root, &args, false).await
}
