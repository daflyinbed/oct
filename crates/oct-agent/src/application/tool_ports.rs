//! Per-run adapters. Tools receive capabilities, never a runtime manager or DB.
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use crate::db::background as task_db;
use crate::runtime::background::{BackgroundTaskManager, BashStart, absolute_output_path};
use crate::tool_contract::ports::{
    BackgroundCommand, BackgroundCommands, BackgroundStarted, BackgroundStop,
};

pub struct RunBackgroundCommands {
    manager: Arc<BackgroundTaskManager>,
    conversation_id: String,
    run_id: String,
}

impl RunBackgroundCommands {
    pub fn new(
        manager: Arc<BackgroundTaskManager>,
        conversation_id: String,
        run_id: String,
    ) -> Self {
        Self {
            manager,
            conversation_id,
            run_id,
        }
    }
}

#[async_trait]
impl BackgroundCommands for RunBackgroundCommands {
    async fn start(&self, command: BackgroundCommand) -> Result<BackgroundStarted> {
        let task = self
            .manager
            .start_bash(BashStart {
                conversation_id: self.conversation_id.clone(),
                parent_run_id: Some(self.run_id.clone()),
                parent_call_id: command.call_id,
                command: command.command,
                cwd: command.cwd,
            })
            .await?;
        Ok(BackgroundStarted {
            task_id: task.id,
            command: task.command,
            working_dir: task.working_dir,
            output_path: absolute_output_path(std::path::Path::new(&task.output_path)),
        })
    }

    async fn stop(&self, task_id: &str) -> Result<BackgroundStop> {
        // Scope check BEFORE any cancel: a task id from another conversation
        // must not be stopped from here.
        let Some(row) = task_db::get_visible_task(self.manager.pool(), task_id)
            .await?
            .filter(|row| row.conversation_id == self.conversation_id)
        else {
            let known = task_db::list_visible_tasks(self.manager.pool(), &self.conversation_id)
                .await
                .unwrap_or_default();
            return Ok(BackgroundStop::Unknown {
                known_ids: known.into_iter().map(|row| row.id).collect(),
            });
        };
        let status = match row.status.as_str() {
            "running" => {
                // Active: cancel + bounded wait; report the settled row. A
                // missing owner while the DB says running is mid-settlement.
                let Some(settled) = self.manager.cancel_and_wait(task_id).await else {
                    return Ok(BackgroundStop::Settling);
                };
                settled.status
            }
            "lost" => {
                // Restart already terminated the process. Persist the intent
                // so the pending notification reports cancellation.
                task_db::cancel_lost_task(self.manager.pool(), task_id).await?;
                "cancelled".into()
            }
            _ => row.status,
        };
        Ok(BackgroundStop::Settled { status })
    }
}
