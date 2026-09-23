//! Git operations are scoped to the project's primary repository. Arguments
//! never pass through a shell, and file operands use literal pathspecs.
use super::{
    AppState,
    error::{ApiResult, AppError},
};
use axum::{
    Json,
    extract::{Path, Query, State},
};
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    sync::{Arc, OnceLock},
    time::Duration,
};
use tokio::{io::AsyncReadExt, process::Command, sync::Mutex};
use utoipa::{IntoParams, ToSchema};

const OUTPUT_LIMIT: u64 = 4 * 1024 * 1024;

#[derive(Debug, Serialize, ToSchema)]
pub struct GitFile {
    pub path: String,
    pub original_path: Option<String>,
    pub index: String,
    pub worktree: String,
    pub untracked: bool,
    pub conflicted: bool,
    pub additions: u64,
    pub deletions: u64,
    pub binary: bool,
    pub staged_additions: u64,
    pub staged_deletions: u64,
    pub unstaged_additions: u64,
    pub unstaged_deletions: u64,
}
#[derive(Debug, Serialize, ToSchema)]
pub struct GitStatus {
    pub root: String,
    pub branch: String,
    pub upstream: Option<String>,
    pub ahead: u64,
    pub behind: u64,
    pub remotes: Vec<String>,
    pub files: Vec<GitFile>,
}
#[derive(Debug, Serialize, ToSchema)]
pub struct GitEntry {
    pub id: String,
    pub short_id: String,
    pub author: String,
    pub date: String,
    pub subject: String,
}
#[derive(Debug, Serialize, ToSchema)]
pub struct GitDiff {
    pub patch: String,
}
#[derive(Debug, Deserialize, IntoParams)]
pub struct DiffQuery {
    pub path: Option<String>,
    pub staged: Option<bool>,
    pub revision: Option<String>,
    pub stash: Option<bool>,
}
#[derive(Debug, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum GitAction {
    Stage,
    Unstage,
    Restore,
    DiscardTracked,
    TrashUntracked,
    StashAll,
    StashTracked,
    StashPop,
    Fetch,
    Pull,
    PullRebase,
    Push,
    ForcePush,
}
#[derive(Debug, Deserialize, ToSchema)]
pub struct GitActionRequest {
    pub action: GitAction,
    /// None means all eligible files; an empty selection is rejected.
    pub paths: Option<Vec<String>>,
    pub remote: Option<String>,
}
#[derive(Debug, Serialize, ToSchema)]
pub struct GitActionResult {
    pub message: String,
}

async fn read_bounded(reader: impl tokio::io::AsyncRead + Unpin) -> ApiResult<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take(OUTPUT_LIMIT + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(anyhow::Error::from)?;
    if bytes.len() as u64 > OUTPUT_LIMIT {
        return Err(AppError::BadRequest(
            "Git output exceeds 4 MiB; select a smaller diff".into(),
        ));
    }
    Ok(bytes)
}

async fn output(root: &std::path::Path, args: &[&str], allow_diff_exit: bool) -> ApiResult<String> {
    let mut command = Command::new("git");
    command
        .current_dir(root)
        .args([
            "--no-pager",
            "-c",
            "color.ui=false",
            "-c",
            "core.quotePath=false",
        ])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    // stash invokes Git internally with generated pathspecs; literal mode there
    // prevents its cleanup from removing the files it just saved.
    if args.first() != Some(&"stash") {
        command.env("GIT_LITERAL_PATHSPECS", "1");
    }
    let mut child = command.spawn().map_err(anyhow::Error::from)?;
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let result = tokio::time::timeout(Duration::from_secs(120), async {
        let (out, err) = tokio::try_join!(read_bounded(stdout), read_bounded(stderr))?;
        let status = child.wait().await.map_err(anyhow::Error::from)?;
        if !status.success() && !(allow_diff_exit && status.code() == Some(1)) {
            return Err(AppError::Conflict(
                String::from_utf8_lossy(&err).trim().to_owned(),
            ));
        }
        String::from_utf8(out)
            .map_err(|_| AppError::BadRequest("Git output contains non-UTF-8 paths or text".into()))
    })
    .await;
    result.map_err(|_| {
        AppError::Conflict(
            "Git operation timed out; refresh repository state before retrying".into(),
        )
    })?
}
async fn root(state: &AppState, id: &str) -> ApiResult<PathBuf> {
    let project = crate::db::projects::get_project(&state.pool, id)
        .await?
        .ok_or_else(|| AppError::NotFound("Project not found".into()))?;
    let dir = PathBuf::from(project.working_dir)
        .canonicalize()
        .map_err(|e| AppError::BadRequest(e.to_string()))?;
    let root = output(&dir, &["rev-parse", "--show-toplevel"], false)
        .await
        .map_err(|_| {
            AppError::BadRequest("The project working directory is not a Git repository".into())
        })?;
    let root = PathBuf::from(root.trim_end_matches('\n'))
        .canonicalize()
        .map_err(anyhow::Error::from)?;
    // Avoid allowing a project nested in a repository to mutate sibling projects.
    if root != dir {
        return Err(AppError::BadRequest(
            "Set the project working directory to the Git repository root to use Changes".into(),
        ));
    }
    Ok(root)
}
fn lock(root: &std::path::Path) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<dashmap::DashMap<PathBuf, Arc<Mutex<()>>>> = OnceLock::new();
    LOCKS
        .get_or_init(Default::default)
        .entry(root.to_owned())
        .or_default()
        .clone()
}
fn parse_status(raw: &str) -> ApiResult<Vec<GitFile>> {
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
async fn files(root: &std::path::Path) -> ApiResult<Vec<GitFile>> {
    parse_status(
        &output(
            root,
            &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
            false,
        )
        .await?,
    )
}
async fn has_head(root: &std::path::Path) -> bool {
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
) -> ApiResult<String> {
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
fn parse_stats(raw: &str) -> (u64, u64, bool) {
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

async fn status(root: &std::path::Path) -> ApiResult<GitStatus> {
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

#[utoipa::path(get, path = "/projects/{projectId}/git/status", params(("projectId" = String, Path)), responses((status = 200, body = GitStatus)), tag = "git")]
pub async fn get_status(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<GitStatus>> {
    let root = root(&state, &id).await?;
    let mutex = lock(&root);
    let _guard = mutex.lock().await;
    Ok(Json(status(&root).await?))
}
#[utoipa::path(get, path = "/projects/{projectId}/git/diff", params(("projectId" = String, Path), DiffQuery), responses((status = 200, body = GitDiff)), tag = "git")]
pub async fn get_diff(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<DiffQuery>,
) -> ApiResult<Json<GitDiff>> {
    let root = root(&state, &id).await?;
    let mutex = lock(&root);
    let _guard = mutex.lock().await;
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
        result = output(&root, &args, false).await?;
    } else {
        let files = files(&root).await?;
        if let Some(path) = &query.path {
            if !files.iter().any(|f| &f.path == path) {
                return Err(AppError::NotFound(
                    "Changed file not found; refresh Changes".into(),
                ));
            }
        }
        let head = has_head(&root).await;
        for file in files
            .iter()
            .filter(|f| query.path.as_ref().is_none_or(|p| p == &f.path))
        {
            result.push_str(&patch(&root, file, query.staged, head, false).await?);
            if result.len() as u64 > OUTPUT_LIMIT {
                return Err(AppError::BadRequest(
                    "Diff exceeds 4 MiB; select an individual file".into(),
                ));
            }
        }
    }
    Ok(Json(GitDiff { patch: result }))
}
async fn entries(root: &std::path::Path, stash: bool) -> ApiResult<Vec<GitEntry>> {
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
        .chunks_exact(5)
        .map(|p| GitEntry {
            id: p[0].into(),
            short_id: p[1].into(),
            author: p[2].into(),
            date: p[3].into(),
            subject: p[4].into(),
        })
        .collect())
}
#[utoipa::path(get, path = "/projects/{projectId}/git/history", params(("projectId" = String, Path)), responses((status = 200, body = Vec<GitEntry>)), tag = "git")]
pub async fn get_history(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<GitEntry>>> {
    Ok(Json(entries(&root(&state, &id).await?, false).await?))
}
#[utoipa::path(get, path = "/projects/{projectId}/git/stashes", params(("projectId" = String, Path)), responses((status = 200, body = Vec<GitEntry>)), tag = "git")]
pub async fn get_stashes(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<GitEntry>>> {
    Ok(Json(entries(&root(&state, &id).await?, true).await?))
}
#[utoipa::path(post, path = "/projects/{projectId}/git/actions", params(("projectId" = String, Path)), request_body = GitActionRequest, responses((status = 200, body = GitActionResult)), tag = "git")]
pub async fn run_action(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<GitActionRequest>,
) -> ApiResult<Json<GitActionResult>> {
    let root = root(&state, &id).await?;
    let mutex = lock(&root);
    let _guard = mutex.lock().await;
    Ok(Json(GitActionResult {
        message: action(&root, request).await?,
    }))
}
async fn action(root: &std::path::Path, request: GitActionRequest) -> ApiResult<String> {
    use GitAction::*;
    let current = files(root).await?;
    if let Some(paths) = &request.paths {
        if paths.is_empty() || paths.iter().any(|p| !current.iter().any(|f| &f.path == p)) {
            return Err(AppError::BadRequest(
                "Selection is empty or stale; refresh Changes".into(),
            ));
        }
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
        if !matches!(request.action, Stage) {
            if let Some(old) = &file.original_path {
                paths.push(old.as_str());
            }
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
