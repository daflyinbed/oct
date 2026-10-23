//! Git operations are scoped to the project's primary repository. Arguments
//! never pass through a shell, and file operands use literal pathspecs.
mod actions;
mod parse;
mod process;
mod queries;
mod types;

use crate::error::{AppError, AppResult};
use process::output;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use tokio::sync::Mutex;
pub(crate) use types::{DiffRequest, GitAction, GitActionRequest, GitEntry, GitFile, GitStatus};

/// A canonical repository root. Status, diff and mutations use the same
/// root-keyed lock, including requests made through different project ids.
pub(crate) struct Repository {
    root: PathBuf,
}

impl Repository {
    pub(crate) async fn open(working_dir: &Path) -> AppResult<Self> {
        let dir = working_dir
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
                "Set the project working directory to the Git repository root to use Changes"
                    .into(),
            ));
        }
        Ok(Self { root })
    }

    pub(crate) async fn status(&self) -> AppResult<GitStatus> {
        let mutex = lock(&self.root);
        let _guard = mutex.lock().await;
        queries::status(&self.root).await
    }

    pub(crate) async fn diff(&self, request: DiffRequest) -> AppResult<String> {
        let mutex = lock(&self.root);
        let _guard = mutex.lock().await;
        queries::diff(&self.root, request).await
    }

    pub(crate) async fn history(&self) -> AppResult<Vec<GitEntry>> {
        queries::entries(&self.root, false).await
    }

    pub(crate) async fn stashes(&self) -> AppResult<Vec<GitEntry>> {
        queries::entries(&self.root, true).await
    }

    pub(crate) async fn action(&self, request: GitActionRequest) -> AppResult<String> {
        let mutex = lock(&self.root);
        let _guard = mutex.lock().await;
        actions::action(&self.root, request).await
    }
}

fn lock(root: &std::path::Path) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<dashmap::DashMap<PathBuf, Arc<Mutex<()>>>> = OnceLock::new();
    LOCKS
        .get_or_init(Default::default)
        .entry(root.to_owned())
        .or_default()
        .clone()
}
