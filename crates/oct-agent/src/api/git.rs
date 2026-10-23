//! HTTP DTOs and adapters for project-scoped Git operations.
use super::{AppState, error::ApiResult};
use crate::{application::git::repository, git};
use axum::{
    Json,
    extract::{Path, Query, State},
};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

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

#[utoipa::path(get, path = "/projects/{projectId}/git/status", params(("projectId" = String, Path)), responses((status = 200, body = GitStatus)), tag = "git")]
pub async fn get_status(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<GitStatus>> {
    Ok(Json(
        repository(&state.pool, &id).await?.status().await?.into(),
    ))
}
#[utoipa::path(get, path = "/projects/{projectId}/git/diff", params(("projectId" = String, Path), DiffQuery), responses((status = 200, body = GitDiff)), tag = "git")]
pub async fn get_diff(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<DiffQuery>,
) -> ApiResult<Json<GitDiff>> {
    let request = git::DiffRequest {
        path: query.path,
        staged: query.staged,
        revision: query.revision,
        stash: query.stash,
    };
    Ok(Json(GitDiff {
        patch: repository(&state.pool, &id).await?.diff(request).await?,
    }))
}
#[utoipa::path(get, path = "/projects/{projectId}/git/history", params(("projectId" = String, Path)), responses((status = 200, body = Vec<GitEntry>)), tag = "git")]
pub async fn get_history(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<GitEntry>>> {
    Ok(Json(
        repository(&state.pool, &id)
            .await?
            .history()
            .await?
            .into_iter()
            .map(Into::into)
            .collect(),
    ))
}
#[utoipa::path(get, path = "/projects/{projectId}/git/stashes", params(("projectId" = String, Path)), responses((status = 200, body = Vec<GitEntry>)), tag = "git")]
pub async fn get_stashes(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<GitEntry>>> {
    Ok(Json(
        repository(&state.pool, &id)
            .await?
            .stashes()
            .await?
            .into_iter()
            .map(Into::into)
            .collect(),
    ))
}
#[utoipa::path(post, path = "/projects/{projectId}/git/actions", params(("projectId" = String, Path)), request_body = GitActionRequest, responses((status = 200, body = GitActionResult)), tag = "git")]
pub async fn run_action(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<GitActionRequest>,
) -> ApiResult<Json<GitActionResult>> {
    let request = git::GitActionRequest {
        action: request.action.into(),
        paths: request.paths,
        remote: request.remote,
    };
    Ok(Json(GitActionResult {
        message: repository(&state.pool, &id).await?.action(request).await?,
    }))
}

impl From<git::GitFile> for GitFile {
    fn from(value: git::GitFile) -> Self {
        Self {
            path: value.path,
            original_path: value.original_path,
            index: value.index,
            worktree: value.worktree,
            untracked: value.untracked,
            conflicted: value.conflicted,
            additions: value.additions,
            deletions: value.deletions,
            binary: value.binary,
            staged_additions: value.staged_additions,
            staged_deletions: value.staged_deletions,
            unstaged_additions: value.unstaged_additions,
            unstaged_deletions: value.unstaged_deletions,
        }
    }
}

impl From<git::GitStatus> for GitStatus {
    fn from(value: git::GitStatus) -> Self {
        Self {
            root: value.root,
            branch: value.branch,
            upstream: value.upstream,
            ahead: value.ahead,
            behind: value.behind,
            remotes: value.remotes,
            files: value.files.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<git::GitEntry> for GitEntry {
    fn from(value: git::GitEntry) -> Self {
        Self {
            id: value.id,
            short_id: value.short_id,
            author: value.author,
            date: value.date,
            subject: value.subject,
        }
    }
}

impl From<GitAction> for git::GitAction {
    fn from(value: GitAction) -> Self {
        match value {
            GitAction::Stage => Self::Stage,
            GitAction::Unstage => Self::Unstage,
            GitAction::Restore => Self::Restore,
            GitAction::DiscardTracked => Self::DiscardTracked,
            GitAction::TrashUntracked => Self::TrashUntracked,
            GitAction::StashAll => Self::StashAll,
            GitAction::StashTracked => Self::StashTracked,
            GitAction::StashPop => Self::StashPop,
            GitAction::Fetch => Self::Fetch,
            GitAction::Pull => Self::Pull,
            GitAction::PullRebase => Self::PullRebase,
            GitAction::Push => Self::Push,
            GitAction::ForcePush => Self::ForcePush,
        }
    }
}
