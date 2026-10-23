//! Resolve the project's primary repository before executing a Git operation.
use crate::db;
use crate::error::{AppError, AppResult};
use crate::git::Repository;
use sqlx::SqlitePool;
use std::path::Path;

pub(crate) async fn repository(pool: &SqlitePool, project_id: &str) -> AppResult<Repository> {
    let project = db::projects::get_project(pool, project_id)
        .await?
        .ok_or_else(|| AppError::NotFound("Project not found".into()))?;
    Repository::open(Path::new(&project.working_dir)).await
}
