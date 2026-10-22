//! The subagent lifecycle manager: registration identity,
//! launch/re-launch, settlement and result envelopes.
//!
//! Memory holds ACTIVE objects only — a terminal child's info lives in the
//! DB (`agent_runs` + the child conversation); the in-memory maps are
//! cleaned at settlement so nothing keeps a finished child's hub alive (the
//! SSE EOF guarantee). The manager never owns the RunService (two-phase
//! wire through a `OnceLock`), and the child hubs are held as `Weak`: the
//! occupancy entry holds the strong `Arc` while the run is active.

use std::sync::{Arc, Weak};
use std::time::Duration;

use anyhow::{Result, anyhow};
use futures_util::FutureExt;
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use oct_llm_provider::core::{ContentPart, Message, Role};
use oct_llm_provider::model::ChatModel;

use crate::agent::{AgentEvent, EventHub, RunOutcome, RunStatus};
use crate::context::LlmView;
use crate::db::{
    conversations as conv_db, messages as msg_db, projects as project_db, runs as run_db,
};
use crate::runtime::{OccupancyGuard, RunService, workspace_roots};
use crate::tools::shared::WorkspaceFileLocks;
use crate::tools::truncate::MAX_TOOL_OUTPUT_BYTES;
use crate::tools::truncate::truncate_middle;
use crate::tools::workspace::WorkspaceRoots;

use super::task_tool::TaskSpawnConfig;

/// One ACTIVE child run. Terminal info lives in the DB only; the `hub` is
/// deliberately `Weak` — the child conversation's occupancy entry holds the
/// strong `Arc`, so dropping the entry (guard drop at settlement) ends the
/// child's SSE streams.
pub struct ChildRun {
    pub run_id: String,
    /// The child's conversation id (== agent id in v1).
    pub agent_id: String,
    pub description: String,
    pub cancel: CancellationToken,
    /// Settled flag: `true` once startup or the loop supervisor finished
    /// settlement. Await with `watch` semantics — no polling.
    pub done: watch::Receiver<bool>,
    /// The child's event hub (weak: never keeps a terminal child's hub alive).
    pub hub: Weak<EventHub>,
    /// Which execution instance this handle belongs to; settlement writes
    /// are guarded by it.
    pub epoch: i64,
}

/// A launched child run, handed back to whoever waits on it.
pub struct ChildHandle {
    pub run_id: String,
    pub agent_id: String,
    pub done: watch::Receiver<bool>,
    pub cancel: CancellationToken,
    pub hub: Arc<EventHub>,
}

/// Everything [`RunService::start_child_run`] needs to supervise one
/// subagent run over its (already-registered, already-occupied) child
/// conversation.
pub struct ChildRunRequest {
    /// Consumed: moved into the child supervisor so the child conversation's
    /// occupancy lasts the whole run.
    pub guard: OccupancyGuard,
    pub run_id: String,
    pub epoch: i64,
    pub conversation_id: String,
    /// The delegation user message — the child run's start boundary.
    pub start_message_id: String,
    pub project: project_db::Project,
    pub model: Box<dyn ChatModel>,
    pub provider_id: String,
    pub model_id: String,
    pub view: LlmView,
    pub context_window: Option<u32>,
    pub agent_config: Option<crate::agent_settings::WorkerSnapshot>,
    /// The child's cancellation token: a `child_token()` of the parent's, so
    /// cancelling the parent cascades; cancelling it directly stops only
    /// this child.
    pub cancel: CancellationToken,
    /// The PARENT run's hub: `subagent_updated` events land here.
    pub parent_hub: Arc<EventHub>,
    pub parent_call_id: String,
    pub description: String,
    pub roots: WorkspaceRoots,
    pub locks: Arc<WorkspaceFileLocks>,
    /// The parent's live toolset when dispatched through the `task` tool
    /// (`None` during recovery — the crashed process's instances are gone
    /// and the child set is rebuilt from the same assembly sources).
    pub parent_toolset: Option<Arc<Vec<Arc<dyn crate::tools::AgentTool>>>>,
    /// The delegating run's own frozen prompt environment (live task-tool
    /// path); `None` during recovery, where the child resolves the shared
    /// cache instead. Prevents mid-run `/reload` drift between a parent and
    /// the children it delegates to.
    pub parent_env: Option<std::sync::Arc<crate::agent::repo_instructions::FrozenPromptEnv>>,
}

/// Failure of a `task` delegation, with the child's ids when they are
/// already known (registration succeeded, launch failed).
pub struct TaskError {
    pub message: String,
    pub agent_id: Option<String>,
    pub run_id: Option<String>,
}

impl std::fmt::Display for TaskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

/// Whether a persisted run status is terminal (`lost` is NOT terminal — it
/// is recoverable).
pub(crate) fn is_terminal_status(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "cancelled")
}

pub struct SubagentManager {
    /// Two-phase wire: set once, right after the RunService is constructed
    /// (`main.rs` and the test harness do this).
    service: std::sync::OnceLock<Weak<RunService>>,
    /// Active children and agent → run lookup. Completion observers only
    /// retain this index, never the full manager or its service callback.
    pub(crate) registry: Arc<super::registry::ChildRegistry>,
    /// Protect registration through publishing the live handle. Released
    /// before waiting for execution, so independent children remain parallel.
    registration: tokio::sync::Mutex<()>,
}

impl Default for SubagentManager {
    fn default() -> Self {
        Self {
            service: std::sync::OnceLock::new(),
            registry: Arc::new(super::registry::ChildRegistry::default()),
            registration: tokio::sync::Mutex::new(()),
        }
    }
}

impl SubagentManager {
    /// Complete the two-phase construction (idempotent).
    pub fn set_service(&self, service: Arc<RunService>) {
        let _ = self.service.set(Arc::downgrade(&service));
    }

    fn service(&self) -> Result<Arc<RunService>> {
        self.service
            .get()
            .and_then(Weak::upgrade)
            .ok_or_else(|| anyhow!("SubagentManager is not wired to a RunService yet"))
    }

    /// The wired RunService, when the two-phase construction has completed
    /// (callers that only need best-effort DB work use this).
    pub fn service_handle(&self) -> Option<Arc<RunService>> {
        self.service.get().and_then(Weak::upgrade)
    }

    /// Record a starting child (called by [`RunService::start_child_run`]
    /// before assembly awaits, then retained by the loop supervisor).
    pub fn register_child(&self, child: ChildRun) {
        self.registry.register(child);
    }

    /// The ACTIVE child run of an agent conversation, if any.
    pub fn child_by_agent(&self, agent_id: &str) -> Option<Arc<ChildRun>> {
        self.registry.child_by_agent(agent_id)
    }

    /// Whether an agent conversation currently has an active in-memory run.
    pub fn is_active(&self, agent_id: &str) -> bool {
        self.registry.is_active(agent_id)
    }

    /// The `task` tool's execution path: register (or
    /// re-attach to) the child run for one parent tool call, run it to a
    /// terminal state and return the settled row — the envelope is built
    /// from that row by the caller.
    pub async fn run_task(
        &self,
        spawn: &TaskSpawnConfig,
        parent_call_id: &str,
        description: &str,
        prompt: &str,
    ) -> std::result::Result<run_db::AgentRun, TaskError> {
        let service = self.service().map_err(|e| TaskError {
            message: e.to_string(),
            agent_id: None,
            run_id: None,
        })?;
        let spawn = spawn.clone();
        let parent_call_id = parent_call_id.to_owned();
        let description = description.to_owned();
        let prompt = prompt.to_owned();
        // This owner outlives the tool future: JoinSet::shutdown may drop the
        // caller even while the registration COMMIT is in flight. Cancellation
        // travels through parent_cancel; it must never abort durable startup.
        // Keep the weak toolset upgrade alive until startup/settlement finishes,
        // even if the cancelled parent has already released its own tools.
        let parent_tools = spawn.parent_toolset.get().and_then(Weak::upgrade);
        tokio::spawn(async move {
            let result = service
                .subagents
                .run_task_owned(&service, &spawn, &parent_call_id, &description, &prompt)
                .await;
            drop(parent_tools);
            result
        })
        .await
        .map_err(|error| task_err(anyhow!("task launcher panicked: {error}")))?
    }

    async fn run_task_owned(
        &self,
        service: &Arc<RunService>,
        spawn: &TaskSpawnConfig,
        parent_call_id: &str,
        description: &str,
        prompt: &str,
    ) -> std::result::Result<run_db::AgentRun, TaskError> {
        let pool = service.pool.clone();

        // Server-side depth guard: only a ROOT run may delegate — workers
        // never see the task tool in their schema, and this check makes the
        // runtime reject nesting even if the schema leaked (depth <= 1).
        let parent_run = run_db::get_run(&pool, &spawn.parent_run_id)
            .await
            .map_err(task_err)?
            .ok_or_else(|| TaskError {
                message: format!("parent run {} not found", spawn.parent_run_id),
                agent_id: None,
                run_id: None,
            })?;
        if parent_run.kind != "root" {
            return Err(TaskError {
                message: "task 嵌套被拒绝 (depth <= 1)".to_string(),
                agent_id: None,
                run_id: None,
            });
        }

        let registration = self.registration.lock().await;
        if spawn.parent_cancel.is_cancelled() {
            return Err(task_err(anyhow!("subagent startup cancelled")));
        }
        // Registration idempotency: one child run per (parent_run_id,
        // parent_tool_call_id) — existing rows are awaited or continued,
        // never duplicated.
        if let Some(existing) =
            run_db::find_run_by_parent_call(&pool, &spawn.parent_run_id, parent_call_id)
                .await
                .map_err(task_err)?
        {
            if is_terminal_status(&existing.status) {
                // 子已完成父未接收: the settled row IS the answer.
                return Ok(existing);
            }
            if let Some(child) = self.child_by_agent(&existing.conversation_id) {
                // Running in memory: attach to the wait (concurrent re-entry
                // of the same call id shares one execution).
                let mut done = child.done.clone();
                drop(registration);
                let _ = done.wait_for(|d| *d).await;
                return run_db::get_run(&pool, &existing.id)
                    .await
                    .map_err(task_err)?
                    .ok_or_else(|| TaskError {
                        message: format!("child run row {} vanished", existing.id),
                        agent_id: Some(existing.conversation_id.clone()),
                        run_id: Some(existing.id.clone()),
                    });
            }
            // Validate before claiming the stale execution: a failed read must
            // leave a lost row recoverable rather than creating a new owner.
            let start = msg_db::first_user_message_id(&pool, &existing.conversation_id)
                .await
                .map_err(task_err)?
                .ok_or_else(|| TaskError {
                    message: format!(
                        "child conversation {} has no delegation message",
                        existing.conversation_id
                    ),
                    agent_id: Some(existing.conversation_id.clone()),
                    run_id: Some(existing.id.clone()),
                })?;
            // Running-but-not-in-memory: a stale execution instance. Bump the
            // epoch (its eventual settlement will no-op on the epoch guard)
            // and continue the SAME row with a fresh instance.
            let epoch = run_db::bump_execution_epoch(&pool, &existing.id)
                .await
                .map_err(task_err)?
                .ok_or_else(|| TaskError {
                    message: format!("child run {} already settled", existing.id),
                    agent_id: Some(existing.conversation_id.clone()),
                    run_id: Some(existing.id.clone()),
                })?;
            return self
                .launch(service, spawn, existing, epoch, start, registration)
                .await;
        }

        // Atomic registration: child conversation + delegation message + run
        // row in ONE transaction (the registration boundary). A concurrent
        // re-entry loses the (parent_run_id, parent_tool_call_id) UNIQUE
        // index race and re-reads the winner's row.
        let settings =
            crate::agent_settings::read_for_run(&pool, "worker", Some(&spawn.project.id))
                .await
                .map_err(|e| task_err(e.into()))?;
        let parent_tools = spawn
            .parent_toolset
            .get()
            .and_then(std::sync::Weak::upgrade)
            .ok_or_else(|| task_err(anyhow!("Parent toolset no longer exists")))?;
        let selected =
            crate::runtime::worker_config::select_tools(&parent_tools, &settings.effective.tools)
                .map_err(task_err)?;
        let (provider_id, model_id) = settings
            .effective
            .model
            .identity(&spawn.provider_id, &spawn.model_id);
        let spec = format!("{provider_id}:{model_id}");
        let (model, _, _) = service
            .resolve_model(&spec)
            .await
            .map_err(|e| task_err(e.into()))?;
        if !selected.is_empty()
            && let Some(row) = crate::db::models::get_model(&pool, &provider_id, &model_id)
                .await
                .map_err(task_err)?
            && !row.tool_call
        {
            return Err(task_err(anyhow!(
                "Worker model does not support tool calls"
            )));
        }
        let context_window = service
            .resolve_context_window(model.as_ref(), &provider_id, &model_id)
            .await;
        let snapshot = crate::agent_settings::WorkerSnapshot {
            version: 2,
            agent_type: "worker".into(),
            instructions: settings.effective.instructions,
            prompt_template_version: 1,
            tools: selected,
            sources: settings.sources,
            tool_mode: spawn.tool_mode,
            code_catalog_version: (spawn.tool_mode == crate::codemode::McpToolMode::Code)
                .then_some(crate::codemode::catalog::VERSION),
        };
        let (run, start_message_id) = match register_child_tx(
            &pool,
            spawn,
            parent_call_id,
            description,
            prompt,
            WorkerRegistration {
                provider_id: &provider_id,
                model_id: &model_id,
                context_window,
                snapshot: &snapshot,
            },
        )
        .await
        {
            Ok(pair) => pair,
            Err(e) if is_unique_violation(&e) => {
                let existing =
                    run_db::find_run_by_parent_call(&pool, &spawn.parent_run_id, parent_call_id)
                        .await
                        .map_err(task_err)?
                        .ok_or_else(|| TaskError {
                            message: format!("registration raced but no row found: {e:#}"),
                            agent_id: None,
                            run_id: None,
                        })?;
                let start = delegation_message_of(&pool, &existing)
                    .await
                    .map_err(task_err)?;
                let epoch = existing.execution_epoch;
                return self
                    .launch(service, spawn, existing, epoch, start, registration)
                    .await;
            }
            Err(e) => return Err(task_err(e)),
        };

        self.launch(service, spawn, run, 1, start_message_id, registration)
            .await
    }

    /// Occupy the child conversation, resolve its model from the run row's
    /// snapshot and hand the run to the RunService's child supervisor;
    /// then wait for settlement (the parent's cancellation cascades through
    /// the token chain — a watch, never polling) and return the settled row.
    async fn launch(
        &self,
        service: &Arc<RunService>,
        spawn: &TaskSpawnConfig,
        run: run_db::AgentRun,
        epoch: i64,
        start_message_id: String,
        registration: tokio::sync::MutexGuard<'_, ()>,
    ) -> std::result::Result<run_db::AgentRun, TaskError> {
        let pool = service.pool.clone();
        let cancel = spawn.parent_cancel.child_token();
        // Occupy before any asynchronous preparation, so per-child cancel
        // and recovery can see the startup owner as soon as registration ends.
        let guard = match service.occupy_with(&run.conversation_id, cancel.clone()) {
            Ok(guard) => guard,
            Err(error) => {
                // A busy child belongs to another launcher; a lifecycle error
                // without an owner must still terminalize our registration.
                if !service.sessions.contains_key(&run.conversation_id) {
                    run_db::finish_run(
                        &pool,
                        &run.id,
                        run_db::RunFinalStatus::Failed,
                        None,
                        Some(&error.to_string()),
                        None,
                        None,
                        Some(epoch),
                    )
                    .await
                    .map_err(task_err)?;
                }
                return Err(task_err_ctx(&run, error.into()));
            }
        };
        let preparation = async {
            let provider_id = run
                .provider_id
                .clone()
                .unwrap_or_else(|| spawn.provider_id.clone());
            let model_id = run
                .model_id
                .clone()
                .unwrap_or_else(|| spawn.model_id.clone());
            let spec = format!("{provider_id}:{model_id}");
            let (model, provider_id, model_id) =
                service.resolve_model(&spec).await.map_err(|e| TaskError {
                    message: format!("failed to resolve the child model {spec}: {e}"),
                    agent_id: Some(run.conversation_id.clone()),
                    run_id: Some(run.id.clone()),
                })?;
            let agent_config =
                crate::agent_settings::WorkerSnapshot::decode(run.agent_config_json.as_deref())
                    .map_err(|e| task_err_ctx(&run, e))?;
            let context_window = if agent_config.is_some() {
                run.context_window.and_then(|v| u32::try_from(v).ok())
            } else {
                service
                    .resolve_context_window(model.as_ref(), &provider_id, &model_id)
                    .await
                    .or_else(|| run.context_window.and_then(|v| u32::try_from(v).ok()))
            };

            if epoch > 1 {
                service
                    .repair_dangling_tools(&run.conversation_id, &[])
                    .await
                    .map_err(|e| task_err_ctx(&run, e))?;
            }
            let view = msg_db::load_llm_view(&pool, &run.conversation_id)
                .await
                .map_err(|e| task_err_ctx(&run, e))?;

            Ok((
                model,
                provider_id,
                model_id,
                agent_config,
                context_window,
                view,
            ))
        };
        let prepared = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(task_err_ctx(&run, anyhow!("subagent startup cancelled"))),
            result = std::panic::AssertUnwindSafe(preparation).catch_unwind() => {
                result.unwrap_or_else(|_| Err(task_err_ctx(&run, anyhow!("subagent startup panicked"))))
            }
        };
        let (model, provider_id, model_id, agent_config, context_window, view) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                let hub = service
                    .sessions
                    .get(&run.conversation_id)
                    .unwrap()
                    .hub
                    .clone();
                let outcome = RunOutcome {
                    status: if cancel.is_cancelled() {
                        RunStatus::Cancelled
                    } else {
                        RunStatus::Failed
                    },
                    final_message_id: None,
                    final_text: None,
                    partial: false,
                    usage: None,
                    error: Some(error.message.clone()),
                };
                let (done_tx, _) = watch::channel(false);
                self.settle(
                    &pool,
                    &run.id,
                    &run.conversation_id,
                    run.description.as_deref().unwrap_or_default(),
                    epoch,
                    run.parent_tool_call_id.as_deref().unwrap_or_default(),
                    &outcome,
                    &spawn.parent_hub,
                    &hub,
                    &done_tx,
                )
                .await;
                drop(guard);
                return Err(error);
            }
        };

        let (roots, _additional) = workspace_roots(&spawn.project);
        let handle = service
            .start_child_run(ChildRunRequest {
                guard,
                run_id: run.id.clone(),
                epoch,
                conversation_id: run.conversation_id.clone(),
                start_message_id,
                project: spawn.project.clone(),
                model,
                provider_id,
                model_id,
                view,
                context_window,
                agent_config,
                cancel,
                parent_hub: spawn.parent_hub.clone(),
                parent_call_id: run.parent_tool_call_id.clone().unwrap_or_default(),
                description: run.description.clone().unwrap_or_default(),
                roots,
                locks: service.file_locks_for(&spawn.project.id),
                parent_toolset: spawn.parent_toolset.get().and_then(Weak::upgrade),
                parent_env: Some(spawn.parent_env.clone()),
            })
            .await
            .map_err(|e| task_err_ctx(&run, anyhow!("failed to start the child run: {e}")))?;

        drop(registration);
        // Wait for settlement: DB row is the single source of truth for the
        // envelope. Cancellation of the parent cancels the child through the
        // token chain; settlement itself never depends on this await.
        let mut done = handle.done.clone();
        let _ = done.wait_for(|d| *d).await;
        run_db::get_run(&pool, &run.id)
            .await
            .map_err(task_err)?
            .ok_or_else(|| TaskError {
                message: format!("child run row {} vanished", run.id),
                agent_id: Some(run.conversation_id.clone()),
                run_id: Some(run.id.clone()),
            })
    }

    /// Child-run settlement (called by the child supervisor after the loop
    /// resolves, including the panic path):
    /// 1. `finish_run` with the epoch guard — a stale execution instance's
    ///    write no-ops instead of overwriting a newer instance's outcome.
    /// 2. Publish `subagent_updated {status: <terminal>, execution_epoch}`
    ///    on the parent hub (harmless when nobody subscribes; the epoch lets
    ///    consumers discard stale-instance events).
    /// 3. Close the done watch and remove the in-memory entries — terminal
    ///    info lives in the DB only from here on.
    #[allow(clippy::too_many_arguments)]
    pub async fn settle(
        &self,
        pool: &sqlx::SqlitePool,
        run_id: &str,
        agent_id: &str,
        description: &str,
        epoch: i64,
        parent_call_id: &str,
        outcome: &RunOutcome,
        parent_hub: &Arc<EventHub>,
        child_hub: &Arc<EventHub>,
        done_tx: &watch::Sender<bool>,
    ) {
        let identity = crate::runtime::execution::ExecutionIdentity {
            run_id: run_id.to_owned(),
            conversation_id: agent_id.to_owned(),
            epoch: Some(epoch),
        };
        let observer = super::registry::ChildCompletion {
            registry: self.registry.clone(),
            agent_id: agent_id.to_owned(),
            parent_call_id: parent_call_id.to_owned(),
            description: description.to_owned(),
            parent_hub: parent_hub.clone(),
            done: done_tx.clone(),
        };
        if let Err(error) = crate::runtime::execution::finish_execution(
            pool,
            &identity,
            outcome,
            child_hub,
            Some(&observer),
        )
        .await
        {
            tracing::error!(%run_id, %error, "failed to settle child run");
        }
    }

    /// The ACTIVE children of a root conversation (used by conversation
    /// deletion: cancel them and wait for settlement before removing rows).
    pub async fn active_children_of_root(
        &self,
        pool: &sqlx::SqlitePool,
        root_conversation_id: &str,
    ) -> Result<Vec<Arc<ChildRun>>> {
        let rows = run_db::list_subagent_runs(pool, root_conversation_id).await?;
        Ok(rows
            .iter()
            .filter(|r| r.status == "running" || r.status == "lost")
            .filter_map(|r| self.child_by_agent(&r.conversation_id))
            .collect())
    }

    /// Cancel one agent's published active run.
    pub fn cancel(&self, agent_id: &str) -> Option<Arc<ChildRun>> {
        let child = self.child_by_agent(agent_id)?;
        child.cancel.cancel();
        Some(child)
    }

    /// Cancel an execution owner, including startup before its live child
    /// handle exists. Without an owner, settle a recoverable DB row under
    /// registration and occupancy so a concurrent launcher cannot revive it.
    pub async fn cancel_run(&self, agent_id: &str) -> Result<bool> {
        let service = self.service()?;
        if self.cancel(agent_id).is_some() {
            return Ok(true);
        }
        if let Some(handle) = service.sessions.get(agent_id) {
            handle.cancel.cancel();
            return Ok(true);
        }

        let _registration = self.registration.lock().await;
        // Registration may have committed while this caller waited for its
        // lock. The session is also recovery's owner before handle publication.
        if self.cancel(agent_id).is_some() {
            return Ok(true);
        }
        if let Some(handle) = service.sessions.get(agent_id) {
            handle.cancel.cancel();
            return Ok(true);
        }
        let _guard = match service.occupy_with(agent_id, CancellationToken::new()) {
            Ok(guard) => guard,
            Err(crate::error::AppError::SessionBusy(_)) => {
                if let Some(handle) = service.sessions.get(agent_id) {
                    handle.cancel.cancel();
                    return Ok(true);
                }
                return Ok(false);
            }
            Err(error) => return Err(error.into()),
        };
        let Some(run) = run_db::latest_run_for_conversation(&service.pool, agent_id).await? else {
            return Ok(false);
        };
        if !matches!(run.status.as_str(), "running" | "lost") {
            return Ok(false);
        }
        run_db::finish_run(
            &service.pool,
            &run.id,
            run_db::RunFinalStatus::Cancelled,
            run.final_message_id.as_deref(),
            run.error.as_deref(),
            run.input_tokens,
            run.output_tokens,
            Some(run.execution_epoch),
        )
        .await
    }
}

fn task_err(e: anyhow::Error) -> TaskError {
    TaskError {
        message: format!("{e:#}"),
        agent_id: None,
        run_id: None,
    }
}

fn task_err_ctx(run: &run_db::AgentRun, e: anyhow::Error) -> TaskError {
    TaskError {
        message: format!("{e:#}"),
        agent_id: Some(run.conversation_id.clone()),
        run_id: Some(run.id.clone()),
    }
}

/// SQLite UNIQUE-index violation (the (parent_run_id, parent_tool_call_id)
/// registration race).
fn is_unique_violation(e: &anyhow::Error) -> bool {
    e.downcast_ref::<sqlx::Error>().is_some_and(
        |db_err| matches!(db_err.as_database_error(), Some(db) if db.is_unique_violation()),
    )
}

// ---------------------------------------------------------------------------
// Registration transaction
// ---------------------------------------------------------------------------

struct WorkerRegistration<'a> {
    provider_id: &'a str,
    model_id: &'a str,
    context_window: Option<u32>,
    snapshot: &'a crate::agent_settings::WorkerSnapshot,
}

/// Create the child conversation, the delegation user message and the
/// `agent_runs` row in ONE transaction. The provider/model
/// are stamped on the delegation message so the child history records its
/// model, and on the run row as the effective-config snapshot.
async fn register_child_tx(
    pool: &sqlx::SqlitePool,
    spawn: &TaskSpawnConfig,
    parent_call_id: &str,
    description: &str,
    prompt: &str,
    worker: WorkerRegistration<'_>,
) -> Result<(run_db::AgentRun, String)> {
    let WorkerRegistration {
        provider_id,
        model_id,
        context_window,
        snapshot,
    } = worker;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let conversation = conv_db::create_subagent_conversation_in_tx(
        &mut tx,
        conv_db::NewSubagentConversation {
            project_id: &spawn.project.id,
            parent_conversation_id: &spawn.parent_conversation_id,
            root_conversation_id: &spawn.root_conversation_id,
            agent_type: "worker",
            title: description,
        },
    )
    .await?;
    let delegation = Message::text(Role::User, prompt);
    let stored = msg_db::insert_message_in_tx(
        &mut tx,
        &conversation.id,
        &delegation,
        Some(provider_id),
        Some(model_id),
        None,
    )
    .await?;
    let mut run = run_db::create_run_in_tx(
        &mut tx,
        run_db::NewRun {
            conversation_id: conversation.id.clone(),
            kind: "subagent",
            parent_run_id: Some(spawn.parent_run_id.clone()),
            root_run_id: Some(spawn.root_run_id.clone()),
            parent_tool_call_id: Some(parent_call_id.to_string()),
            provider_id: Some(provider_id.to_string()),
            model_id: Some(model_id.to_string()),
            context_window: context_window.map(i64::from),
            description: Some(description.to_string()),
        },
    )
    .await?;
    let json = serde_json::to_string(snapshot)?;
    sqlx::query!(
        "UPDATE agent_runs SET agent_config_json = ? WHERE id = ?",
        json,
        run.id
    )
    .execute(&mut *tx)
    .await?;
    run.agent_config_json = Some(json);
    tx.commit().await?;
    Ok((run, stored.id))
}

// ---------------------------------------------------------------------------
// Result envelope
// ---------------------------------------------------------------------------

/// The model-visible task result: `content` is the envelope JSON text,
/// `is_error` marks non-completed outcomes, `details` is the UI metadata
/// the TaskCard renders.
pub struct TaskEnvelope {
    pub content: String,
    pub is_error: bool,
    pub details: Value,
}

/// Build the task envelope from a SETTLED run row + the child pool:
/// shared verbatim by the live path, the crash backfill and the
/// recovery — the DB row is the single source of truth.
pub async fn build_task_envelope(
    pool: &sqlx::SqlitePool,
    run: &run_db::AgentRun,
) -> Result<TaskEnvelope> {
    anyhow::ensure!(
        is_terminal_status(&run.status),
        "child run {} is not durably settled",
        run.id
    );
    let output = final_output_text(pool, run).await?;
    let truncated_output = truncate_middle(&output, MAX_TOOL_OUTPUT_BYTES);
    let truncated = truncated_output != output;

    let mut envelope = json!({
        "agent_id": run.conversation_id,
        "run_id": run.id,
        "status": run.status,
        "output": truncated_output,
        "truncated": truncated,
        "usage": usage_json(run),
    });
    if let Some(error) = &run.error {
        envelope["error"] = json!(error);
    }
    // Interrupted runs may still carry persisted partial output — it must
    // not read as a settled success.
    if run.status != "completed" && !output.is_empty() {
        envelope["partial"] = json!(true);
    }

    let description = run.description.clone().unwrap_or_default();
    Ok(TaskEnvelope {
        content: serde_json::to_string(&envelope)?,
        is_error: run.status != "completed",
        details: json!({
            "kind": "subagent_task",
            "agent_id": run.conversation_id,
            "run_id": run.id,
            "description": description,
            "status": run.status,
        }),
    })
}

/// `{"input_tokens":..,"output_tokens":..}` when either side is known,
/// `null` when usage never arrived (unknown ≠ zero).
fn usage_json(run: &run_db::AgentRun) -> Value {
    match (run.input_tokens, run.output_tokens) {
        (None, None) => Value::Null,
        (input, output) => json!({
            "input_tokens": input,
            "output_tokens": output,
        }),
    }
}

/// The run's final assistant text: the `final_message_id` content row when
/// present, else the last assistant row of the child conversation (crash
/// paths may have persisted the message without updating the run row).
async fn final_output_text(pool: &sqlx::SqlitePool, run: &run_db::AgentRun) -> Result<String> {
    if let Some(final_id) = &run.final_message_id
        && let Some(row) = msg_db::get_message_content(pool, final_id).await?
    {
        return Ok(text_of_parts(&row.parts_json));
    }
    let stored = msg_db::list_messages(pool, &run.conversation_id).await?;
    Ok(stored
        .iter()
        .rev()
        .find(|m| m.role == "assistant")
        .map(|m| text_of_parts(&m.parts_json))
        .unwrap_or_default())
}

/// Concatenated text parts of a serialized `Vec<ContentPart>`.
fn text_of_parts(parts_json: &str) -> String {
    serde_json::from_str::<Vec<ContentPart>>(parts_json)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| match p {
                    ContentPart::Text(text) => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

/// The delegation message id for an already-registered child: its first
/// user row (the child conversation starts with the delegation prompt by
/// construction).
async fn delegation_message_of(pool: &sqlx::SqlitePool, run: &run_db::AgentRun) -> Result<String> {
    msg_db::first_user_message_id(pool, &run.conversation_id)
        .await?
        .ok_or_else(|| {
            anyhow!(
                "child conversation {} has no delegation message",
                run.conversation_id
            )
        })
}

/// Re-export for the API layer: the activity summary of an active child
/// (best-effort from its hub snapshot), used by the subagent list endpoint.
pub fn activity_of(child: &ChildRun) -> Option<String> {
    let hub = child.hub.upgrade()?;
    hub.snapshot().iter().rev().find_map(|e| match e {
        AgentEvent::ToolCallStart { title, .. } => Some(title.clone()),
        _ => None,
    })
}

/// Await a child's settlement with a hard timeout (conversation deletion
/// uses this so a stuck child cannot hang the DELETE forever).
pub async fn await_done(child: &ChildRun, timeout: Duration) -> bool {
    let mut done = child.done.clone();
    matches!(
        tokio::time::timeout(timeout, done.wait_for(|d| *d)).await,
        Ok(Ok(_))
    )
}
