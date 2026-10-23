//! Durable notification batching and application-owned wake scheduling.
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures_util::FutureExt;
use oct_llm_provider::core::{Message, Role};
use sqlx::SqlitePool;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use super::conversation::{ConversationApplication, WakeCommand};
use super::services::ApplicationServices;
use crate::db::background::{self as task_db, BackgroundTask};
use crate::runtime::background::absolute_output_path;

/// How much of the output file the notification message inlines.
const NOTIFICATION_TAIL_BYTES: u64 = 4096;
const RETRY: Duration = Duration::from_secs(5);

/// Held by the composition root. Drop cancels the receiver and all retry/idle
/// waits, so a background task never retains the application through a callback.
pub struct WakeDrainer {
    cancel: CancellationToken,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for WakeDrainer {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl WakeDrainer {
    pub async fn shutdown(mut self) {
        self.cancel.cancel();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }

    pub fn start(runs: Arc<ApplicationServices>) -> Self {
        let mut receiver = runs.background.take_notifications();
        let pool = runs.pool.clone();
        let application = ConversationApplication::new(runs);
        let cancel = CancellationToken::new();
        let shutdown = cancel.clone();
        let task = tokio::spawn(async move {
            let mut workers = JoinSet::new();
            let work = async {
                // Reconcile durable work after startup. A transient DB error
                // retries here without requiring another process settlement.
                let initial = loop {
                    if pool.is_closed() {
                        return;
                    }
                    match task_db::conversations_with_pending_notifications(&pool).await {
                        Ok(ids) => break ids,
                        Err(error) => tracing::warn!(%error, "startup wake reconciliation pending"),
                    }
                    tokio::time::sleep(RETRY).await;
                };
                let mut active = HashSet::new();
                for id in initial {
                    schedule(&mut workers, &mut active, &application, &pool, id);
                }
                loop {
                    if pool.is_closed() {
                        break;
                    }
                    tokio::select! {
                        address = receiver.recv() => match address {
                            Some(id) => schedule(&mut workers, &mut active, &application, &pool, id),
                            None => break,
                        },
                        result = workers.join_next(), if !workers.is_empty() => {
                            if let Some(Ok(id)) = result {
                                active.remove(&id);
                                // Election is released before the final DB
                                // check. Concurrent kicks remain queued, so a
                                // task settling at this boundary cannot vanish.
                                match task_db::conversations_with_pending_notifications(&pool).await {
                                    Ok(ids) => for id in ids {
                                        schedule(&mut workers, &mut active, &application, &pool, id);
                                    },
                                    Err(error) => {
                                        tracing::warn!(%error, "wake reconciliation pending");
                                        schedule(&mut workers, &mut active, &application, &pool, id);
                                    }
                                }
                            }
                        }
                    }
                }
            };
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => {},
                _ = work => {},
            }
            // Abort and JOIN owned workers before releasing the application.
            // Merely dropping a JoinSet only requests abort asynchronously.
            workers.abort_all();
            while workers.join_next().await.is_some() {}
        });
        Self {
            cancel,
            task: Some(task),
        }
    }
}

fn schedule(
    workers: &mut JoinSet<String>,
    active: &mut HashSet<String>,
    application: &ConversationApplication,
    pool: &SqlitePool,
    id: String,
) {
    if !active.insert(id.clone()) {
        return;
    }
    let application = application.clone();
    let pool = pool.clone();
    workers.spawn(async move {
        let delivery = deliver_pending(&application, &pool, &id);
        if std::panic::AssertUnwindSafe(delivery).catch_unwind().await.is_err() {
            tracing::error!(conversation_id = %id, "notification delivery panicked; reconciliation will retry");
            tokio::time::sleep(RETRY).await;
        }
        id
    });
}

async fn deliver_pending(application: &ConversationApplication, pool: &SqlitePool, id: &str) {
    loop {
        if pool.is_closed() {
            break;
        }
        if let Err(error) = application
            .wake(WakeCommand {
                conversation_id: id.to_owned(),
            })
            .await
        {
            tracing::warn!(conversation_id = %id, %error, "notification remains pending");
            tokio::time::sleep(RETRY).await;
        }
        match task_db::conversations_with_pending_notifications(pool).await {
            Ok(ids) if !ids.iter().any(|pending| pending == id) => break,
            Ok(_) => {}
            Err(_) => tokio::time::sleep(RETRY).await,
        }
    }
}

/// Build the model-visible notification content and its UI details for one
/// batch of settled tasks: one `<task-notification>` block per task, a
/// bounded output tail inline, and the output-file path for the rest.
pub(crate) async fn build_notification_content(
    pending: &[BackgroundTask],
) -> (String, serde_json::Value) {
    let mut blocks = Vec::with_capacity(pending.len());
    let mut details_tasks = Vec::with_capacity(pending.len());
    for task in pending {
        let output_path = absolute_output_path(Path::new(&task.output_path));
        let tail = read_tail(&task.output_path, NOTIFICATION_TAIL_BYTES).await;
        let tail_note = if tail.0 {
            "[earlier output omitted]\n"
        } else {
            ""
        };
        let lost_note = if task.status == "lost" {
            "note: interrupted by a backend restart — the command was terminated; the output below is partial.\n"
        } else {
            ""
        };
        let exit = task
            .exit_code
            .map(|c| c.to_string())
            .unwrap_or_else(|| "unknown (killed by a signal)".to_string());
        blocks.push(format!(
            "<task-notification>\n\
             task_id: {id}\n\
             command: {command}\n\
             status: {status}\n\
             exit_code: {exit}\n\
             {lost_note}\
             output_file: {path}\n\
             Use read_file or grep on output_file for more output.\n\
             output (tail):\n{tail_note}{tail}\n\
             </task-notification>",
            id = task.id,
            command = task.command,
            status = task.status,
            path = output_path.display(),
            tail = tail.1,
        ));
        details_tasks.push(serde_json::json!({
            "task_id": task.id,
            "command": task.command,
            "status": task.status,
            "exit_code": task.exit_code,
            "error": task.error,
            "output_path": output_path,
            "created_at": task.created_at.format("%Y-%m-%dT%H:%M:%S").to_string(),
            "finished_at": task.finished_at.map(|t| t.format("%Y-%m-%dT%H:%M:%S").to_string()),
        }));
    }
    let content = format!(
        "The following background command(s) started earlier have finished — process their results and continue:\n\n{}",
        blocks.join("\n\n")
    );
    let details = serde_json::json!({
        "kind": "background_task_notification",
        "tasks": details_tasks,
    });
    (content, details)
}

/// Read the last `max` bytes of a file as lossy UTF-8. Returns
/// `(was_truncated, text)`.
pub(crate) async fn read_tail(path: &str, max: u64) -> (bool, String) {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let Ok(mut file) = tokio::fs::File::open(path).await else {
        return (false, String::new());
    };
    let Ok(len) = file.metadata().await.map(|m| m.len()) else {
        return (false, String::new());
    };
    let start = len.saturating_sub(max);
    if file.seek(std::io::SeekFrom::Start(start)).await.is_err() {
        return (false, String::new());
    }
    let mut buf = Vec::with_capacity((len - start) as usize);
    // Reading past EOF simply ends early; the file may keep growing while
    // we read (the command can still be flushing).
    let _ = file.take(max).read_to_end(&mut buf).await;
    (start > 0, String::from_utf8_lossy(&buf).into_owned())
}

/// The send/resume catch-up: flush settled-but-unnotified tasks (typically
/// `lost` rows after a restart, or deliveries the drainer could not start a
/// wake run for) as one batched notification message. No wake run — the
/// caller's own run is about to consume the message.
pub async fn catch_up_notifications(
    pool: &SqlitePool,
    conversation_id: &str,
) -> anyhow::Result<usize> {
    let pending = task_db::list_pending_notifications(pool, conversation_id).await?;
    if pending.is_empty() {
        return Ok(0);
    }
    let (content, details) = build_notification_content(&pending).await;
    let message = Message::text(Role::User, &content);
    let task_ids: Vec<String> = pending.iter().map(|t| t.id.clone()).collect();
    task_db::insert_notification_marking_tasks(
        pool,
        conversation_id,
        &message,
        Some(&details),
        &task_ids,
    )
    .await?;
    Ok(pending.len())
}
