//! Directory completion for the user-operated project form, before a project exists.
use std::path::PathBuf;

use axum::Json;
use axum::extract::Query;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use super::error::{ApiResult, AppError};

#[derive(Deserialize, IntoParams)]
pub struct DirectoryQuery {
    /// Absolute path or path prefix. Empty and `~` start in the server user's home.
    pub path: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct DirectoryEntry {
    pub name: String,
    pub path: String,
}

#[derive(Serialize, ToSchema)]
pub struct DirectoryListing {
    /// Absolute directory containing the suggestions.
    pub directory: String,
    pub parent: Option<String>,
    /// Incomplete final path component; empty when browsing a directory.
    pub query: String,
    pub entries: Vec<DirectoryEntry>,
}

fn display_path(path: &std::path::Path) -> String {
    let path = path.to_string_lossy().into_owned();
    if cfg!(windows) {
        path.replace('\\', "/")
    } else {
        path
    }
}

fn matches(name: &str, query: &str) -> bool {
    let name = name.to_lowercase();
    let mut chars = name.chars();
    query
        .chars()
        .all(|ch| chars.any(|candidate| candidate == ch))
}

#[utoipa::path(
    get,
    path = "/directories",
    params(DirectoryQuery),
    responses(
        (status = 200, description = "Matching directories on the server", body = DirectoryListing),
        (status = 400, description = "Invalid, missing or unreadable directory")
    ),
    tag = "projects"
)]
pub async fn list_directories(
    Query(params): Query<DirectoryQuery>,
) -> ApiResult<Json<DirectoryListing>> {
    let raw = params.path.as_deref().unwrap_or("");
    let home = || {
        dirs::home_dir().ok_or_else(|| AppError::BadRequest("Home directory is unavailable".into()))
    };
    let path = if raw.is_empty() || raw == "~" {
        home()?
    } else if let Some(rest) = raw.strip_prefix("~/") {
        home()?.join(rest)
    } else {
        PathBuf::from(raw)
    };
    if !path.is_absolute() {
        return Err(AppError::BadRequest("Enter an absolute path or ~/".into()));
    }

    let browsing = raw.is_empty()
        || raw == "~"
        || raw.ends_with('/')
        || (cfg!(windows) && raw.ends_with('\\'))
        || path.parent().is_none();
    let (directory, query) = if browsing {
        (path, String::new())
    } else {
        // Split the raw path: Path::file_name normalizes a final `.` away,
        // but typing `.` here must complete hidden directories in this folder.
        let raw_path = path
            .to_str()
            .ok_or_else(|| AppError::BadRequest("Invalid directory path".into()))?;
        let separator = raw_path
            .rfind(|ch| ch == '/' || (cfg!(windows) && ch == '\\'))
            .ok_or_else(|| AppError::BadRequest("Invalid directory path".into()))?;
        (
            PathBuf::from(&raw_path[..=separator]),
            raw_path[separator + 1..].to_owned(),
        )
    };
    let directory = tokio::fs::canonicalize(directory)
        .await
        .map_err(|e| AppError::BadRequest(format!("Cannot open directory: {e}")))?;
    let mut reader = tokio::fs::read_dir(&directory)
        .await
        .map_err(|e| AppError::BadRequest(format!("Cannot read directory: {e}")))?;
    let needle = query.to_lowercase();
    let mut entries = Vec::new();
    while let Some(entry) = reader
        .next_entry()
        .await
        .map_err(|e| AppError::BadRequest(format!("Cannot read directory: {e}")))?
    {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if !matches(&name, &needle) {
            continue;
        }
        // metadata follows directory symlinks; broken links and files are omitted.
        if !tokio::fs::metadata(entry.path())
            .await
            .is_ok_and(|meta| meta.is_dir())
        {
            continue;
        }
        entries.push(DirectoryEntry {
            name,
            path: display_path(&entry.path()),
        });
    }
    entries.sort_by_cached_key(|entry| {
        let name = entry.name.to_lowercase();
        (!name.starts_with(&needle), name, entry.name.clone())
    });
    Ok(Json(DirectoryListing {
        directory: display_path(&directory),
        parent: directory.parent().map(display_path),
        query,
        entries,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn list(path: String) -> DirectoryListing {
        list_directories(Query(DirectoryQuery { path: Some(path) }))
            .await
            .unwrap()
            .0
    }

    #[tokio::test]
    async fn browse_and_complete_only_directories() {
        let root = tempfile::tempdir().unwrap();
        for name in ["Documents", ".docker", "ZCodeProject", "项目 空格", "empty"] {
            std::fs::create_dir(root.path().join(name)).unwrap();
        }
        std::fs::write(root.path().join("Doc.txt"), "file").unwrap();
        let prefix = display_path(root.path());
        let listing = list(format!("{prefix}/Doc")).await;
        assert_eq!(listing.query, "Doc");
        assert_eq!(
            listing
                .entries
                .iter()
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>(),
            ["Documents", ".docker", "ZCodeProject"]
        );
        assert_eq!(listing.entries[0].path, format!("{prefix}/Documents"));
        let hidden = list(format!("{prefix}/.")).await;
        assert_eq!(hidden.query, ".");
        assert_eq!(hidden.entries.len(), 1);
        assert_eq!(hidden.entries[0].name, ".docker");
        let listing = list(format!("{prefix}/")).await;
        assert!(listing.query.is_empty());
        assert_eq!(listing.entries.len(), 5);
        let listing = list(format!("{prefix}/项目 空格/")).await;
        assert_eq!(listing.parent.as_deref(), Some(prefix.as_str()));
        assert!(listing.entries.is_empty());
        assert!(list(format!("{prefix}/missing")).await.entries.is_empty());
    }

    #[tokio::test]
    async fn invalid_paths_return_errors_and_root_has_no_parent() {
        for path in ["relative/path", "/nonexistent-oct-directory/"] {
            assert!(
                list_directories(Query(DirectoryQuery {
                    path: Some(path.into())
                }))
                .await
                .is_err()
            );
        }
        let root = list("/".into()).await;
        assert_eq!(root.parent, None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn directory_symlinks_are_navigable_but_broken_links_are_omitted() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("target")).unwrap();
        std::os::unix::fs::symlink(root.path().join("target"), root.path().join("link")).unwrap();
        std::os::unix::fs::symlink(root.path().join("missing"), root.path().join("broken"))
            .unwrap();
        let listing = list(format!("{}/", display_path(root.path()))).await;
        assert_eq!(
            listing
                .entries
                .iter()
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>(),
            ["link", "target"]
        );
    }
}
