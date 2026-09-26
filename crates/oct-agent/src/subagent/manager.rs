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
use dashmap::DashMap;
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use oct_llm_provider::core::{ContentPart, Message, Role};
use oct_llm_provider::model::ChatModel;

use crate::agent::{AgentEvent, EventHub, RunOutcome, RunStatus, SubagentRunStatus};
use crate::context::LlmView;
use crate::db::{conversations as conv_db, messages as msg_db, projects as project_db, runs as run_db};
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
    /// Settled flag: `true` once the supervisor finished settlement. Await
    /// with `watch` semantics — no polling.
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
    service: std::sync::OnceLock<Arc<RunService>>,
    /// key = run id — ACTIVE children only.
    children: DashMap<String, Arc<ChildRun>>,
    /// key = agent (child conversation) id → run id — ACTIVE children only.
    by_agent: DashMap<String, String>,
}

impl Default for SubagentManager {
    fn default() -> Self {
        Self {
            service: std::sync::OnceLock::new(),
            children: DashMap::new(),
            by_agent: DashMap::new(),
        }
    }
}

impl SubagentManager {
    /// Complete the two-phase construction (idempotent).
    pub fn set_service(&self, service: Arc<RunService>) {
        let _ = self.service.set(service);
    }

    fn service(&self) -> Result<Arc<RunService>> {
        self.service
            .get()
            .cloned()
            .ok_or_else(|| anyhow!("SubagentManager is not wired to a RunService yet"))
    }

    /// The wired RunService, when the two-phase construction has completed
    /// (callers that only need best-effort DB work use this).
    pub fn service_handle(&self) -> Option<Arc<RunService>> {
        self.service.get().cloned()
    }

    /// Record a freshly launched child (called by
    /// [`RunService::start_child_run`] right before spawning the
    /// supervisor).
    pub fn register_child(&self, child: ChildRun) {
        self.by_agent
            .insert(child.agent_id.clone(), child.run_id.clone());
        self.children.insert(child.run_id.clone(), Arc::new(child));
    }

    /// The ACTIVE child run of an agent conversation, if any.
    pub fn child_by_agent(&self, agent_id: &str) -> Option<Arc<ChildRun>> {
        let run_id = self.by_agent.get(agent_id)?.value().clone();
        self.children.get(&run_id).map(|c| c.value().clone())
    }

    /// Whether an agent conversation currently has an active in-memory run.
    pub fn is_active(&self, agent_id: &str) -> bool {
        self.by_agent.contains_key(agent_id)
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
            return self
                .launch(&service, spawn, existing, epoch, start)
                .await;
        }

        // Atomic registration: child conversation + delegation message + run
        // row in ONE transaction (the registration boundary). A concurrent
        // re-entry loses the (parent_run_id, parent_tool_call_id) UNIQUE
        // index race and re-reads the winner's row.
        let (run, start_message_id) = match register_child_tx(
            &pool,
            spawn,
            parent_call_id,
            description,
            prompt,
        )
        .await
        {
            Ok(pair) => pair,
            Err(e) if is_unique_violation(&e) => {
                let existing = run_db::find_run_by_parent_call(
                    &pool,
                    &spawn.parent_run_id,
                    parent_call_id,
                )
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
                return self.launch(&service, spawn, existing, 1, start).await;
            }
            Err(e) => return Err(task_err(e)),
        };

        self.launch(&service, spawn, run, 1, start_message_id).await
    }

    /// Resolve the child model from the run row's snapshot, occupy the child
    /// conversation and hand the run to the RunService's child supervisor;
    /// then wait for settlement (the parent's cancellation cascades through
    /// the token chain — a watch, never polling) and return the settled row.
    async fn launch(
        &self,
        service: &Arc<RunService>,
        spawn: &TaskSpawnConfig,
        run: run_db::AgentRun,
        epoch: i64,
        start_message_id: String,
    ) -> std::result::Result<run_db::AgentRun, TaskError> {
        let pool = service.pool.clone();
        let provider_id = run
            .provider_id
            .clone()
            .unwrap_or_else(|| spawn.provider_id.clone());
        let model_id = run.model_id.clone().unwrap_or_else(|| spawn.model_id.clone());
        let spec = format!("{provider_id}:{model_id}");
        let (model, provider_id, model_id) = service.resolve_model(&spec).await.map_err(|e| TaskError {
            message: format!("failed to resolve the child model {spec}: {e}"),
            agent_id: Some(run.conversation_id.clone()),
            run_id: Some(run.id.clone()),
        })?;
        let context_window = service
            .resolve_context_window(model.as_ref(), &provider_id, &model_id)
            .await
            .or_else(|| run.context_window.and_then(|v| u32::try_from(v).ok()));

        let view = msg_db::load_llm_view(&pool, &run.conversation_id)
            .await
            .map_err(|e| task_err_ctx(&run, e))?;

        let cancel = spawn.parent_cancel.child_token();
        let guard = service
            .occupy_with(&run.conversation_id, cancel.clone())
            .map_err(|e| TaskError {
                message: format!("child conversation is busy: {e}"),
                agent_id: Some(run.conversation_id.clone()),
                run_id: Some(run.id.clone()),
            })?;

        let (roots, _additional) = workspace_roots(&spawn.project);
        let locks = service.file_locks_for(&spawn.project.id);
        let description = run.description.clone().unwrap_or_default();
        let parent_call_id = run
            .parent_tool_call_id
            .clone()
            .unwrap_or_default();

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
                cancel,
                parent_hub: spawn.parent_hub.clone(),
                parent_call_id,
                description,
                roots,
                locks,
                parent_toolset: spawn
                    .parent_toolset
                    .get()
                    .and_then(std::sync::Weak::upgrade),
            })
            .await
            .map_err(|e| TaskError {
                message: format!("failed to start the child run: {e}"),
                agent_id: Some(run.conversation_id.clone()),
                run_id: Some(run.id.clone()),
            })?;

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
        let final_status = match outcome.status {
            RunStatus::Completed => run_db::RunFinalStatus::Completed,
            RunStatus::Failed => run_db::RunFinalStatus::Failed,
            RunStatus::Cancelled => run_db::RunFinalStatus::Cancelled,
        };
        if let Err(e) = run_db::finish_run(
            pool,
            run_id,
            final_status,
            outcome.final_message_id.as_deref(),
            outcome.error.as_deref(),
            outcome
                .usage
                .as_ref()
                .map(|u| i64::try_from(u.input_tokens).unwrap_or(i64::MAX)),
            outcome
                .usage
                .as_ref()
                .map(|u| i64::try_from(u.output_tokens).unwrap_or(i64::MAX)),
            Some(epoch),
        )
        .await
        {
            tracing::error!("failed to persist terminal state of child run {run_id}: {e}");
        }

        let status = match outcome.status {
            RunStatus::Completed => "completed",
            RunStatus::Failed => "failed",
            RunStatus::Cancelled => "cancelled",
        };

        // Best-effort activity: the last tool call the child hub saw.
        let activity = child_hub.snapshot().iter().rev().find_map(|e| match e {
            AgentEvent::ToolCallStart { title, .. } => Some(title.clone()),
            _ => None,
        });

        parent_hub.publish(AgentEvent::SubagentUpdated {
            run_id: run_id.to_string(),
            agent_id: agent_id.to_string(),
            parent_call_id: parent_call_id.to_string(),
            status: SubagentRunStatus::from_run_status(status),
            execution_epoch: u64::try_from(epoch).unwrap_or(u64::MAX),
            description: description.to_string(),
            activity,
        });

        // Done watch first (waiters wake and re-read the settled row), then
        // drop the in-memory entries — a terminal child's hub must not be
        // kept alive by the manager.
        let _ = done_tx.send(true);
        self.children.remove(run_id);
        self.by_agent.remove(agent_id);
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

    /// Cancel one agent's active run (the subagent cancel endpoint). Also
    /// returns the child when its run row is `lost` (not active) so callers
    /// can distinguish "cancelled live" from "persisted a lost run as
    /// cancelled" — see the API layer.
    pub fn cancel(&self, agent_id: &str) -> Option<Arc<ChildRun>> {
        let child = self.child_by_agent(agent_id)?;
        child.cancel.cancel();
        Some(child)
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
    e.downcast_ref::<sqlx::Error>()
        .is_some_and(|db_err| matches!(db_err.as_database_error(), Some(db) if db.is_unique_violation()))
}

// ---------------------------------------------------------------------------
// Registration transaction
// ---------------------------------------------------------------------------

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
) -> Result<(run_db::AgentRun, String)> {
    let mut tx = pool.begin().await?;
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
        Some(&spawn.provider_id),
        Some(&spawn.model_id),
        None,
    )
    .await?;
    let run = run_db::create_run_in_tx(
        &mut tx,
        run_db::NewRun {
            conversation_id: conversation.id.clone(),
            kind: "subagent",
            parent_run_id: Some(spawn.parent_run_id.clone()),
            root_run_id: Some(spawn.root_run_id.clone()),
            parent_tool_call_id: Some(parent_call_id.to_string()),
            provider_id: Some(spawn.provider_id.clone()),
            model_id: Some(spawn.model_id.clone()),
            context_window: spawn.context_window.map(i64::from),
            description: Some(description.to_string()),
        },
    )
    .await?;
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
        .ok_or_else(|| anyhow!("child conversation {} has no delegation message", run.conversation_id))
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
    tokio::time::timeout(timeout, async move {
        let _ = done.wait_for(|d| *d).await;
    })
    .await
    .is_ok()
}
