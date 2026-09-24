//! Application-layer run lifecycle (ADR 0002 §6.1/§6.2): the RunService owns
//! conversation occupancy, run-row persistence, model/tool assembly, loop
//! startup and terminal-state settlement — the pieces the HTTP chat handlers
//! used to stitch together by hand. The API layer keeps only its
//! HTTP-specific concerns (spec resolution, repair, user-message insertion,
//! the manual compaction transaction) and hands the resolved pieces to
//! [`RunService::start_root_run`]; later PRs will let the `task` tool start
//! subagent runs through this same service instead of calling REST handlers.
//!
//! Lifecycle of a root run:
//!
//! 1. [`RunService::occupy`] takes the conversation's session entry (the
//!    occupancy mutex shared by send/resume/compact) and returns an
//!    [`OccupancyGuard`] — every error path after this point simply drops
//!    the guard, so occupancy can never leak.
//! 2. [`RunService::start_root_run`] creates the `agent_runs` row, subscribes
//!    to the run's event hub, assembles the agent context, and spawns the
//!    supervisor. The guard moves into the supervisor, so occupancy lasts
//!    the whole run.
//! 3. The supervisor awaits the loop (in its own task, so a panic becomes a
//!    Failed outcome instead of skipping cleanup), persists the terminal
//!    state — an async write done here, never in `Drop` — and releases the
//!    guard last: the session entry goes away, the last hub `Arc` drops, and
//!    every SSE stream reaches EOF.

use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use dashmap::DashMap;
use futures_util::StreamExt;
use sqlx::SqlitePool;
use tokio_util::sync::CancellationToken;

use oct_llm_provider::model::ChatModel;
use oct_llm_provider::provider::ProviderRegistry;

use crate::agent::loop_runner::run_agent_loop;
use crate::agent::prompt::system_prompt;
use crate::agent::{AgentContext, AgentEvent, EventHub, RunHandle, RunOutcome, RunStatus};
use crate::api::error::{ApiResult, AppError};
use crate::context::LlmView;
use crate::db::projects as project_db;
use crate::db::runs as run_db;
use crate::mcp::McpManager;
use crate::tools;
use crate::tools::workspace::WorkspaceRoots;

/// Serialize an AgentEvent as an SSE `data:` payload.
pub(crate) fn sse_event(e: &AgentEvent) -> Event {
    Event::default()
        .json_data(e)
        .unwrap_or_else(|_| Event::default())
}

/// Owns a conversation's occupancy for as long as it lives: dropping it
/// removes the session entry (and with it the run's event hub — the last
/// `Arc` goes away and every SSE stream attached to it reaches EOF).
/// Returned by [`RunService::occupy`]; moving it into
/// [`RunService::start_root_run`] keeps the conversation locked for the
/// whole run while every early error path releases it simply by returning.
pub struct OccupancyGuard {
    sessions: Arc<DashMap<String, RunHandle>>,
    conv_id: String,
}

impl Drop for OccupancyGuard {
    fn drop(&mut self) {
        self.sessions.remove(&self.conv_id);
    }
}

/// Application-layer run orchestration: conversation occupancy, agent_runs
/// persistence, model/tool assembly and loop supervision (ADR 0002 §6.1).
/// Holds clones of the same `Arc`s `AppState` holds — the `sessions` map in
/// particular is one shared instance so the reconnect/status endpoints keep
/// observing the same occupancy.
pub struct RunService {
    pub pool: SqlitePool,
    pub registry: Arc<ProviderRegistry>,
    pub mcp: Arc<McpManager>,
    /// Same instance as `AppState.sessions`: the conversation occupancy map
    /// shared by send/resume/compact and every SSE reconnect reader.
    pub sessions: Arc<DashMap<String, RunHandle>>,
}

/// Everything [`RunService::start_root_run`] needs to launch one root run.
/// The HTTP layer resolves the model/window/idle and performs its own
/// ordering-sensitive writes (repair, user-message insert) BEFORE handing
/// the guard over — from here on the run is supervised end to end.
pub struct RootRunRequest {
    /// Consumed: moved into the supervisor so occupancy lasts the whole run
    /// (and auto-releases on any pre-spawn failure inside the service).
    pub guard: OccupancyGuard,
    pub conversation_id: String,
    pub project: project_db::Project,
    pub model: Box<dyn ChatModel>,
    pub provider_id: String,
    pub model_id: String,
    pub view: LlmView,
    pub context_window: Option<u32>,
    /// How long the conversation sat idle BEFORE this run started — the
    /// microcompact idle trigger.
    pub idle: Option<Duration>,
}

/// A successfully launched run: the SSE response body to return from the
/// HTTP handler, plus the persisted run id.
pub struct StartedRun {
    pub response: Response,
    pub run_id: String,
}

impl RunService {
    /// Insert the session entry for a new run: a REAL handle with a fresh
    /// event hub (not a placeholder channel). A GET /events arriving in the
    /// run-startup window already replays from the same hub this run will
    /// publish to, so no handle swap is ever needed. Returns the occupancy
    /// guard whose drop releases the entry again.
    pub fn occupy(&self, id: &str) -> Result<OccupancyGuard, AppError> {
        match self.sessions.entry(id.to_string()) {
            dashmap::mapref::entry::Entry::Occupied(_) => Err(AppError::SessionBusy(
                "Agent is already running for this conversation".into(),
            )),
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                entry.insert(RunHandle {
                    hub: Arc::new(EventHub::new()),
                    cancel: CancellationToken::new(),
                    start_message_id: None,
                });
                Ok(OccupancyGuard {
                    sessions: self.sessions.clone(),
                    conv_id: id.to_string(),
                })
            }
        }
    }

    /// Record the run boundary on the (already live) session entry: the user
    /// message row is the divide between DB history and this run's event
    /// log. GET /events reports it as run_meta so a reconnecting frontend
    /// truncates its history exactly here before replaying the run's events.
    pub fn set_start_message_id(&self, conv_id: &str, msg_id: &str) {
        if let Some(mut entry) = self.sessions.get_mut(conv_id) {
            entry.value_mut().start_message_id = Some(msg_id.to_string());
        }
    }

    /// Create the run's persistence row, subscribe to its event hub,
    /// assemble the agent context, and hand the whole run to the supervisor.
    /// The SSE response stream is live from the very first event. On any
    /// failure the consumed guard simply drops, releasing the conversation.
    pub async fn start_root_run(&self, req: RootRunRequest) -> ApiResult<StartedRun> {
        let RootRunRequest {
            guard,
            conversation_id,
            project,
            model,
            provider_id,
            model_id,
            view,
            context_window,
            idle,
        } = req;

        // The run row lands before anything is spawned: a run that never
        // settles (crash) is identifiable from persistence alone, and the
        // supervisor below always has a row to terminalize.
        let run = run_db::create_run(
            &self.pool,
            run_db::NewRun {
                conversation_id: conversation_id.clone(),
                kind: "root",
                parent_run_id: None,
                root_run_id: None,
                parent_tool_call_id: None,
                provider_id: Some(provider_id),
                model_id: Some(model_id),
                context_window: context_window.map(i64::from),
            },
        )
        .await
        .map_err(AppError::from)?;

        // Read the session entry the guard guarantees and subscribe to its
        // hub BEFORE spawning so the response stream is live from the very
        // first event (the hub log would replay them anyway — subscribing
        // first is just the cheaper path).
        let handle = self
            .sessions
            .get(&conversation_id)
            .map(|entry| entry.value().clone())
            .expect("the occupancy guard guarantees the session entry");
        let stream = handle
            .hub
            .subscribe()
            .map(|e| Ok::<_, Infallible>(sse_event(&e)));

        // Re-resolved from the project row on every send/resume, so edits to a
        // project's working directories take effect on the next turn. Additional
        // directories are accessible to the tools via absolute paths only.
        let additional: Vec<PathBuf> = project
            .additional_dirs
            .0
            .iter()
            .map(PathBuf::from)
            .collect();
        let roots = WorkspaceRoots::new(PathBuf::from(&project.working_dir), additional.clone());

        // Default workspace tools plus every connected MCP server's tools that
        // are in scope for THIS project (global + project-scoped; the LLM sees
        // one flat toolset; MCP names are the mcp__{server}__{tool} forms
        // deduplicated across servers).
        let mut run_tools = tools::default_tools(roots);
        run_tools.extend(self.mcp.tools_for_run(Some(&project.id)));

        let ctx = AgentContext {
            model: Arc::from(model),
            tools: Arc::new(run_tools),
            pool: self.pool.clone(),
            system_prompt: system_prompt(&PathBuf::from(&project.working_dir), &additional),
            context_window,
            idle,
        };

        let run_id = run.id.clone();
        let pool = self.pool.clone();
        tokio::spawn(async move {
            supervise(pool, run_id, conversation_id, guard, ctx, view, handle).await;
        });

        Ok(StartedRun {
            response: Sse::new(stream)
                .keep_alive(KeepAlive::default())
                .into_response(),
            run_id: run.id,
        })
    }
}

/// The run supervisor (ADR 0002 §6.1): await the loop, persist the terminal
/// state, then release the conversation's occupancy — in that order, so a
/// new run can never start before the previous one's outcome is on disk.
async fn supervise(
    pool: SqlitePool,
    run_id: String,
    conv_id: String,
    guard: OccupancyGuard,
    ctx: AgentContext,
    view: LlmView,
    handle: RunHandle,
) {
    // The loop runs in its OWN task: a panic inside it resolves this await
    // as a JoinError and becomes a Failed outcome here, instead of unwinding
    // past the supervisor and skipping both the terminal write and the
    // occupancy release.
    let outcome = match tokio::spawn(run_agent_loop(ctx, conv_id.clone(), view, handle)).await {
        Ok(outcome) => outcome,
        Err(join_err) => RunOutcome {
            status: RunStatus::Failed,
            final_message_id: None,
            final_text: None,
            partial: false,
            usage: None,
            error: Some(format!("agent loop panicked: {join_err}")),
        },
    };

    // The async terminal write lives here — never in Drop (a guard's Drop
    // cannot await). The `status = 'running'` guard in the UPDATE makes the
    // settlement one-shot; a failure to persist is logged but must not keep
    // the conversation occupied forever.
    let final_status = match outcome.status {
        RunStatus::Completed => run_db::RunFinalStatus::Completed,
        RunStatus::Failed => run_db::RunFinalStatus::Failed,
        RunStatus::Cancelled => run_db::RunFinalStatus::Cancelled,
    };
    if let Err(e) = run_db::finish_run(
        &pool,
        &run_id,
        final_status,
        outcome.final_message_id.as_deref(),
        outcome.error.as_deref(),
        outcome.usage.as_ref().map(|u| i64::try_from(u.input_tokens).unwrap_or(i64::MAX)),
        outcome.usage.as_ref().map(|u| i64::try_from(u.output_tokens).unwrap_or(i64::MAX)),
    )
    .await
    {
        tracing::error!("failed to persist terminal state of run {run_id}: {e}");
    }

    // Release occupancy LAST: dropping the guard removes the session entry,
    // the last hub `Arc` goes away, and every SSE stream reaches EOF — the
    // same termination semantics the pre-RunService inline cleanup had.
    drop(guard);
}

/// Server-startup recovery (ADR 0002 §6.2): `agent_runs` rows a previous
/// process left `running` are marked `lost` — the executing instance is
/// gone, while the persisted history and the conversation's resume
/// eligibility stay untouched. Call once per process, before traffic.
pub async fn bootstrap(pool: &SqlitePool) {
    match run_db::mark_stale_runs_lost(pool).await {
        Ok(stale) => {
            for run in &stale {
                tracing::warn!(
                    run_id = %run.id,
                    conversation_id = %run.conversation_id,
                    "marking agent run left running by a previous process as lost"
                );
            }
        }
        Err(e) => tracing::error!("failed to sweep interrupted agent runs: {e}"),
    }
}
