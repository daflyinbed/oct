//! Worker registration and launch orchestration. This service owns only
//! worker assembly, occupancy and execution capabilities; no root assembler.
use super::inheritance::WorkerInheritance;
use super::{assembly::AssemblyServices, execution::ChildLauncher};
use crate::agent::{EventHub, RunOutcome, RunStatus};
use crate::db::{conversations as conv_db, messages as msg_db, projects::Project, runs as run_db};
use crate::runtime::{RunOccupancy, workspace_roots};
use crate::subagent::manager::is_terminal_status;
use crate::subagent::{ChildRunRequest, SubagentManager, build_task_envelope};
use crate::tool_contract::ports::{TaskDelegation, TaskError, TaskResult};
use crate::tools::{shared::WorkspaceFileLocks, workspace::WorkspaceRoots};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use futures_util::FutureExt;
use oct_llm_provider::core::{Message, Role};
use std::sync::Arc;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct DelegationService {
    shared: AssemblyServices,
    occupancy: RunOccupancy,
    subagents: Arc<SubagentManager>,
    launcher: ChildLauncher,
}

impl DelegationService {
    pub(crate) fn new(
        shared: AssemblyServices,
        occupancy: RunOccupancy,
        subagents: Arc<SubagentManager>,
    ) -> Self {
        let launcher = ChildLauncher::new(shared.clone(), subagents.registry.clone());
        Self {
            shared,
            occupancy,
            subagents,
            launcher,
        }
    }

    pub(crate) fn bind(&self, spawn: TaskSpawnConfig) -> Arc<dyn TaskDelegation> {
        Arc::new(RunDelegation {
            service: self.clone(),
            spawn,
        })
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
        let service = self.clone();
        let spawn = spawn.clone();
        let parent_call_id = parent_call_id.to_owned();
        let description = description.to_owned();
        let prompt = prompt.to_owned();
        // This owner outlives the tool future: JoinSet::shutdown may drop the
        // caller even while the registration COMMIT is in flight. Cancellation
        // travels through parent_cancel; it must never abort durable startup.
        tokio::spawn(async move {
            service
                .run_task_owned(&spawn, &parent_call_id, &description, &prompt)
                .await
        })
        .await
        .map_err(|error| task_err(anyhow!("task launcher panicked: {error}")))?
    }

    async fn run_task_owned(
        &self,
        spawn: &TaskSpawnConfig,
        parent_call_id: &str,
        description: &str,
        prompt: &str,
    ) -> std::result::Result<run_db::AgentRun, TaskError> {
        let pool = self.shared.pool.clone();

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

        let registration = self.subagents.registration.lock().await;
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
            if let Some(child) = self.subagents.child_by_agent(&existing.conversation_id) {
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
                .launch(spawn, existing, epoch, start, registration)
                .await;
        }

        // Atomic registration: child conversation + delegation message + run
        // row in ONE transaction (the registration boundary). A concurrent
        // re-entry loses the (parent_run_id, parent_tool_call_id) UNIQUE
        // index race and re-reads the winner's row.
        let settings = crate::application::settings::SettingsApplication::new(pool.clone())
            .read_for_run("worker", Some(&spawn.project.id))
            .await
            .map_err(|e| task_err(e.into()))?;
        let selected = spawn
            .inheritance
            .select(&settings.effective.tools)
            .map_err(task_err)?;
        let (provider_id, model_id) = settings
            .effective
            .model
            .identity(&spawn.provider_id, &spawn.model_id);
        let spec = format!("{provider_id}:{model_id}");
        let (model, _, _) = self
            .shared
            .models
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
        let context_window = self
            .shared
            .models
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
                    .launch(spawn, existing, epoch, start, registration)
                    .await;
            }
            Err(e) => return Err(task_err(e)),
        };

        self.launch(spawn, run, 1, start_message_id, registration)
            .await
    }

    /// Occupy the child conversation, resolve its model from the run row's
    /// snapshot and hand the run to the RunService's child supervisor;
    /// then wait for settlement (the parent's cancellation cascades through
    /// the token chain — a watch, never polling) and return the settled row.
    async fn launch(
        &self,
        spawn: &TaskSpawnConfig,
        run: run_db::AgentRun,
        epoch: i64,
        start_message_id: String,
        registration: tokio::sync::MutexGuard<'_, ()>,
    ) -> std::result::Result<run_db::AgentRun, TaskError> {
        let pool = self.shared.pool.clone();
        let cancel = spawn.parent_cancel.child_token();
        // Occupy before any asynchronous preparation, so per-child cancel
        // and recovery can see the startup owner as soon as registration ends.
        let guard = match self
            .occupancy
            .occupy_with(&run.conversation_id, cancel.clone())
        {
            Ok(guard) => guard,
            Err(error) => {
                // A busy child belongs to another launcher; a lifecycle error
                // without an owner must still terminalize our registration.
                if !self.shared.sessions.contains_key(&run.conversation_id) {
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
            let (model, provider_id, model_id) = self
                .shared
                .models
                .resolve_model(&spec)
                .await
                .map_err(|e| TaskError {
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
                self.shared
                    .models
                    .resolve_context_window(model.as_ref(), &provider_id, &model_id)
                    .await
                    .or_else(|| run.context_window.and_then(|v| u32::try_from(v).ok()))
            };

            if epoch > 1 {
                self.shared
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
                let hub = self
                    .shared
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
                self.subagents
                    .settle(
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
        let handle = self
            .launcher
            .launch(ChildRunRequest {
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
                locks: self.shared.file_locks_for(&spawn.project.id),
                inheritance: Some(spawn.inheritance.clone()),
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

    /// Cancel an execution owner, including startup before its live child
    /// handle exists. Without an owner, settle a recoverable DB row under
    /// registration and occupancy so a concurrent launcher cannot revive it.
    pub async fn cancel_run(&self, agent_id: &str) -> Result<bool> {
        if self.subagents.cancel(agent_id).is_some() {
            return Ok(true);
        }
        if let Some(handle) = self.shared.sessions.get(agent_id) {
            handle.cancel.cancel();
            return Ok(true);
        }

        let _registration = self.subagents.registration.lock().await;
        // Registration may have committed while this caller waited for its
        // lock. The session is also recovery's owner before handle publication.
        if self.subagents.cancel(agent_id).is_some() {
            return Ok(true);
        }
        if let Some(handle) = self.shared.sessions.get(agent_id) {
            handle.cancel.cancel();
            return Ok(true);
        }
        let _guard = match self
            .occupancy
            .occupy_with(agent_id, CancellationToken::new())
        {
            Ok(guard) => guard,
            Err(crate::error::AppError::SessionBusy(_)) => {
                if let Some(handle) = self.shared.sessions.get(agent_id) {
                    handle.cancel.cancel();
                    return Ok(true);
                }
                return Ok(false);
            }
            Err(error) => return Err(error.into()),
        };
        let Some(run) = run_db::latest_run_for_conversation(&self.shared.pool, agent_id).await?
        else {
            return Ok(false);
        };
        if !matches!(run.status.as_str(), "running" | "lost") {
            return Ok(false);
        }
        run_db::finish_run(
            &self.shared.pool,
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

/// Frozen delegation scope and worker inheritance for one parent run.
#[derive(Clone)]
pub struct TaskSpawnConfig {
    /// The conversation whose run is delegating.
    pub parent_conversation_id: String,
    /// The delegating run row id.
    pub parent_run_id: String,
    pub root_conversation_id: String,
    pub root_run_id: String,
    /// Project snapshot at spawn time (working dir + additional dirs).
    pub project: Project,
    pub roots: WorkspaceRoots,
    /// The project's task-tree-shared file locks.
    pub locks: Arc<WorkspaceFileLocks>,
    /// The parent's effective model — the child inherits it in v1.
    pub provider_id: String,
    pub model_id: String,
    pub context_window: Option<u32>,
    /// Effective parent exposure mode; persisted with each worker's tool set.
    pub tool_mode: crate::codemode::McpToolMode,
    /// The parent run's cancellation token: children derive `child_token()`
    /// from it, so cancelling the parent cascades to every worker.
    pub parent_cancel: CancellationToken,
    /// The parent run's hub: `subagent_updated` events land here.
    pub parent_hub: Arc<EventHub>,
    /// Frozen tool presentation and reusable MCP instances captured BEFORE
    /// appending task. No native instance or root task control is retained.
    pub inheritance: Arc<WorkerInheritance>,
    /// The parent run's OWN frozen prompt environment: children spawned from
    /// this live run inherit it verbatim (no cache re-resolution), so a
    /// mid-run `/reload` or compaction dirty flag cannot make a child drift
    /// from the parent that delegated to it. No cycle risk — the environment
    /// references nothing from the toolset.
    pub parent_env: std::sync::Arc<crate::agent::repo_instructions::FrozenPromptEnv>,
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
struct RunDelegation {
    service: DelegationService,
    spawn: TaskSpawnConfig,
}

#[async_trait]
impl TaskDelegation for RunDelegation {
    async fn delegate(
        &self,
        call_id: &str,
        description: &str,
        prompt: &str,
    ) -> std::result::Result<TaskResult, TaskError> {
        let run = self
            .service
            .run_task(&self.spawn, call_id, description, prompt)
            .await?;
        let envelope = build_task_envelope(&self.service.shared.pool, &run)
            .await
            .map_err(|error| TaskError {
                message: format!("failed to build the task envelope: {error:#}"),
                agent_id: Some(run.conversation_id),
                run_id: Some(run.id),
            })?;
        Ok(TaskResult {
            content: envelope.content,
            is_error: envelope.is_error,
            details: envelope.details,
        })
    }
}
