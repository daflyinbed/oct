//! Application-layer run lifecycle (ADR 0002 §6.1/§6.2): the RunService owns
//! conversation occupancy, run-row persistence, model/tool assembly, loop
//! startup and terminal-state settlement — the pieces the HTTP chat handlers
//! used to stitch together by hand. The API layer keeps only its
//! HTTP-specific concerns (spec resolution, repair, user-message insertion,
//! the manual compaction transaction) and hands the resolved pieces to
//! [`RunService::start_root_run`]; the `task` tool starts subagent runs
//! through this same service (via [`crate::subagent::SubagentManager`])
//! instead of calling REST handlers.
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
//!
//! Child (subagent) runs ride the same machinery through
//! [`RunService::start_child_run`]: their occupancy entry holds the child
//! hub's strong `Arc`, their cancellation token is a `child_token()` of the
//! parent's, and their supervisor settles through the SubagentManager
//! (epoch-guarded `finish_run` + `subagent_updated` on the parent hub +
//! done-watch + memory cleanup) before dropping the guard.

/// DB-driven model resolution (the single ChatModel construction path).
mod resolve;

pub mod background;

use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::{Arc, Weak};
use std::time::Duration;

use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use dashmap::DashMap;
use futures_util::StreamExt;
use sqlx::SqlitePool;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use oct_llm_provider::model::ChatModel;
use oct_llm_provider::provider::ProviderRegistry;

use crate::agent::loop_runner::run_agent_loop;
use crate::agent::prompt::{system_prompt, worker_system_prompt};
use crate::agent::{
    AgentContext, AgentEvent, EventHub, RunHandle, RunOutcome, RunStatus, SubagentRunStatus,
};
use crate::api::error::{ApiResult, AppError};
use crate::context::LlmView;
use crate::db::projects as project_db;
use crate::db::runs as run_db;
use crate::mcp::McpManager;
use crate::permissions::evaluator::PermissionMode;
use crate::permissions::gate::{PermissionGate, publish_cancelled_approvals};
use crate::permissions::registry::ApprovalRegistry;
use crate::permissions::rules::{Effect, Rule, RuleSet};
use crate::subagent::{ChildHandle, ChildRunRequest, SubagentManager, TaskSpawnConfig, TaskTool};
use crate::tools;
use crate::tools::shared::{AgentReadState, WorkspaceFileLocks};
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
    /// Per-project workspace file locks (ADR 0002 §5.3): every run of a
    /// project — root and subagent alike — shares the same lock set so
    /// cross-agent writes to one file serialize. Keyed by project id.
    pub file_locks: DashMap<String, Arc<WorkspaceFileLocks>>,
    /// Subagent registry/lifecycle (two-phase wired: the manager holds this
    /// service through a `OnceLock`, set right after construction — see
    /// `main.rs` / the test harness).
    pub subagents: Arc<SubagentManager>,
    /// Background bash task registry/lifecycle (ADR 0003): same two-phase
    /// wiring — the manager starts wake runs through this service.
    pub background: Arc<background::BackgroundTaskManager>,
    /// Pending tool-approval registry (ADR 0004 §4): every gate of this
    /// service parks its Ask arms here; one registry process-wide is all
    /// the decide endpoint needs.
    pub approvals: Arc<ApprovalRegistry>,
    /// Live permission gates by conversation id (ADR 0004 §1.8): weak so a
    /// settled run's gate dies with its loop; `allow_always` decisions and
    /// mode PATCHes reach the ACTIVE runs of a conversation through here.
    pub gates: Arc<DashMap<String, Vec<Weak<PermissionGate>>>>,
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
    /// Whether to attach the `task` delegation tool: root runs of kind
    /// `chat` conversations get it; resumed roots included. Workers never
    /// do (depth <= 1 is enforced server-side on top).
    pub attach_task_tool: bool,
}

/// A successfully launched run: the SSE response body to return from the
/// HTTP handler, plus the persisted run id.
pub struct StartedRun {
    pub response: Response,
    pub run_id: String,
}

/// The roots (and their additional-dirs shadow) of a project's workspace,
/// re-resolved from the project row on every run so directory edits take
/// effect on the next turn.
pub(crate) fn workspace_roots(project: &project_db::Project) -> (WorkspaceRoots, Vec<PathBuf>) {
    let additional: Vec<PathBuf> = project
        .additional_dirs
        .0
        .iter()
        .map(PathBuf::from)
        .collect();
    let roots = WorkspaceRoots::new(PathBuf::from(&project.working_dir), additional.clone());
    (roots, additional)
}

impl RunService {
    /// Insert the session entry for a new run: a REAL handle with a fresh
    /// event hub (not a placeholder channel) and a fresh cancellation token.
    /// A GET /events arriving in the run-startup window already replays from
    /// the same hub this run will publish to, so no handle swap is ever
    /// needed. Returns the occupancy guard whose drop releases the entry
    /// again.
    pub fn occupy(&self, id: &str) -> Result<OccupancyGuard, AppError> {
        self.occupy_with(id, CancellationToken::new())
    }

    /// [`RunService::occupy`] with a caller-supplied cancellation token:
    /// subagent runs pass a `child_token()` of the parent's (cancelling the
    /// parent cascades), and the resume orchestrator passes the token its
    /// SSE-visible session entry already carries so a user cancel reaches
    /// the reconstructed children before the parent loop even starts.
    pub fn occupy_with(
        &self,
        id: &str,
        cancel: CancellationToken,
    ) -> Result<OccupancyGuard, AppError> {
        match self.sessions.entry(id.to_string()) {
            dashmap::mapref::entry::Entry::Occupied(_) => Err(AppError::SessionBusy(
                "Agent is already running for this conversation".into(),
            )),
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                entry.insert(RunHandle {
                    hub: Arc::new(EventHub::new()),
                    cancel,
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

    /// The project's shared workspace file locks, created on first use.
    /// Every run of the project — root and subagent — receives the same
    /// `Arc`, serializing same-file writes across the whole task tree.
    pub fn file_locks_for(&self, project_id: &str) -> Arc<WorkspaceFileLocks> {
        self.file_locks
            .entry(project_id.to_string())
            .or_insert_with(|| Arc::new(WorkspaceFileLocks::new()))
            .value()
            .clone()
    }

    // --- Permission gate assembly (ADR 0004 §1.8) --------------------------

    /// Assemble one run's permission gate: the project's rules (project +
    /// global rows) loaded once into memory, the conversation mode, and the
    /// built-in exec-root deny anchored at the SAME directory the
    /// background manager writes to. Subagent runs pass their ROOT
    /// conversation's id as `mode_conversation_id` (child rows carry no
    /// effective mode, §1.6) and `subagent = true` (Ask → Deny).
    ///
    /// Best-effort by design: a rules/mode read failure degrades to an
    /// empty rule set / the default mode rather than failing the run — the
    /// gate's fail-closed defaults still hold.
    pub(crate) async fn assemble_gate(
        &self,
        conversation_id: &str,
        mode_conversation_id: &str,
        run_id: &str,
        project_id: &str,
        roots: &WorkspaceRoots,
        subagent: bool,
    ) -> Arc<PermissionGate> {
        let mut rules = RuleSet::new(self.background.exec_dir());
        match crate::db::permissions::list_rules_for_project(&self.pool, project_id).await {
            Ok(rows) => {
                for row in &rows {
                    // CHECK-constrained columns; an unparsable value (a
                    // hand-edited DB) just drops the row.
                    if let Some(effect) = Effect::from_str_opt(&row.effect) {
                        rules.add(Rule {
                            effect,
                            tool: row.tool.clone(),
                            pattern: row.pattern.clone(),
                        });
                    }
                }
            }
            Err(e) => tracing::warn!("loading permission rules for run {run_id} failed: {e}"),
        }
        let mode = crate::db::conversations::get_conversation(&self.pool, mode_conversation_id)
            .await
            .ok()
            .flatten()
            .and_then(|conv| PermissionMode::from_str_opt(&conv.permission_mode))
            .unwrap_or(PermissionMode::Default);

        let gate = PermissionGate::new(
            conversation_id.to_string(),
            run_id.to_string(),
            roots.clone(),
            rules,
            mode,
            subagent,
            self.approvals.clone(),
        );
        self.register_gate(conversation_id, &gate);
        // Subagent gates also register under their ROOT conversation: a
        // mode PATCH on the root reaches the workers whose mode it is, and
        // a root `allow_always` covers the whole live task tree.
        if subagent && mode_conversation_id != conversation_id {
            self.register_gate(mode_conversation_id, &gate);
        }
        gate
    }

    /// Record a gate under a conversation key, pruning references whose
    /// runs have settled (weak by design — the gate dies with its loop).
    fn register_gate(&self, conversation_id: &str, gate: &Arc<PermissionGate>) {
        let mut entry = self
            .gates
            .entry(conversation_id.to_string())
            .or_default();
        entry.retain(|weak| weak.strong_count() > 0);
        entry.push(Arc::downgrade(gate));
    }

    /// The conversation's currently live gates (root run plus any live
    /// subagent runs of its tree).
    pub fn active_gates(&self, conversation_id: &str) -> Vec<Arc<PermissionGate>> {
        match self.gates.get(conversation_id) {
            Some(gates) => gates.iter().filter_map(Weak::upgrade).collect(),
            None => Vec::new(),
        }
    }

    /// Live mode switch for every active gate of a conversation (the PATCH
    /// endpoint; §1.5's per-call immediate effect). Subagent gates are
    /// registered under the root id too, so the whole tree follows.
    pub fn set_conversation_permission_mode(&self, conversation_id: &str, mode: PermissionMode) {
        for gate in self.active_gates(conversation_id) {
            gate.set_mode(mode);
        }
    }

    /// Resolve `provider_id:model_id` into a live `ChatModel` (moved here
    /// from the API layer so subagents and the recovery orchestrator resolve
    /// exactly like a chat turn does). Errors map 404/400 as before.
    ///
    /// Every call re-reads the provider row, so the DB configuration drives
    /// each request afresh — a settings change takes effect on the next
    /// run. Construction rules (adapter dispatch, DB-over-env fallback
    /// order) live in [`resolve`].
    pub async fn resolve_model(
        &self,
        provider_spec: &str,
    ) -> Result<(Box<dyn ChatModel>, String, String), AppError> {
        let (provider_id, model_id) = provider_spec.split_once(':').ok_or_else(|| {
            AppError::BadRequest(format!(
                "Invalid provider_spec format: '{}'. Expected 'provider_id:model_id'",
                provider_spec
            ))
        })?;

        let provider = crate::db::providers::get_provider(&self.pool, provider_id)
            .await
            .map_err(AppError::from)?
            .ok_or_else(|| AppError::NotFound(format!("Provider '{}' not found", provider_id)))?;

        let model = resolve::resolve_chat_model(&self.registry, &provider, provider_id, model_id)?;

        Ok((model, provider_id.to_string(), model_id.to_string()))
    }

    /// Context window for compaction decisions: built-in provider info first
    /// (Anthropic/Moonshot carry their limits in code), then the models
    /// table's `limit_context` (seeded from models.dev or set on custom
    /// models). `None` means unknown — auto compaction stays disabled,
    /// manual still works.
    pub async fn resolve_context_window(
        &self,
        model: &dyn ChatModel,
        provider_id: &str,
        model_id: &str,
    ) -> Option<u32> {
        if let Some(window) = model.info().limits.max_input_tokens.filter(|w| *w > 0) {
            return Some(window);
        }
        let row = crate::db::models::get_model(&self.pool, provider_id, model_id)
            .await
            .ok()
            .flatten()?;
        row.limit_context
            .and_then(|v| u32::try_from(v).ok())
            .filter(|w| *w > 0)
    }

    /// Create the run's persistence row, subscribe to its event hub,
    /// assemble the agent context, and hand the whole run to the supervisor.
    /// The SSE response stream is live from the very first event. On any
    /// failure the consumed guard simply drops, releasing the conversation.
    pub async fn start_root_run(&self, req: RootRunRequest) -> ApiResult<StartedRun> {
        // Read the session entry the guard guarantees and subscribe to its
        // hub BEFORE spawning so the response stream is live from the very
        // first event (the hub log would replay them anyway — subscribing
        // first is just the cheaper path).
        let handle = self
            .sessions
            .get(&req.conversation_id)
            .map(|entry| entry.value().clone())
            .expect("the occupancy guard guarantees the session entry");
        let stream = handle
            .hub
            .subscribe()
            .map(|e| Ok::<_, Infallible>(sse_event(&e)));

        let run_id = self.run_root_with_guard(req).await?;

        Ok(StartedRun {
            response: Sse::new(stream)
                .keep_alive(KeepAlive::default())
                .into_response(),
            run_id,
        })
    }

    /// The supervised launch itself, minus the SSE subscription: creates the
    /// run row, assembles the context (attaching the `task` tool when asked)
    /// and spawns the supervisor with the caller's guard. This is the entry
    /// the resume-recovery orchestrator uses — it already holds the guard
    /// and its hub has live subscribers, so it must NOT re-occupy.
    pub async fn run_root_with_guard(&self, req: RootRunRequest) -> ApiResult<String> {
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
            attach_task_tool,
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
                provider_id: Some(provider_id.clone()),
                model_id: Some(model_id.clone()),
                context_window: context_window.map(i64::from),
                description: None,
            },
        )
        .await
        .map_err(AppError::from)?;

        let handle = self
            .sessions
            .get(&conversation_id)
            .map(|entry| entry.value().clone())
            .expect("the occupancy guard guarantees the session entry");

        let (roots, additional) = workspace_roots(&project);
        let locks = self.file_locks_for(&project.id);
        let reads = Arc::new(AgentReadState::new());

        // Default workspace tools plus every connected MCP server's tools that
        // are in scope for THIS project (global + project-scoped; the LLM sees
        // one flat toolset; MCP names are the mcp__{server}__{tool} forms
        // deduplicated across servers). The file tools share the project's
        // lock set so subagents serialize with this run on the same paths.
        let mut run_tools: Vec<Arc<dyn tools::AgentTool>> =
            tools::default_tools_with(roots.clone(), locks.clone(), reads)
                .into_iter()
                .map(Arc::from)
                .collect();
        run_tools.extend(
            self.mcp
                .tools_for_run(Some(&project.id))
                .into_iter()
                .map(Arc::from),
        );

        // Background execution surface (ADR 0003): the default set's plain
        // execute tool is swapped in place for a background-enabled instance
        // carrying this run's identity, and the two companion tools join the
        // set. The swap is positional (schema order is otherwise unchanged).
        for tool in run_tools.iter_mut() {
            if tool.name() == "execute_command" {
                *tool = Arc::new(tools::execute::ExecuteCommandTool::with_background(
                    roots.clone(),
                    tools::execute::ExecuteBackground {
                        manager: self.background.clone(),
                        conversation_id: conversation_id.clone(),
                        parent_run_id: run.id.clone(),
                    },
                ));
            }
        }
        run_tools.push(Arc::new(tools::task_output::TaskOutputTool::new(
            self.background.clone(),
            conversation_id.clone(),
        )) as Arc<dyn tools::AgentTool>);
        run_tools.push(Arc::new(tools::task_stop::TaskStopTool::new(
            self.background.clone(),
            conversation_id.clone(),
        )) as Arc<dyn tools::AgentTool>);

        // The delegation tool for root runs of chat conversations: it hands
        // the SubagentManager everything it needs to spawn a worker —
        // including (through the OnceLock filled right below) the very
        // toolset built here, which children re-derive as parent-minus-task.
        // The slot pattern exists because the TaskTool is PART of the set it
        // must reference: the lock is filled only after the full Arc'd set
        // (task included) exists, and every read happens at execute time,
        // long after.
        let parent_toolset: crate::subagent::ToolsetSlot = Arc::new(std::sync::OnceLock::new());
        if attach_task_tool {
            let task: Arc<dyn tools::AgentTool> = Arc::new(TaskTool::new(
                self.subagents.clone(),
                Arc::new(TaskSpawnConfig {
                    parent_conversation_id: conversation_id.clone(),
                    parent_run_id: run.id.clone(),
                    root_conversation_id: conversation_id.clone(),
                    root_run_id: run.id.clone(),
                    project: project.clone(),
                    roots: roots.clone(),
                    locks: locks.clone(),
                    provider_id: provider_id.clone(),
                    model_id: model_id.clone(),
                    context_window,
                    parent_cancel: handle.cancel.clone(),
                    parent_hub: handle.hub.clone(),
                    parent_toolset: parent_toolset.clone(),
                }),
            ));
            run_tools.push(task);
        }

        let toolset = Arc::new(run_tools);
        if attach_task_tool {
            // Fill the slot now that the full set (task included) exists —
            // as a WEAK reference, so the slot never keeps the toolset (and
            // the run's hub, via the task tool's config) alive past the run
            // (see TaskSpawnConfig::parent_toolset). Children exclude `task`
            // by name when deriving their own sets.
            let _ = parent_toolset.set(Arc::downgrade(&toolset));
        }

        // The run's permission gate (ADR 0004 §1.8): project rules + this
        // conversation's mode, loaded once; `allow_always` decisions inject
        // into it live through the service's gate index.
        let gate = self
            .assemble_gate(
                &conversation_id,
                &conversation_id,
                &run.id,
                &project.id,
                &roots,
                false,
            )
            .await;

        let ctx = AgentContext {
            model: Arc::from(model),
            tools: toolset.clone(),
            pool: self.pool.clone(),
            // Built from the ACTUAL tool list (task guidance included for
            // root agents; see the prompt module).
            system_prompt: system_prompt(
                &PathBuf::from(&project.working_dir),
                &additional,
                &toolset,
            ),
            context_window,
            idle,
            permission: gate,
        };

        let run_id = run.id.clone();
        let pool = self.pool.clone();
        let approvals = self.approvals.clone();
        tokio::spawn(async move {
            supervise(pool, run_id, conversation_id, approvals, guard, ctx, view, handle).await;
        });

        Ok(run.id)
    }

    /// Launch a subagent run over its (already-registered and occupied)
    /// conversation: same supervisor pattern as a root run, but settlement
    /// goes through the SubagentManager (epoch-guarded finish + parent-hub
    /// event + done watch + memory cleanup), and the guard's drop releases
    /// the child's occupancy. The child toolset is derived here — from the
    /// parent's live toolset when dispatched through the `task` tool, or
    /// rebuilt from the same assembly sources (`default_tools_with` +
    /// `mcp.tools_for_run`) during recovery, where the crashed parent's
    /// toolset is gone; both equal parent-minus-task in effect.
    pub async fn start_child_run(&self, req: ChildRunRequest) -> ApiResult<ChildHandle> {
        let ChildRunRequest {
            guard,
            run_id,
            epoch,
            conversation_id,
            start_message_id,
            view,
            context_window,
            cancel,
            parent_hub,
            parent_call_id,
            description,
            roots,
            locks,
            parent_toolset,
            project,
            model,
            // The provider/model ids already live on the run row's snapshot;
            // the child context itself records them via its history rows.
            provider_id: _,
            model_id: _,
        } = req;

        self.set_start_message_id(&conversation_id, &start_message_id);
        let handle = self
            .sessions
            .get(&conversation_id)
            .map(|entry| entry.value().clone())
            .expect("the occupancy guard guarantees the session entry");

        // Effective toolset = parent set − task, with the three file tools
        // rebuilt over the shared locks and a FRESH read state (the child's
        // read records must be its own; stateless builtins and MCP tools are
        // reused by Arc so connections stay shared). Recovery passes
        // `parent_toolset = None`: the parent's instances died with the
        // crashed process, so the set is rebuilt from the same assembly
        // sources — which yields the same effective set.
        let child_tools: Vec<Arc<dyn tools::AgentTool>> = match parent_toolset {
            Some(parent) => derive_child_toolset(&parent, roots.clone(), locks.clone()),
            None => {
                let mut rebuilt: Vec<Arc<dyn tools::AgentTool>> = tools::file_tools_with(
                    roots.clone(),
                    locks.clone(),
                    Arc::new(AgentReadState::new()),
                )
                .into_iter()
                .map(Arc::from)
                .collect();
                rebuilt.extend(
                    self.mcp
                        .tools_for_run(Some(&project.id))
                        .into_iter()
                        .map(Arc::from),
                );
                rebuilt
            }
        };
        let child_tools = Arc::new(child_tools);

        // The child's permission gate (ADR 0004 §1.6/§1.8): rules load like
        // a root run's, the MODE inherits the root conversation (child rows
        // carry no effective mode — recovery included, since this assembly
        // is the recovery path too), and Ask is downgraded to Deny because
        // a worker has no interactive surface.
        let root_conversation_id = crate::db::conversations::get_conversation(
            &self.pool,
            &conversation_id,
        )
        .await
        .ok()
        .flatten()
        .and_then(|conv| conv.root_conversation_id)
        .unwrap_or_else(|| conversation_id.clone());
        let gate = self
            .assemble_gate(
                &conversation_id,
                &root_conversation_id,
                &run_id,
                &project.id,
                &roots,
                true,
            )
            .await;

        let (_, additional) = workspace_roots(&project);
        let ctx = AgentContext {
            model: Arc::from(model),
            tools: child_tools.clone(),
            pool: self.pool.clone(),
            system_prompt: worker_system_prompt(
                &PathBuf::from(&project.working_dir),
                &additional,
                &child_tools,
            ),
            context_window,
            idle: None,
            permission: gate,
        };

        // Publish the running transition on the PARENT hub (small events
        // only — the child's full stream stays on its own hub; ADR 0002
        // §7.3).
        let agent_id = conversation_id.clone();
        parent_hub.publish(AgentEvent::SubagentUpdated {
            run_id: run_id.clone(),
            agent_id: agent_id.clone(),
            parent_call_id: parent_call_id.clone(),
            status: SubagentRunStatus::Running,
            execution_epoch: u64::try_from(epoch).unwrap_or(u64::MAX),
            description: description.clone(),
            activity: None,
        });

        let (done_tx, done_rx) = watch::channel(false);
        self.subagents.register_child(crate::subagent::ChildRun {
            run_id: run_id.clone(),
            agent_id: agent_id.clone(),
            description: description.clone(),
            cancel: cancel.clone(),
            done: done_rx.clone(),
            hub: Arc::downgrade(&handle.hub),
            epoch,
        });

        let pool = self.pool.clone();
        let approvals = self.approvals.clone();
        let subagents = self.subagents.clone();
        let hub = handle.hub.clone();
        let run_id2 = run_id.clone();
        let agent_id2 = agent_id.clone();
        let parent_call_id2 = parent_call_id.clone();
        let description2 = description.clone();
        let conv = conversation_id.clone();
        tokio::spawn(async move {
            supervise_child(
                pool,
                approvals,
                subagents,
                run_id2,
                conv,
                agent_id2,
                description2,
                epoch,
                parent_call_id2,
                guard,
                ctx,
                view,
                handle,
                parent_hub,
                done_tx,
            )
            .await;
        });

        Ok(ChildHandle {
            run_id,
            agent_id,
            done: done_rx,
            cancel,
            hub,
        })
    }
}

/// Derive a worker's toolset from the parent's live set (ADR 0002 §5.2):
/// parent set − `task`, with `read_file`/`write_file`/`edit_file` rebuilt
/// over the shared locks and a fresh read state. Everything else (stateless
/// builtins, MCP tools) is Arc-cloned so connections and configs are reused
/// verbatim — schema AND dispatch stay exactly parent-minus-task.
///
/// `execute_command` is likewise rebuilt (background gate closed, schema
/// identical): subagent workers must not detach commands — the wake-run
/// semantics only hold in the root conversation (ADR 0003). The background
/// companions (`task_output`/`task_stop`) are scoped to the root
/// conversation and simply dropped.
pub(crate) fn derive_child_toolset(
    parent: &[Arc<dyn tools::AgentTool>],
    roots: WorkspaceRoots,
    locks: Arc<WorkspaceFileLocks>,
) -> Vec<Arc<dyn tools::AgentTool>> {
    let mut child: Vec<Arc<dyn tools::AgentTool>> =
        tools::file_tools_with(roots.clone(), locks, Arc::new(AgentReadState::new()))
            .into_iter()
            .map(Arc::from)
            .collect();
    child.push(Arc::new(tools::execute::ExecuteCommandTool::new(roots)) as Arc<dyn tools::AgentTool>);
    const NOT_REUSED: [&str; 7] = [
        "task",
        "read_file",
        "write_file",
        "edit_file",
        "execute_command",
        "task_output",
        "task_stop",
    ];
    child.extend(
        parent
            .iter()
            .filter(|t| !NOT_REUSED.contains(&t.name()))
            .cloned(),
    );
    child
}

/// The run supervisor (ADR 0002 §6.1): await the loop, persist the terminal
/// state, then release the conversation's occupancy — in that order, so a
/// new run can never start before the previous one's outcome is on disk.
async fn supervise(
    pool: SqlitePool,
    run_id: String,
    conv_id: String,
    approvals: Arc<ApprovalRegistry>,
    guard: OccupancyGuard,
    ctx: AgentContext,
    view: LlmView,
    handle: RunHandle,
) {
    // The loop runs in its OWN task: a panic inside it resolves this await
    // as a JoinError and becomes a Failed outcome here, instead of unwinding
    // past the supervisor and skipping both the terminal write and the
    // occupancy release. The hub is cloned out first — the approval sweep
    // below publishes on it after the loop (and with it `handle`) is gone.
    let hub = handle.hub.clone();
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
    // cannot await). The `status IN ('running','lost')` guard in the UPDATE
    // makes the settlement one-shot; a failure to persist is logged but must
    // not keep the conversation occupied forever.
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
        outcome
            .usage
            .as_ref()
            .map(|u| i64::try_from(u.input_tokens).unwrap_or(i64::MAX)),
        outcome
            .usage
            .as_ref()
            .map(|u| i64::try_from(u.output_tokens).unwrap_or(i64::MAX)),
        // Root runs never resume their own row (recovery starts a NEW root
        // run), so no epoch guard is needed here.
        None,
    )
    .await
    {
        tracing::error!("failed to persist terminal state of run {run_id}: {e}");
    }

    // Pending-approval cleanup (ADR 0004 §4's guard duty): whatever the
    // gate futures could not settle themselves (a cancel drops them
    // mid-await) resolves here — published while the hub is still alive so
    // subscribers see the closed cards before the stream ends.
    publish_cancelled_approvals(approvals.as_ref(), &run_id, &hub);

    // Release occupancy LAST: dropping the guard removes the session entry,
    // the last hub `Arc` goes away, and every SSE stream reaches EOF — the
    // same termination semantics the pre-RunService inline cleanup had.
    drop(guard);
}

/// The subagent-run supervisor: same shape as [`supervise`], but the
/// settlement goes through [`SubagentManager::settle`] (epoch-guarded
/// finish + `subagent_updated` on the parent hub + done watch + memory
/// cleanup) and runs BEFORE the guard drop, so a watcher released by the
/// watch always finds the terminal row in the DB.
#[allow(clippy::too_many_arguments)]
async fn supervise_child(
    pool: SqlitePool,
    approvals: Arc<ApprovalRegistry>,
    subagents: Arc<SubagentManager>,
    run_id: String,
    conv_id: String,
    agent_id: String,
    description: String,
    epoch: i64,
    parent_call_id: String,
    guard: OccupancyGuard,
    ctx: AgentContext,
    view: LlmView,
    handle: RunHandle,
    parent_hub: Arc<EventHub>,
    done_tx: watch::Sender<bool>,
) {
    let outcome =
        match tokio::spawn(run_agent_loop(ctx, conv_id.clone(), view, handle.clone())).await {
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

    // Same pending-approval sweep as the root supervisor — uniformity over
    // the (currently unreachable) subagent Ask arm: children downgrade Ask
    // to Deny, but the sweep is what makes that structurally guaranteed.
    publish_cancelled_approvals(approvals.as_ref(), &run_id, &handle.hub);

    subagents
        .settle(
            &pool,
            &run_id,
            &agent_id,
            &description,
            epoch,
            &parent_call_id,
            &outcome,
            &parent_hub,
            &handle.hub,
            &done_tx,
        )
        .await;

    // Release the child's occupancy LAST (settlement is already on disk, the
    // done watch is closed): the session entry goes away, the last strong
    // `Arc` to the child hub drops, and the child's SSE streams reach EOF.
    drop(guard);
}

/// Server-startup recovery (ADR 0002 §6.2): `agent_runs` rows a previous
/// process left `running` are marked `lost` — the executing instance is
/// gone, while the persisted history and the conversation's resume
/// eligibility stay untouched. The same sweep covers `background_tasks`
/// rows (ADR 0003): PDEATHSIG already terminated the commands when the
/// process died; their notifications are delivered by the send/resume
/// catch-up. Call once per process, before traffic.
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
    match background::task_sweep(pool).await {
        Ok(stale) => {
            for task in &stale {
                tracing::warn!(
                    task_id = %task.id,
                    conversation_id = %task.conversation_id,
                    "marking background task left running by a previous process as lost"
                );
            }
        }
        Err(e) => tracing::error!("failed to sweep interrupted background tasks: {e}"),
    }
}
