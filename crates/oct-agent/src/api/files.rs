use axum::extract::{Path, Query, State};
use axum::Json;
use serde::{Deserialize, Serialize};
use std::path::{Path as FsPath, PathBuf};
use utoipa::IntoParams;
use utoipa::ToSchema;

use super::AppState;
use super::error::{AppError, ApiResult};
use crate::db::projects as db;
use crate::tools::listdir::should_ignore;

#[derive(Debug, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum FileEntryKind {
    Dir,
    File,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct FileEntry {
    pub name: String,
    /// Path relative to the project working directory (forward slashes)
    pub path: String,
    pub kind: FileEntryKind,
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct ListFilesQuery {
    /// Directory path relative to the project working directory. Defaults to the root.
    pub path: Option<String>,
}

/// Resolve a requested relative path inside the canonicalized working directory,
/// rejecting anything that escapes it (e.g. via `..`).
fn resolve_within(working_resolved: &FsPath, requested: &str) -> Result<PathBuf, AppError> {
    if requested.is_empty() || requested == "." {
        return Ok(working_resolved.to_path_buf());
    }

    let candidate = working_resolved.join(requested);
    let resolved = candidate
        .canonicalize()
        .map_err(|e| AppError::BadRequest(format!("Path not found: {requested} ({e})")))?;

    if !resolved.starts_with(working_resolved) {
        return Err(AppError::BadRequest(format!(
            "Access denied: {requested} is outside the working directory"
        )));
    }

    Ok(resolved)
}

fn relative_path(working_resolved: &FsPath, absolute: &FsPath) -> String {
    absolute
        .strip_prefix(working_resolved)
        .unwrap_or(absolute)
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/")
}

#[utoipa::path(
    get,
    path = "/projects/{projectId}/files",
    params(
        ("projectId" = String, Path, description = "Project ID"),
        ListFilesQuery
    ),
    responses(
        (status = 200, description = "Directory entries (directories first, then alphabetical)", body = Vec<FileEntry>),
        (status = 400, description = "Path outside working directory or not a directory"),
        (status = 404, description = "Project not found")
    ),
    tag = "projects"
)]
pub async fn list_project_files(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
    Query(query): Query<ListFilesQuery>,
) -> ApiResult<Json<Vec<FileEntry>>> {
    let project = db::get_project(&state.pool, &project_id)
        .await?
        .ok_or_else(|| AppError::NotFound("Project not found".into()))?;

    let working_resolved = PathBuf::from(&project.working_dir)
        .canonicalize()
        .map_err(|e| {
            AppError::BadRequest(format!(
                "Invalid working directory {}: {e}",
                project.working_dir
            ))
        })?;

    let requested = query.path.as_deref().map(str::trim).unwrap_or("");
    let target = resolve_within(&working_resolved, requested)?;
    if !target.is_dir() {
        return Err(AppError::BadRequest(format!(
            "{} is not a directory",
            if requested.is_empty() {
                "."
            } else {
                requested
            }
        )));
    }

    let mut reader = tokio::fs::read_dir(&target)
        .await
        .map_err(anyhow::Error::from)?;
    let mut entries: Vec<FileEntry> = Vec::new();

    while let Some(entry) = reader.next_entry().await.map_err(anyhow::Error::from)? {
        let name = entry.file_name().to_string_lossy().to_string();
        if should_ignore(&name) {
            continue;
        }
        // file_type() does not follow symlinks, so a symlink to a directory
        // is reported as a file and can never be expanded past the sandbox.
        let is_dir = entry
            .file_type()
            .await
            .map_err(anyhow::Error::from)?
            .is_dir();
        entries.push(FileEntry {
            name,
            path: relative_path(&working_resolved, &entry.path()),
            kind: if is_dir {
                FileEntryKind::Dir
            } else {
                FileEntryKind::File
            },
        });
    }

    entries.sort_by(|a, b| {
        let dir_first =
            usize::from(b.kind == FileEntryKind::Dir).cmp(&usize::from(a.kind == FileEntryKind::Dir));
        dir_first.then_with(|| a.name.cmp(&b.name))
    });

    Ok(Json(entries))
}
