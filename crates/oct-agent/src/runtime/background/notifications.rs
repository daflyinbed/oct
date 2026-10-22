//! Durable notification delivery and wake scheduling.
use super::*;

impl BackgroundTaskManager {
    /// Kick the notification drainer for a conversation (no-op when one is
    /// already running): it re-reads the pending list from the DB, so the
    /// kick carries no data and a task settling right after a drainer's
    /// last query is still picked up by the next iteration.
    pub fn kick_notification(self: &Arc<Self>, conversation_id: &str) {
        // Election is atomic. Release the election BEFORE rechecking the DB:
        // a concurrent kick either owns the next drainer or is seen below.
        if let dashmap::mapref::entry::Entry::Vacant(entry) =
            self.delivering.entry(conversation_id.to_string())
        {
            entry.insert(());
        } else {
            return;
        }
        let manager = Arc::clone(self);
        let conv = conversation_id.to_string();
        tokio::spawn(async move {
            loop {
                if manager.pool.is_closed() || manager.service_handle().is_none() {
                    break;
                }
                if let Err(e) = manager.deliver_pending(&conv).await {
                    tracing::warn!(conversation_id = %conv, "notification remains pending: {e}");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
                manager.delivering.remove(&conv);
                let pending =
                    match task_db::conversations_with_pending_notifications(&manager.pool).await {
                        Ok(ids) => ids.iter().any(|id| id == &conv),
                        Err(_) => true,
                    };
                if !pending {
                    return;
                }
                match manager.delivering.entry(conv.clone()) {
                    dashmap::mapref::entry::Entry::Vacant(entry) => {
                        entry.insert(());
                    }
                    _ => return,
                }
            }
            manager.delivering.remove(&conv);
        });
    }

    /// Reconcile durable work after startup; weak ownership lets the service
    /// shut down without a task retaining the entire application forever.
    pub fn resume_notifications(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                let Some(manager) = weak.upgrade() else {
                    return;
                };
                if manager.pool.is_closed() || manager.service_handle().is_none() {
                    return;
                }
                match task_db::conversations_with_pending_notifications(&manager.pool).await {
                    Ok(ids) => {
                        for id in ids {
                            manager.kick_notification(&id);
                        }
                        return;
                    }
                    Err(error) => tracing::warn!(%error, "startup wake reconciliation pending"),
                }
                drop(manager);
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });
    }

    /// Scheduling and retries stay with the drainer; conversation preparation
    /// and wake execution are shared application operations.
    async fn deliver_pending(&self, conversation_id: &str) -> anyhow::Result<()> {
        let service = self
            .service_handle()
            .ok_or_else(|| anyhow::anyhow!("run service is not wired"))?;
        crate::application::conversation::ConversationApplication::new(service)
            .wake(crate::application::conversation::WakeCommand {
                conversation_id: conversation_id.to_owned(),
            })
            .await?;
        Ok(())
    }
}
