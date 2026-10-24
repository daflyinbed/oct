//! Atomic worker registration and scoped delegation commands. Child startup
//! owns preparation, recovery, supervision handoff and settlement.
use super::assembly::AssemblyServices;
use super::child_startup::{ChildParent, ChildStartup, LiveParent};
use super::inheritance::WorkerInheritance;
use crate::db::{conversations as conv_db, messages as msg_db, projects::Project, runs as run_db};
use crate::events::EventHub;
use crate::runtime::RunOccupancy;
use crate::subagent::{SubagentManager, build_task_envelope};
use crate::tool_contract::ports::{TaskDelegation, TaskError, TaskResult};
use crate::tools::{shared::WorkspaceFileLocks, workspace::WorkspaceRoots};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use oct_llm_provider::core::{Message, Role};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct DelegationService {
    shared: AssemblyServices,
    occupancy: RunOccupancy,
    subagents: Arc<SubagentManager>,
    startup: ChildStartup,
}

impl DelegationService {
    pub(crate) fn new(
        shared: AssemblyServices,
        occupancy: RunOccupancy,
        subagents: Arc<SubagentManager>,
    ) -> Self {
        let startup = ChildStartup::new(shared.clone(), occupancy.clone(), subagents.clone());
        Self {
            shared,
            occupancy,
            subagents,
            startup,
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

        let parent = ChildParent {
            project: spawn.project.clone(),
            provider_id: spawn.provider_id.clone(),
            model_id: spawn.model_id.clone(),
            cancel: spawn.parent_cancel.clone(),
            hub: spawn.parent_hub.clone(),
            live: Some(LiveParent {
                inheritance: spawn.inheritance.clone(),
                prompt_env: spawn.parent_env.clone(),
            }),
        };
        let registration = self.subagents.registration.clone().lock_owned().await;
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
            // Startup reuses settled rows, attaches to active children, or
            // prepares and claims the same stale run. Read-only preparation
            // leaves registration unlocked so lost-run cancellation can win.
            drop(registration);
            return self
                .startup
                .recover(&existing, parent)
                .await
                .map_err(|error| task_err_ctx(&existing, error));
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
            code_catalog_version: (spawn.tool_mode == crate::tool_contract::McpToolMode::Code)
                .then_some(crate::tool_contract::CODE_CATALOG_VERSION),
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
                drop(registration);
                return self
                    .startup
                    .recover(&existing, parent)
                    .await
                    .map_err(|error| task_err_ctx(&existing, error));
            }
            Err(e) => return Err(task_err(e)),
        };

        self.startup
            .launch_registered(&run, start_message_id, parent, registration)
            .await
            .map_err(|error| task_err_ctx(&run, error))
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
    pub tool_mode: crate::tool_contract::McpToolMode,
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
/// `is_error` marks non-completed outcomes, and `details` is the UI metadata
/// the TaskCard renders. The persisted settled row is the source of truth.
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
