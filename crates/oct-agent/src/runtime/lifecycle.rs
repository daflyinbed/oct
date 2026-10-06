//! Admission and deletion are application operations, shared by every entry point.
use super::{OccupancyGuard, RunService};
use crate::error::AppError;
use dashmap::DashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

#[derive(Default)]
pub struct Lifecycle {
    pub admission: RwLock<()>,
    deleting: Arc<DashSet<String>>,
}
struct Deleting {
    keys: Arc<DashSet<String>>,
    key: String,
}
impl Drop for Deleting {
    fn drop(&mut self) {
        self.keys.remove(&self.key);
    }
}
impl Lifecycle {
    fn begin(&self, key: String) -> Result<Deleting, AppError> {
        if !self.deleting.insert(key.clone()) {
            return Err(AppError::Conflict("Deletion already in progress".into()));
        }
        Ok(Deleting {
            keys: self.deleting.clone(),
            key,
        })
    }
    pub fn check(&self, kind: &str, id: &str) -> Result<(), AppError> {
        if self.deleting.contains(&format!("{kind}:{id}")) {
            return Err(AppError::Conflict("Resource is being deleted".into()));
        }
        Ok(())
    }
}
impl RunService {
    async fn drain_conversation(&self, id: &str) -> Result<OccupancyGuard, AppError> {
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if let Some(handle) = self.sessions.get(id) {
                    handle.cancel.cancel();
                }
                match self.occupy_unchecked(id, tokio_util::sync::CancellationToken::new()) {
                    Ok(guard) => return guard,
                    Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
                }
            }
        })
        .await
        .map_err(|_| AppError::Conflict("Run has not settled; deletion was not performed".into()))
    }
    pub async fn delete_conversation(&self, id: &str) -> Result<bool, AppError> {
        let _deleting = self.lifecycle.begin(format!("conversation:{id}"))?;
        let _guard = self.drain_conversation(id).await?;
        let children = self
            .subagents
            .active_children_of_root(&self.pool, id)
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
        self.background.cancel_tasks_for_conversation(id).await?;
        let deleted = crate::db::conversations::delete_conversation(&self.pool, id).await?;
        if deleted {
            match tokio::fs::remove_dir_all(self.codemode_root().join(id)).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => tracing::warn!("Cannot remove code mode artifacts for {id}: {error}"),
            }
        }
        Ok(deleted)
    }
    pub async fn delete_project(&self, id: &str) -> Result<bool, AppError> {
        // Serialize project deletion against creation of conversations. Runs
        // are rejected by the project tombstone while existing runs drain.
        let _admission = self.lifecycle.admission.write().await;
        let _deleting = self.lifecycle.begin(format!("project:{id}"))?;
        let conversations =
            crate::db::conversations::list_conversations_by_project(&self.pool, id).await?;
        for conv in conversations {
            self.delete_conversation(&conv.id).await?;
        }
        let deleted = crate::db::projects::delete_project(&self.pool, id).await?;
        self.mcp.ensure_connections(&self.pool, Some(id)).await;
        Ok(deleted)
    }
}
