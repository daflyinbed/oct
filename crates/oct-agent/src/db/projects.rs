use anyhow::Result;
use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use sqlx::types::Json;
use utoipa::ToSchema;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub working_dir: String,
    /// Additional directories the agent may access via absolute paths,
    /// stored as a JSON array. Relative paths always anchor to
    /// `working_dir`; these roots are absolute-only.
    #[serde(default)]
    #[schema(value_type = Vec<String>)]
    pub additional_dirs: Json<Vec<String>>,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateProjectRequest {
    pub name: String,
    pub working_dir: String,
    /// Additional working directories (absolute paths). Defaults to empty.
    pub additional_dirs: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateProjectRequest {
    pub name: Option<String>,
    pub working_dir: Option<String>,
    /// Additional working directories (absolute paths). `None` leaves the
    /// list unchanged; `Some(vec![])` clears it.
    pub additional_dirs: Option<Vec<String>>,
}

/// Collapse trailing separators ("/a/b/" → "/a/b"); the filesystem root "/"
/// stays as-is so it is not mistaken for an empty entry.
fn strip_trailing_separators(dir: &str) -> &str {
    if dir == "/" {
        dir
    } else {
        dir.trim_end_matches('/')
    }
}

/// Normalize a list of additional directories: trim each entry, drop empty
/// strings, drop duplicates (keeping first occurrence, comparing after
/// trailing-separator normalization so "/a/b/" and "/a/b" are one root) and
/// drop entries equal to the primary working directory. Entries are expected
/// to be absolute — the API layer rejects relative paths before we get here.
/// No existence check is performed — like `working_dir`, directories are
/// validated lazily at use time.
pub(crate) fn normalize_additional_dirs(
    working_dir: &str,
    dirs: impl IntoIterator<Item = impl AsRef<str>>,
) -> Vec<String> {
    let working_dir = strip_trailing_separators(working_dir.trim());
    let mut out: Vec<String> = Vec::new();
    for dir in dirs {
        let dir = strip_trailing_separators(dir.as_ref().trim());
        if dir.is_empty() || dir == working_dir {
            continue;
        }
        if !out.iter().any(|d| d == dir) {
            out.push(dir.to_string());
        }
    }
    out
}

pub async fn list_projects(pool: &SqlitePool) -> Result<Vec<Project>> {
    let rows = sqlx::query_as!(
        Project,
        // The `: _` override decodes the JSON text column straight into the
        // record's Json<Vec<String>> field (the macro's default TEXT mapping
        // does not know Json is compatible).
        "SELECT id, name, working_dir, additional_dirs AS \"additional_dirs: _\", created_at, updated_at FROM projects ORDER BY updated_at DESC"
    )
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

pub async fn get_project(pool: &SqlitePool, id: &str) -> Result<Option<Project>> {
    let row = sqlx::query_as!(
        Project,
        "SELECT id, name, working_dir, additional_dirs AS \"additional_dirs: _\", created_at, updated_at FROM projects WHERE id = $1",
        id,
    )
    .fetch_optional(pool)
    .await?;

    Ok(row)
}

pub async fn create_project(pool: &SqlitePool, req: &CreateProjectRequest) -> Result<Project> {
    let id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().naive_utc();
    let additional_dirs =
        normalize_additional_dirs(&req.working_dir, req.additional_dirs.iter().flatten());
    // The query! macro borrows its arguments later in the statement, so the
    // Json wrapper must outlive the macro invocation.
    let additional_json = Json(additional_dirs.clone());

    sqlx::query!(
        "INSERT INTO projects (id, name, working_dir, additional_dirs, created_at, updated_at) VALUES ($1, $2, $3, $4, $5, $6)",
        id,
        req.name,
        req.working_dir,
        additional_json,
        now,
        now,
    )
    .execute(pool)
    .await?;

    Ok(Project {
        id,
        name: req.name.clone(),
        working_dir: req.working_dir.clone(),
        additional_dirs: Json(additional_dirs),
        created_at: now,
        updated_at: now,
    })
}

pub async fn update_project(
    pool: &SqlitePool,
    id: &str,
    req: &UpdateProjectRequest,
) -> Result<bool> {
    let existing = sqlx::query_as!(
        Project,
        "SELECT id, name, working_dir, additional_dirs AS \"additional_dirs: _\", created_at, updated_at FROM projects WHERE id = $1",
        id,
    )
    .fetch_optional(pool)
    .await?;

    let Some(existing) = existing else {
        return Ok(false);
    };

    let now = chrono::Utc::now().naive_utc();
    let name = req.name.as_deref().unwrap_or(&existing.name);
    let working_dir = req.working_dir.as_deref().unwrap_or(&existing.working_dir);
    let additional_dirs = match &req.additional_dirs {
        Some(dirs) => normalize_additional_dirs(working_dir, dirs),
        // Re-normalize the kept list against the (possibly new) working_dir:
        // a working_dir change must not leave stale entries that now equal
        // the primary directory, and legacy rows keep unnormalized strings.
        None => normalize_additional_dirs(working_dir, &existing.additional_dirs.0),
    };

    let additional_json = Json(additional_dirs);

    let result: sqlx::sqlite::SqliteQueryResult = sqlx::query!(
        "UPDATE projects SET name = $1, working_dir = $2, additional_dirs = $3, updated_at = $4 WHERE id = $5",
        name,
        working_dir,
        additional_json,
        now,
        id,
    )
    .execute(pool)
    .await?;

    Ok(result.rows_affected() > 0)
}

pub async fn delete_project(pool: &SqlitePool, id: &str) -> Result<bool> {
    let result: sqlx::sqlite::SqliteQueryResult =
        sqlx::query!("DELETE FROM projects WHERE id = $1", id)
            .execute(pool)
            .await?;

    Ok(result.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_trims_dedupes_and_drops_working_dir() {
        assert_eq!(
            normalize_additional_dirs(
                "/primary",
                vec![" /a ", "", "/a", "/primary", "  ", "/primary ", "/b"]
            ),
            vec!["/a", "/b"]
        );
        assert_eq!(
            normalize_additional_dirs("/primary", Vec::<String>::new()),
            Vec::<String>::new()
        );
    }

    #[test]
    fn normalize_collapses_trailing_separators() {
        // "/a/b/" and "/a/b" are the same root; the filesystem root survives.
        assert_eq!(
            normalize_additional_dirs("/primary", vec!["/a/b/", "/a/b", "/", "/c//"]),
            vec!["/a/b", "/", "/c"]
        );
        // A working_dir with a trailing separator still suppresses the same
        // additional entry.
        assert_eq!(
            normalize_additional_dirs("/primary/", vec!["/primary", "/x"]),
            vec!["/x"]
        );
    }
}
