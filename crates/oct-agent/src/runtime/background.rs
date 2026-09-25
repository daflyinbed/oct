//! Background bash task runtime (ADR 0003): the manager `execute_command`
//! hands detached commands to when the model passes `run_in_background`.
//!
//! The shape deliberately mirrors the subagent runtime (ADR 0002):
//!
//! * **Envelope-immediate**: the tool call is ANSWERED on the spot with the
//!   task id, so the agent loop's history stays legal (no dangling calls,
//!   no interaction with the repair/recovery machinery).
//! * **Registry in memory, truth in the DB**: only ACTIVE tasks live in the
//!   `DashMap`; the `background_tasks` row (created before the supervisor
//!   is spawned) is what queries, the restart sweep and the notification
//!   catch-up read.
//! * **Settlement is one-shot**: the supervisor's terminal write is guarded
//!   by `status = 'running'`; it then closes the done watch, drops the
//!   registry entry and kicks the notification drainer.
//! * **Wake, don't poll**: the drainer waits for the conversation to become
//!   idle, inserts ONE synthetic user message carrying every pending
//!   `<task-notification>` (message + `notified` flag in a single
//!   transaction, so a delivery can never repeat or get lost across a
//!   crash) and starts a fresh root run over it. A send/resume catch-up
//!   flush covers anything the drainer could not deliver.
//! * **Die with the backend**: background children keep the same spawn
//!   hygiene as the foreground `execute` tool (own process group, Linux
//!   PDEATHSIG) but are NOT owned by the tool future — no `kill_on_drop`,
//!   no group-kill-on-drop guard. A backend exit therefore SIGTERMs them
//!   (they are marked `lost` on the next bootstrap), while a run's
//!   cancellation does NOT touch them: the envelope already promised the
//!   model a notification, and the task's own token (task_stop /
//!   conversation deletion) is the only thing that kills it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use dashmap::DashMap;
use sqlx::SqlitePool;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use oct_llm_provider::core::{Message, Role};

use crate::db::background as task_db;
use crate::db::background::{BackgroundTask, TaskFinalStatus};

/// Default hard cap on one task's output FILE (checked by the size
/// watchdog; the file is also the only output store, so this bounds disk
/// usage).
const DEFAULT_OUTPUT_LIMIT_BYTES: u64 = 1024 * 1024 * 1024; // 1 GiB
/// How often the watchdog stats the output file.
const WATCHDOG_INTERVAL: Duration = Duration::from_secs(5);
/// Wall-clock cap on one background task (the anti-`yes`-forever backstop;
/// the output cap is the primary bound).
const MAX_RUNTIME: Duration = Duration::from_secs(24 * 60 * 60);
/// Grace between the SIGTERM and the SIGKILL of a deliberate group kill.
const TERM_GRACE: Duration = Duration::from_millis(1500);
/// How long to keep reaping after the SIGKILL before giving up.
const REAP_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a cancel waits for the supervisor's settlement before
/// returning anyway (the row settles regardless, the caller just sees it
/// still `running`).
const CANCEL_SETTLE_WAIT: Duration = Duration::from_secs(10);
/// How much of the output file the notification message inlines.
pub(crate) const NOTIFICATION_TAIL_BYTES: u64 = 4096;
/// Poll period while the drainer waits for the conversation to go idle.
const DELIVERY_IDLE_RETRY: Duration = Duration::from_secs(1);

/// One ACTIVE background task, as the in-memory registry holds it. Dropped
/// from the map the moment the supervisor settles.
pub struct ActiveTask {
    pub conversation_id: String,
    pub command: String,
    pub output_path: PathBuf,
    /// The task's OWN token — deliberately independent of any run's token:
    /// cancelling a run must not kill tasks its envelope already promised
    /// to report. task_stop / conversation deletion cancel this one.
    pub cancel: CancellationToken,
    /// Flips to true when the supervisor has settled the DB row.
    pub done: watch::Receiver<bool>,
}

/// Everything `execute_command` needs to hand one command to the manager.
pub struct BashStart {
    pub conversation_id: String,
    /// The root run whose tool call is starting the task (provenance on the
    /// row; informational).
    pub parent_run_id: Option<String>,
    pub parent_call_id: String,
    pub command: String,
    /// Validated (workspace-scoped) working directory.
    pub cwd: PathBuf,
}

/// The background task manager: in-memory active registry + notification
/// delivery. Two-phase wired into the [`crate::runtime::RunService`] exactly
/// like the SubagentManager (the service owns the manager, the manager
/// reaches back through a `OnceLock` to start wake runs).
pub struct BackgroundTaskManager {
    /// Output-file cap enforced by the size watchdog; interior mutability
    /// so the test harness can shrink it through the shared `Arc` (the
    /// production value is the constant).
    output_limit_bytes: std::sync::atomic::AtomicU64,
    pool: SqlitePool,
    /// Root directory for task output files: `{exec_dir}/{conversation_id}/
    /// {task_id}.log`. Deliberately OUTSIDE any project workspace — the
    /// sandboxed `read` tool must not reach it; the model reads output
    /// through `task_output` instead.
    exec_dir: PathBuf,
    service: OnceLock<Arc<crate::runtime::RunService>>,
    tasks: DashMap<String, ActiveTask>,
    /// Conversations with a live notification drainer (the kick-once
    /// marker): prevents stacking drainers for one conversation.
    delivering: DashMap<String, ()>,
}

/// Open (create) the task's output file for appending: O_APPEND keeps the
/// two stdio fds (stdout/stderr) interleaving without tearing, 0o600
/// because command output can carry secrets the command printed.
#[cfg(unix)]
fn open_output_file(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn open_output_file(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .truncate(false)
        .open(path)
}

impl BackgroundTaskManager {
    pub fn new(pool: SqlitePool, exec_dir: PathBuf) -> Self {
        Self {
            output_limit_bytes: std::sync::atomic::AtomicU64::new(DEFAULT_OUTPUT_LIMIT_BYTES),
            pool,
            exec_dir,
            service: OnceLock::new(),
            tasks: DashMap::new(),
            delivering: DashMap::new(),
        }
    }

    /// Two-phase wire (see the type comment); called right after the
    /// RunService is constructed.
    pub fn set_service(&self, service: Arc<crate::runtime::RunService>) {
        let _ = self.service.set(service);
    }

    fn service_handle(&self) -> Option<Arc<crate::runtime::RunService>> {
        self.service.get().cloned()
    }

    /// The pool the task rows live in (shared with the tools and the API).
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// The output-file cap the size watchdog enforces.
    pub fn output_limit(&self) -> u64 {
        self.output_limit_bytes
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Override the output-file cap (test harness hook).
    pub fn set_output_limit(&self, limit: u64) {
        self.output_limit_bytes
            .store(limit, std::sync::atomic::Ordering::Relaxed);
    }

    /// The registry lookup the tools and the API share: an active task's
    /// live handles (done watch, output path), when it is still running.
    pub fn active_task(&self, task_id: &str) -> Option<(watch::Receiver<bool>, PathBuf)> {
        self.tasks
            .get(task_id)
            .map(|e| (e.value().done.clone(), e.value().output_path.clone()))
    }

    /// Start one detached command: create the output file, spawn the child
    /// (own process group + PDEATHSIG, stdout/stderr wired DIRECTLY to the
    /// file — output never passes through the backend), persist the row,
    /// register the active task and hand it to the supervisor. Returns the
    /// persisted row for the tool's envelope.
    pub async fn start_bash(self: &Arc<Self>, req: BashStart) -> anyhow::Result<BackgroundTask> {
        let task_dir = self.exec_dir.join(&req.conversation_id);
        tokio::fs::create_dir_all(&task_dir).await?;

        let id = format!("bg_{}", uuid::Uuid::new_v4());
        let output_path = task_dir.join(format!("{id}.log"));

        let file = open_output_file(&output_path)?;
        let stderr_file = file.try_clone()?;

        let mut command = tokio::process::Command::new("sh");
        command
            .arg("-c")
            .arg(&req.command)
            .current_dir(&req.cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::from(file))
            .stderr(std::process::Stdio::from(stderr_file));
        // Same group semantics as the foreground path: the child leads its
        // own group so a deliberate kill takes down the whole tree.
        #[cfg(unix)]
        command.process_group(0);
        // Same crash story as the foreground path: if the backend dies
        // outright, the kernel SIGTERMs the child instead of orphaning it.
        // Unlike the foreground path there is NO kill_on_drop and no
        // group-kill-on-drop guard: this child must outlive the tool call
        // (and the run); its lifetime belongs to the registry below.
        #[cfg(all(unix, target_os = "linux"))]
        // SAFETY: same pre_exec contract as the execute tool (async-signal-
        // safe calls only).
        unsafe {
            command.pre_exec(crate::tools::execute::set_pdeathsig);
        }

        let mut child = command.spawn()?;
        let pgid = child.id();

        let task = match task_db::create_task(
            &self.pool,
            task_db::NewBackgroundTask {
                id: id.clone(),
                conversation_id: req.conversation_id.clone(),
                parent_run_id: req.parent_run_id,
                parent_call_id: req.parent_call_id,
                command: req.command.clone(),
                working_dir: req.cwd.to_str().map(str::to_string),
                output_path: output_path.display().to_string(),
            },
        )
        .await
        {
            Ok(task) => task,
            Err(e) => {
                // The row is the task's durable identity; without it the
                // registry entry would be orphaned state — take the child
                // back down and fail the tool call.
                #[cfg(unix)]
                if let Some(pgid) = pgid {
                    unsafe { libc::killpg(pgid as libc::pid_t, libc::SIGKILL) };
                }
                let _ = child.start_kill();
                let _ = tokio::time::timeout(REAP_TIMEOUT, child.wait()).await;
                return Err(e);
            }
        };

        let (done_tx, done_rx) = watch::channel(false);
        let cancel = CancellationToken::new();
        self.tasks.insert(
            task.id.clone(),
            ActiveTask {
                conversation_id: req.conversation_id.clone(),
                command: req.command,
                output_path: output_path.clone(),
                cancel,
                done: done_rx,
            },
        );

        let manager = Arc::clone(self);
        let conversation_id = req.conversation_id;
        tokio::spawn(async move {
            supervise_task(
                manager,
                id,
                conversation_id,
                child,
                pgid,
                output_path,
                done_tx,
            )
            .await;
        });
        Ok(task)
    }

    /// Cancel one active task and wait (bounded) for its settlement, then
    /// return the settled row. `None` when the id is not an active task —
    /// the caller reads the DB row itself (terminal, or `lost` for the
    /// cancel-lost path).
    pub async fn cancel_and_wait(&self, task_id: &str) -> Option<BackgroundTask> {
        let (cancel, mut done) = {
            let entry = self.tasks.get(task_id)?;
            (entry.value().cancel.clone(), entry.value().done.clone())
        };
        cancel.cancel();
        let _ = tokio::time::timeout(CANCEL_SETTLE_WAIT, async move {
            let _ = done.wait_for(|v| *v).await;
        })
        .await;
        task_db::get_task(&self.pool, task_id).await.ok().flatten()
    }

    /// Stop every active task of a conversation and wait (bounded) for
    /// their settlement — the conversation-deletion cascade. Also removes
    /// the conversation's output directory (best effort).
    pub async fn cancel_tasks_for_conversation(&self, conversation_id: &str) {
        let mut waiters = Vec::new();
        for entry in self.tasks.iter() {
            if entry.value().conversation_id == conversation_id {
                entry.value().cancel.cancel();
                waiters.push(entry.value().done.clone());
            }
        }
        let _ = tokio::time::timeout(CANCEL_SETTLE_WAIT, async move {
            for mut done in waiters {
                let _ = done.wait_for(|v| *v).await;
            }
        })
        .await;
        let _ = tokio::fs::remove_dir_all(self.exec_dir.join(conversation_id)).await;
    }

    /// Kick the notification drainer for a conversation (no-op when one is
    /// already running): it re-reads the pending list from the DB, so the
    /// kick carries no data and a task settling right after a drainer's
    /// last query is still picked up by the next iteration.
    pub fn kick_notification(self: &Arc<Self>, conversation_id: &str) {
        if self.delivering.contains_key(conversation_id) {
            return;
        }
        self.delivering.insert(conversation_id.to_string(), ());
        let manager = Arc::clone(self);
        let conv = conversation_id.to_string();
        tokio::spawn(async move {
            manager.run_delivery_loop(&conv).await;
            manager.delivering.remove(&conv);
        });
    }

    /// The delivery loop: deliver pending notifications until none remain,
    /// then let the kick marker go. Busy conversations are retried — the
    /// drainer parks while any run (the user's or a previous wake's) holds
    /// the occupancy.
    async fn run_delivery_loop(&self, conversation_id: &str) {
        loop {
            let pending =
                match task_db::list_pending_notifications(&self.pool, conversation_id).await {
                    Ok(pending) => pending,
                    Err(e) => {
                        tracing::error!(
                            conversation_id = conversation_id,
                            "background notification query failed: {e}"
                        );
                        return;
                    }
                };
            if pending.is_empty() {
                return;
            }
            if let Err(e) = self.deliver_pending(conversation_id).await {
                // Leave the rows `notified = 0`: the send/resume catch-up
                // owns them from here.
                tracing::error!(
                    conversation_id = conversation_id,
                    "background notification delivery failed: {e}"
                );
                return;
            }
        }
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
                Err(crate::api::error::AppError::SessionBusy(_)) => {
                    tokio::time::sleep(DELIVERY_IDLE_RETRY).await;
                }
                Err(e) => return Err(e.into()),
            }
        };

        // Re-read under the guard: tasks settled while we were waiting join
        // this batch (the loop's next query would catch them too, but one
        // message per wake keeps the model turns cheap).
        let pending = task_db::list_pending_notifications(&self.pool, conversation_id).await?;
        if pending.is_empty() {
            return Ok(()); // the guard drops here
        }

        let (content, details) = build_notification_content(&pending).await;
        let message = Message::text(Role::User, &content);
        let task_ids: Vec<String> = pending.iter().map(|t| t.id.clone()).collect();
        let start_message_id = task_db::insert_notification_marking_tasks(
            &self.pool,
            conversation_id,
            &message,
            Some(&details),
            &task_ids,
        )
        .await?;

        // The wake run reuses the conversation's last provider/model — the
        // same rule resume follows. Resolution failures leave the message
        // delivered (it shows up on the next send); only the run is lost.
        let Some((provider_id, model_id)) =
            crate::db::messages::get_last_provider_spec(&self.pool, conversation_id).await?
        else {
            tracing::warn!(
                conversation_id,
                "background notification delivered without a wake run: no provider recorded"
            );
            return Ok(());
        };
        let spec = format!("{provider_id}:{model_id}");
        let (model, provider_id, model_id) = match service.resolve_model(&spec).await {
            Ok(resolved) => resolved,
            Err(e) => {
                tracing::warn!(
                    conversation_id,
                    "background notification delivered without a wake run: {e}"
                );
                return Ok(());
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

        service
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
        Ok(())
    }
}

/// The supervisor of one background task: waits for the child (or a
/// deliberate kill / the watchdog / the runtime cap), settles the row
/// one-shot, closes the done watch, drops the registry entry and kicks the
/// notification drainer. Owns the only `watch::Sender`.
async fn supervise_task(
    manager: Arc<BackgroundTaskManager>,
    task_id: String,
    conversation_id: String,
    mut child: tokio::process::Child,
    pgid: Option<u32>,
    output_path: PathBuf,
    done_tx: watch::Sender<bool>,
) {
    enum KillReason {
        None,
        Cancelled,
        OutputLimit,
        MaxRuntime,
    }

    let cancel = manager
        .tasks
        .get(&task_id)
        .map(|e| e.value().cancel.clone())
        // The registry entry was inserted right before this task spawned;
        // its absence here would be a logic bug, but failing soft (no
        // cancellation arm) still leaves the natural-exit and watchdog
        // arms below.
        .unwrap_or_default();

    let mut kill_reason = KillReason::None;
    let mut deadline = Box::pin(tokio::time::sleep(MAX_RUNTIME));
    let mut watchdog = tokio::time::interval(WATCHDOG_INTERVAL);
    let status: Option<std::process::ExitStatus> = loop {
        tokio::select! {
            s = child.wait() => break s.ok(),
            _ = cancel.cancelled() => { kill_reason = KillReason::Cancelled; break None; }
            _ = &mut deadline => { kill_reason = KillReason::MaxRuntime; break None; }
            _ = watchdog.tick() => {
                let over = tokio::fs::metadata(&output_path)
                    .await
                    .map(|m| m.len() >= manager.output_limit())
                    .unwrap_or(false);
                if over {
                    kill_reason = KillReason::OutputLimit;
                    break None;
                }
            }
        }
    };

    let status = match status {
        Some(status) => Some(status),
        None => kill_group_graceful(pgid, &mut child).await,
    };

    // Terminal mapping: a natural exit is `completed` whatever its code —
    // a non-zero exit is the command's result, not the task machinery's
    // failure (the notification carries the code). `failed` is reserved
    // for the machinery killing the command.
    let (final_status, exit_code, error) = match kill_reason {
        KillReason::None => (
            TaskFinalStatus::Completed,
            status.as_ref().and_then(|s| s.code()).map(i64::from),
            None,
        ),
        KillReason::Cancelled => (
            TaskFinalStatus::Cancelled,
            None,
            Some("stopped by task_stop"),
        ),
        KillReason::OutputLimit => (
            TaskFinalStatus::Failed,
            None,
            Some("output file exceeded the 1 GiB limit"),
        ),
        KillReason::MaxRuntime => (
            TaskFinalStatus::Failed,
            None,
            Some("exceeded the maximum background runtime (24h)"),
        ),
    };

    let output_bytes = tokio::fs::metadata(&output_path)
        .await
        .map(|m| m.len())
        .unwrap_or(0);

    let settled = task_db::finish_task(
        &manager.pool,
        &task_id,
        final_status,
        exit_code,
        error,
        i64::try_from(output_bytes).unwrap_or(i64::MAX),
    )
    .await;
    if let Err(e) = settled {
        tracing::error!(task_id = task_id, "failed to settle background task: {e}");
    }

    // Settlement is on disk: wake waiters, drop the registry entry, then
    // kick the drainer (it reads the DB, so ordering only affects which
    // iteration picks the row up).
    let _ = done_tx.send(true);
    manager.tasks.remove(&task_id);
    manager.kick_notification(&conversation_id);
}

/// Two-stage group kill: SIGTERM the whole process group, give it
/// [`TERM_GRACE`] to exit on its own, then SIGKILL the group and reap
/// (bounded — a child in uninterruptible sleep can outlast even SIGKILL).
/// Returns the reaped status when one was obtained.
async fn kill_group_graceful(
    pgid: Option<u32>,
    child: &mut tokio::process::Child,
) -> Option<std::process::ExitStatus> {
    #[cfg(unix)]
    {
        if let Some(pgid) = pgid {
            unsafe { libc::killpg(pgid as libc::pid_t, libc::SIGTERM) };
        }
    }
    #[cfg(not(unix))]
    let _ = child.start_kill();

    if let Ok(Ok(status)) = tokio::time::timeout(TERM_GRACE, child.wait()).await {
        return Some(status);
    }

    #[cfg(unix)]
    {
        if let Some(pgid) = pgid {
            unsafe { libc::killpg(pgid as libc::pid_t, libc::SIGKILL) };
        }
    }
    let _ = child.start_kill();
    match tokio::time::timeout(REAP_TIMEOUT, child.wait()).await {
        Ok(Ok(status)) => Some(status),
        _ => None,
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
             output (tail):\n{tail_note}{tail}\n\
             </task-notification>",
            id = task.id,
            command = task.command,
            status = task.status,
            path = task.output_path,
            tail = tail.1,
        ));
        details_tasks.push(serde_json::json!({
            "task_id": task.id,
            "command": task.command,
            "status": task.status,
            "exit_code": task.exit_code,
            "error": task.error,
            "output_path": task.output_path,
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
    let _ = file.read_to_end(&mut buf).await;
    (start > 0, String::from_utf8_lossy(&buf).into_owned())
}

/// One bounded slice of a task's output file, for `task_output` and the
/// output REST endpoint: `offset` pages from the start, no offset takes
/// the tail.
pub struct OutputChunk {
    pub content: String,
    /// Absolute byte offset the returned content starts at.
    pub start: u64,
    /// Byte offset the next page starts at, when more output follows.
    pub next_offset: Option<u64>,
    pub total_bytes: u64,
}

pub async fn read_output_chunk(
    path: &Path,
    offset: Option<u64>,
    max_bytes: u64,
) -> std::io::Result<OutputChunk> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let mut file = tokio::fs::File::open(path).await?;
    let total = file.metadata().await?.len();
    let start = offset.unwrap_or_else(|| total.saturating_sub(max_bytes));
    file.seek(std::io::SeekFrom::Start(start)).await?;
    let mut buf = Vec::with_capacity(max_bytes as usize);
    let mut limited = (&mut file).take(max_bytes);
    limited.read_to_end(&mut buf).await?;
    let end = start + buf.len() as u64;
    let next_offset = (end < total).then_some(end);
    Ok(OutputChunk {
        content: String::from_utf8_lossy(&buf).into_owned(),
        start,
        next_offset,
        total_bytes: total,
    })
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

/// The bootstrap sweep (see `runtime::bootstrap`): background task rows a
/// previous process left `running` become `lost`.
pub async fn task_sweep(pool: &SqlitePool) -> anyhow::Result<Vec<BackgroundTask>> {
    task_db::mark_stale_tasks_lost(pool).await
}
