//! Deletion drains root/child execution and processes before removing storage.
use super::services::ApplicationServices;
use crate::error::AppError;
use crate::runtime::OccupancyGuard;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone)]
pub struct LifecycleApplication {
    runs: Arc<ApplicationServices>,
}
impl LifecycleApplication {
    pub fn new(runs: Arc<ApplicationServices>) -> Self {
        Self { runs }
    }

    async fn drain_conversation(&self, id: &str) -> Result<OccupancyGuard, AppError> {
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if let Some(handle) = self.runs.sessions.get(id) {
                    handle.cancel.cancel();
                }
                match self
                    .runs
                    .occupancy()
                    .occupy_unchecked(id, tokio_util::sync::CancellationToken::new())
                {
                    Ok(guard) => return guard,
                    Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
                }
            }
        })
        .await
        .map_err(|_| AppError::Conflict("Run has not settled; deletion was not performed".into()))
    }
    pub async fn delete_conversation(&self, id: &str) -> Result<bool, AppError> {
        let _deleting = self.runs.lifecycle.begin(format!("conversation:{id}"))?;
        let _guard = self.drain_conversation(id).await?;
        let children = self
            .runs
            .subagents
            .active_children_of_root(&self.runs.pool, id)
            .await?;
        for child in &children {
            child.cancel.cancel();
        }
        for child in &children {
            if !crate::subagent::manager::await_done(child, Duration::from_secs(30)).await {
                return Err(AppError::Conflict(
                    "Child run has not settled; deletion was not performed".into(),
                ));
            }
        }
        self.runs
            .background
            .cancel_tasks_for_conversation(id)
            .await?;
        let deleted = crate::db::conversations::delete_conversation(&self.runs.pool, id).await?;
        if deleted {
            // The conversation's frozen prompt environment dies with it
            // (children are cascade-deleted with the same row; forks never
            // shared the key).
            self.runs.assembly_services().drop_prompt_env(id);
            match tokio::fs::remove_dir_all(self.runs.assembly_services().codemode_root().join(id))
                .await
            {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => tracing::warn!("Cannot remove code mode artifacts for {id}: {error}"),
            }
            // The attachment sweep is reference-driven and GLOBAL (rows have
            // no FK cascade — fork-shared content rows outlive the deleted
            // conversation), so one pass after any deletion reclaims exactly
            // the rows/files nothing references anymore.
            self.runs.attachments.sweep().await;
        }
        Ok(deleted)
    }
    pub async fn delete_project(&self, id: &str) -> Result<bool, AppError> {
        // Serialize project deletion against creation of conversations and
        // terminals. Runs are rejected by the project tombstone while existing
        // runs drain.
        let _admission = self.runs.lifecycle.admission.write().await;
        let _deleting = self.runs.lifecycle.begin(format!("project:{id}"))?;
        let conversations =
            crate::db::conversations::list_conversations_by_project(&self.runs.pool, id).await?;
        for conv in conversations {
            self.delete_conversation(&conv.id).await?;
        }
        let deleted = crate::db::projects::delete_project(&self.runs.pool, id).await?;
        self.runs
            .mcp
            .ensure_connections(&self.runs.pool, Some(id))
            .await;
        Ok(deleted)
    }
}
