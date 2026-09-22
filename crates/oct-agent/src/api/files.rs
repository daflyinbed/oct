use axum::Json;
use axum::extract::{Path, Query, State};
use serde::{Deserialize, Serialize};
use std::path::{Path as FsPath, PathBuf};
use tokio::io::AsyncReadExt;
use utoipa::IntoParams;
use utoipa::ToSchema;

use super::AppState;
use super::error::{ApiResult, AppError};
use crate::db::projects as db;
use crate::tools::listdir::should_ignore;
use crate::tools::workspace::WorkspaceRoots;

#[derive(Debug, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum FileEntryKind {
    Dir,
    File,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct FileEntry {
    pub name: String,
    /// Path relative to the project working directory (forward slashes), or
    /// the absolute path for entries inside an additional working directory
    pub path: String,
    pub kind: FileEntryKind,
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct ListFilesQuery {
    /// Directory path: relative to the project working directory, or an
    /// absolute path inside one of its additional working directories.
    /// Defaults to the working directory root.
    pub path: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct FileContent {
    /// Path relative to the project working directory (forward slashes), or
    /// the absolute path for files inside an additional working directory
    pub path: String,
    /// UTF-8 text content, truncated to the preview size limit
    pub content: String,
    /// Total file size in bytes
    pub size: u64,
    /// Whether the content was cut off because the file exceeds the size limit
    pub truncated: bool,
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct ReadFileQuery {
    /// File path: relative to the project working directory, or an absolute
    /// path inside one of its additional working directories
    pub path: String,
}

/// Build the project's sandbox roots plus the CANONICALIZED primary root.
/// The canonical form is what validation resolves paths to, so display code
/// must strip it (not the raw configured string) or a symlinked/`..`-laden
/// working_dir would make every entry path degrade to absolute. An invalid
/// primary is a bad request, as in the single-root era. Additional roots are
/// canonicalized lazily by the validation helpers.
fn project_roots(project: &db::Project) -> Result<(WorkspaceRoots, PathBuf), AppError> {
    let primary = PathBuf::from(&project.working_dir);
    let canonical = primary.canonicalize().map_err(|e| {
        AppError::BadRequest(format!(
            "Invalid working directory {}: {e}",
            project.working_dir
        ))
    })?;
    let roots = WorkspaceRoots::new(
        primary,
        project
            .additional_dirs
            .0
            .iter()
            .map(PathBuf::from)
            .collect(),
    );
    Ok((roots, canonical))
}

/// Resolve a requested path inside the canonicalized workspace roots,
/// rejecting anything that escapes them (e.g. via `..`). Relative paths join
/// the primary root (note: `Path::join` REPLACES the base when given an
/// absolute path, hence the explicit branch); absolute paths must land inside
/// any root.
fn resolve_within(roots: &WorkspaceRoots, requested: &str) -> Result<PathBuf, AppError> {
    if requested.is_empty() || requested == "." {
        return Ok(roots.primary().to_path_buf());
    }
    roots
        .validate_path(requested)
        .map_err(|output| AppError::BadRequest(output.content))
}

/// Display path for API consumers: relative to the primary working directory
/// when inside it (forward slashes), otherwise the absolute path. Entries in
/// additional working directories therefore stay addressable by the absolute
/// paths the API expects for them.
fn display_path(primary: &FsPath, absolute: &FsPath) -> String {
    absolute
        .strip_prefix(primary)
        .map(|rel| rel.display().to_string())
        .unwrap_or_else(|_| absolute.display().to_string())
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

    let (roots, primary) = project_roots(&project)?;

    let requested = query.path.as_deref().map(str::trim).unwrap_or("");
    let target = resolve_within(&roots, requested)?;
    if !target.is_dir() {
        return Err(AppError::BadRequest(format!(
            "{} is not a directory",
            if requested.is_empty() { "." } else { requested }
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
            path: display_path(&primary, &entry.path()),
            kind: if is_dir {
                FileEntryKind::Dir
            } else {
                FileEntryKind::File
            },
        });
    }

    entries.sort_by(|a, b| {
        let dir_first = usize::from(b.kind == FileEntryKind::Dir)
            .cmp(&usize::from(a.kind == FileEntryKind::Dir));
        dir_first.then_with(|| a.name.cmp(&b.name))
    });

    Ok(Json(entries))
}

/// Preview responses carry at most this many bytes of content.
const MAX_CONTENT_BYTES: u64 = 1024 * 1024;

#[utoipa::path(
    get,
    path = "/projects/{projectId}/files/content",
    params(
        ("projectId" = String, Path, description = "Project ID"),
        ReadFileQuery
    ),
    responses(
        (status = 200, description = "File text content (truncated at 1 MiB)", body = FileContent),
        (status = 400, description = "Path outside working directory, not a file, binary, or not valid UTF-8"),
        (status = 404, description = "Project not found")
    ),
    tag = "projects"
)]
pub async fn read_project_file(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
    Query(query): Query<ReadFileQuery>,
) -> ApiResult<Json<FileContent>> {
    let project = db::get_project(&state.pool, &project_id)
        .await?
        .ok_or_else(|| AppError::NotFound("Project not found".into()))?;

    let (roots, primary) = project_roots(&project)?;

    let requested = query.path.trim();
    if requested.is_empty() {
        return Err(AppError::BadRequest("Missing file path".into()));
    }
    let target = resolve_within(&roots, requested)?;
    if !target.is_file() {
        return Err(AppError::BadRequest(format!("{requested} is not a file")));
    }

    let file = tokio::fs::File::open(&target)
        .await
        .map_err(anyhow::Error::from)?;
    let size = file.metadata().await.map_err(anyhow::Error::from)?.len();

    // Read limit+1 bytes so truncation is detectable without loading huge files.
    let mut reader = tokio::io::BufReader::new(file).take(MAX_CONTENT_BYTES + 1);
    let mut buf = Vec::new();
    reader
        .read_to_end(&mut buf)
        .await
        .map_err(anyhow::Error::from)?;
    let truncated = buf.len() as u64 > MAX_CONTENT_BYTES;
    if truncated {
        buf.truncate(MAX_CONTENT_BYTES as usize);
    }

    // A NUL byte near the start is a reliable binary signature.
    if buf[..buf.len().min(8192)].contains(&0) {
        return Err(AppError::BadRequest(format!(
            "{requested} is a binary file and cannot be previewed"
        )));
    }

    // Truncation may have split a multi-byte sequence; fall back to the valid
    // prefix instead of rejecting the whole file.
    let content = match String::from_utf8(buf) {
        Ok(content) => content,
        Err(err) if truncated && err.utf8_error().error_len().is_none() => {
            String::from_utf8_lossy(&err.as_bytes()[..err.utf8_error().valid_up_to()]).into_owned()
        }
        Err(_) => {
            return Err(AppError::BadRequest(format!(
                "{requested} is not valid UTF-8 text"
            )));
        }
    };

    Ok(Json(FileContent {
        path: display_path(&primary, &target),
        content,
        size,
        truncated,
    }))
}
