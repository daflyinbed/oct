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

    /// Deliver EVERY pending notification of the conversation as one batched
    /// synthetic user message and start the wake run over it. Takes the
    /// occupancy first (retrying while busy) so the message insert and the
    /// run start are serialized against send/resume/compact exactly like a
    /// user turn.
    async fn deliver_pending(&self, conversation_id: &str) -> anyhow::Result<()> {
        let service = self
            .service_handle()
            .ok_or_else(|| anyhow::anyhow!("run service is not wired"))?;

        // The conversation must still exist (deletion cascades the task
        // rows away — a stale pending list simply yields nothing below).
        let conversation = crate::db::conversations::get_conversation(&self.pool, conversation_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("conversation is gone"))?;
        if conversation.kind != "chat" {
            // Background tasks are gated to root chat runs; defensive.
            anyhow::bail!("not a chat conversation");
        }

        // Wait for the conversation to go idle, then hold the occupancy for
        // the whole insert + run start.
        let cancel = CancellationToken::new();
        let guard = loop {
            match service.occupy_with(conversation_id, cancel.clone()) {
                Ok(guard) => break guard,
                Err(crate::error::AppError::SessionBusy(_)) => {
                    tokio::time::sleep(DELIVERY_IDLE_RETRY).await;
                }
                Err(e) => return Err(e.into()),
            }
        };

        // Re-read under the guard: tasks settled while we were waiting join
        // this batch (the loop's next query would catch them too, but one
        // message per wake keeps the model turns cheap).
        let pending = task_db::list_pending_notifications(&self.pool, conversation_id).await?;
        let previous = task_db::pending_wake(&self.pool, conversation_id).await?;
        if pending.is_empty() && previous.is_none() {
            return Ok(());
        }
        crate::runtime::preparation::ensure_no_pending_recovery(&self.pool, conversation_id)
            .await?;
        service.repair_dangling_tools(conversation_id, &[]).await?;
        let start_message_id = if pending.is_empty() {
            previous.expect("checked above")
        } else {
            let (content, details) = build_notification_content(&pending).await;
            let message = Message::text(Role::User, &content);
            let task_ids: Vec<String> = pending.iter().map(|t| t.id.clone()).collect();
            task_db::insert_notification_marking_tasks(
                &self.pool,
                conversation_id,
                &message,
                Some(&details),
                &task_ids,
            )
            .await?
        };

        // The wake run reuses the conversation's last provider/model — the
        // same rule resume follows. Resolution failures retain the durable
        // wake intent; the drainer retries without duplicating the message.
        let Some((provider_id, model_id)) =
            crate::db::messages::get_last_provider_spec(&self.pool, conversation_id).await?
        else {
            tracing::warn!(
                conversation_id,
                "background notification delivered without a wake run: no provider recorded"
            );
            anyhow::bail!("no provider recorded for pending wake");
        };
        let spec = format!("{provider_id}:{model_id}");
        let (model, provider_id, model_id) = match service.resolve_model(&spec).await {
            Ok(resolved) => resolved,
            Err(e) => {
                tracing::warn!(
                    conversation_id,
                    "background notification delivered without a wake run: {e}"
                );
                return Err(e.into());
            }
        };
        let context_window = service
            .resolve_context_window(model.as_ref(), &provider_id, &model_id)
            .await;

        let project = crate::db::projects::get_project(&self.pool, &conversation.project_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("project is gone"))?;

        let view = crate::db::messages::load_llm_view(&self.pool, conversation_id).await?;

        service.set_start_message_id(conversation_id, &start_message_id);
        // Same pre-run connection sync a user turn performs (best effort).
        service
            .mcp
            .ensure_connections(&self.pool, Some(&project.id))
            .await;

        let run_id = service
            .run_root_with_guard(crate::runtime::RootRunRequest {
                guard,
                conversation_id: conversation_id.to_string(),
                project,
                model,
                provider_id,
                model_id,
                view,
                context_window,
                // The notification itself is the activity; the idle
                // microcompact trigger is meaningless here.
                idle: None,
                attach_task_tool: true,
            })
            .await?;
        // Starting the run is only admission, not successful delivery. Wait
        // for durable settlement so model failures use the drainer's retry
        // delay while the wake intent remains available for the next run.
        loop {
            let Some(run) = crate::db::runs::get_run(&self.pool, &run_id).await? else {
                return Ok(()); // Conversation deletion cascaded the run.
            };
            match run.status.as_str() {
                "running" => tokio::time::sleep(DELIVERY_IDLE_RETRY).await,
                "failed" => anyhow::bail!("wake run {run_id} failed"),
                _ => return Ok(()),
            }
        }
    }
}
