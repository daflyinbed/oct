//! Shared admission and deletion tombstones; application use cases own draining.
use crate::error::AppError;
use dashmap::DashSet;
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Clone, Default)]
pub struct Lifecycle {
    pub admission: Arc<RwLock<()>>,
    deleting: Arc<DashSet<String>>,
}
pub(crate) struct Deleting {
    keys: Arc<DashSet<String>>,
    key: String,
}
impl Drop for Deleting {
    fn drop(&mut self) {
        self.keys.remove(&self.key);
    }
}
impl Lifecycle {
    pub(crate) fn begin(&self, key: String) -> Result<Deleting, AppError> {
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
