//! Compatibility facade over conversation occupancy and application launchers.
//! Root/worker assembly lives in application::assembly; runtime::execution owns
//! the common loop supervisor and guarded terminal-state persistence. The conversation application owns
//! request validation, history preparation, user-message insertion and manual
//! compaction, handing resolved pieces to [`RunService::start_root_run`].
//! HTTP adapters translate commands and encode responses; the `task` tool
//! receives a scoped delegation port backed by worker assembly and the
//! execution kernel, without retaining this facade.
//!
//! Lifecycle of a root run:
//!
//! 1. [`RunService::occupy`] takes the conversation's session entry (the
//!    occupancy mutex shared by send/resume/compact) and returns an
//!    [`OccupancyGuard`] — every error path after this point simply drops
//!    the guard, so occupancy can never leak.
//! 2. [`RunService::start_root_run`] subscribes to the run's hub and hands
//!    registration/assembly to an owned application task. Dropping its waiter
//!    cannot abandon a durable run. Cancellation reaches startup through the
//!    same token as the loop; assembly failure or panic settles the row before
//!    releasing occupancy. Ready contexts transfer their guard to supervision.
//! 3. The supervisor awaits the loop (in its own task, so a panic becomes a
//!    Failed outcome instead of skipping cleanup), persists the terminal
//!    state — an async write done here, never in `Drop` — and releases the
//!    guard last: the session entry goes away, the last hub `Arc` drops, and
//!    every SSE stream reaches EOF.
//!
//! Child (subagent) runs ride the same machinery through
//! [`RunService::start_child_run`]: their occupancy entry holds the child
//! hub's strong `Arc`, their cancellation token is a `child_token()` of the
//! parent's. The common supervisor performs the epoch-guarded terminal write;
//! a child completion observer then publishes to the parent, closes the done
//! watch and removes the active index before the guard is released.

pub(crate) mod execution;
pub(crate) mod replay;
/// DB-driven model resolution (the single ChatModel construction path).
mod resolve;
pub use resolve::ModelResolver;

pub mod background;
pub mod lifecycle;
pub mod preparation;
pub(crate) mod worker_config;

use std::path::PathBuf;
use std::sync::{Arc, Weak};
use std::time::Duration;

use dashmap::DashMap;
use sqlx::SqlitePool;
use tokio_util::sync::CancellationToken;

use oct_llm_provider::model::ChatModel;
use oct_llm_provider::provider::ProviderRegistry;

use crate::agent::repo_instructions::FrozenPromptEnv;
use crate::agent::{EventHub, RunHandle};
use crate::context::LlmView;
use crate::db::projects as project_db;
use crate::db::runs as run_db;
use crate::error::{AppError, AppResult};
use crate::mcp::McpManager;
use crate::permissions::evaluator::PermissionMode;
use crate::permissions::gate::PermissionGate;
use crate::permissions::registry::ApprovalRegistry;
use crate::subagent::{ChildHandle, ChildRunRequest, SubagentManager};
use crate::tools::shared::WorkspaceFileLocks;
use crate::tools::workspace::WorkspaceRoots;

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
/// persistence, model/tool assembly and loop supervision.
/// Holds clones of the same `Arc`s `AppState` holds — the `sessions` map in
/// particular is one shared instance so the reconnect/status endpoints keep
/// observing the same occupancy.
pub struct RunService {
    pub lifecycle: lifecycle::Lifecycle,
    pub pool: SqlitePool,
    pub registry: Arc<ProviderRegistry>,
    pub mcp: Arc<McpManager>,
    /// Same instance as `AppState.sessions`: the conversation occupancy map
    /// shared by send/resume/compact and every SSE reconnect reader.
    pub sessions: Arc<DashMap<String, RunHandle>>,
    /// Process-wide canonical-path locks, shared by root/child runs and by
    /// overlapping projects so aliases across project boundaries serialize.
    pub file_locks: Arc<DashMap<String, Arc<WorkspaceFileLocks>>>,
    /// Active child registry and guarded settlement. Delegation orchestration
    /// uses worker-only application capabilities; the index has no callback.
    pub subagents: Arc<SubagentManager>,
    /// Background process registry/lifecycle. Settlement emits a wake address
    /// consumed by the composition root's application notification drainer.
    pub background: Arc<background::BackgroundTaskManager>,
    /// Chat attachment store: run assembly grants the conversation's sent
    /// assets to the file tools as read-only files, conversation deletion
    /// sweeps it, and request assembly hydrates `oct-attachment://` references.
    pub attachments: Arc<crate::attachments::AttachmentService>,
    /// Pending tool-approval registry: every gate of this
    /// service parks its Ask arms here; one registry process-wide is all
    /// the decide endpoint needs.
    pub approvals: Arc<ApprovalRegistry>,
    /// Live permission gates by conversation id: weak so a
    /// settled run's gate dies with its loop; `allow_always` decisions and
    /// mode PATCHes reach the ACTIVE runs of a conversation through here.
    pub gates: Arc<DashMap<String, Vec<Weak<PermissionGate>>>>,
    /// Frozen prompt-head environments by ROOT conversation id: the
    /// repo-instruction block and MCP identity snapshot every run of that
    /// tree assembles from, reused byte-for-byte while clean by the shared
    /// assembly environment. `Arc` so dirty-flag handles can outlive a borrow.
    pub prompt_envs: Arc<DashMap<String, FrozenPromptEnv>>,
    /// Per-root-conversation serialization of environment resolution vs
    /// `/reload`: a reload in flight makes a concurrently STARTING run wait
    /// and then resolve the fresh entry, instead of racing the old one for
    /// its whole lifetime. Locks cover only the resolution section (never
    /// the run itself), and the guard extends to backend-initiated entry
    /// points (wake runs, recovery) that no frontend coordination reaches.
    pub prompt_env_locks: Arc<DashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

/// Everything [`RunService::start_root_run`] needs to launch one root run.
/// The application resolves the model/window/idle and performs its
/// ordering-sensitive writes (repair, user-message insert) BEFORE handing
/// the guard over — from here on the run is supervised end to end.
pub struct RootRunRequest {
    /// Consumed: moved into the supervisor so occupancy lasts the whole run
    /// (and auto-releases on any failure before supervision takes over).
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
    /// 用户为本次 run 选择的思考档位（send 请求携带；resume 从 user 消息
    /// details 恢复；wake 等内部启动为 `None`）。
    pub reasoning: Option<oct_llm_provider::core::ReasoningRequest>,
    /// Whether to attach the `task` delegation tool: root runs of kind
    /// `chat` conversations get it; resumed roots included. Workers never
    /// do (depth <= 1 is enforced server-side on top).
    pub attach_task_tool: bool,
}

/// A successfully launched run's event subscription and persisted identity.
/// The subscription receives events without owning the executing task.
pub struct StartedRun {
    pub events: crate::agent::events::SubscribeStream,
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
    pub(crate) fn assembly_services(&self) -> crate::application::assembly::AssemblyServices {
        crate::application::assembly::AssemblyServices {
            pool: self.pool.clone(),
            models: self.model_resolver(),
            mcp: self.mcp.clone(),
            exec_dir: self.background.exec_dir().to_owned(),
            sessions: self.sessions.clone(),
            file_locks: self.file_locks.clone(),
            attachments: self.attachments.clone(),
            approvals: self.approvals.clone(),
            gates: self.gates.clone(),
            prompt_envs: self.prompt_envs.clone(),
            prompt_env_locks: self.prompt_env_locks.clone(),
        }
    }

    fn codemode_root(&self) -> PathBuf {
        self.assembly_services().codemode_root()
    }

    pub fn model_resolver(&self) -> ModelResolver {
        ModelResolver::new(self.pool.clone(), self.registry.clone())
    }

    pub fn prompt_env_refresh(
        &self,
        root: &str,
    ) -> crate::agent::repo_instructions::PromptEnvRefresh {
        self.assembly_services().prompt_env_refresh(root)
    }
    pub fn mark_prompt_env_dirty(&self, root: &str) {
        self.assembly_services().mark_prompt_env_dirty(root);
    }
    pub fn drop_prompt_env(&self, root: &str) {
        self.assembly_services().drop_prompt_env(root);
    }
    pub async fn reload_prompt_env(
        &self,
        root: &str,
        project: &project_db::Project,
    ) -> FrozenPromptEnv {
        self.assembly_services()
            .reload_prompt_env(root, project)
            .await
    }

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
        self.occupancy().occupy_with(id, cancel)
    }

    fn occupy_unchecked(
        &self,
        id: &str,
        cancel: CancellationToken,
    ) -> Result<OccupancyGuard, AppError> {
        self.occupancy().occupy_unchecked(id, cancel)
    }

    pub(crate) fn occupancy(&self) -> RunOccupancy {
        RunOccupancy {
            sessions: self.sessions.clone(),
            lifecycle: self.lifecycle.clone(),
        }
    }

    pub fn delegation_service(&self) -> crate::application::delegation::DelegationService {
        crate::application::delegation::DelegationService::new(
            self.assembly_services(),
            self.occupancy(),
            self.subagents.clone(),
        )
    }

    pub(crate) async fn repair_dangling_tools(
        &self,
        conversation: &str,
        exclude: &[String],
    ) -> anyhow::Result<Vec<String>> {
        self.assembly_services()
            .repair_dangling_tools(conversation, exclude)
            .await
    }

    /// Shared canonical-path locks across root/worker runs and projects.
    pub fn file_locks_for(&self, project_id: &str) -> Arc<WorkspaceFileLocks> {
        self.assembly_services().file_locks_for(project_id)
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
    /// endpoint; takes effect on the very next tool call). Subagent gates are
    /// registered under the root id too, so the whole tree follows.
    pub fn set_conversation_permission_mode(&self, conversation_id: &str, mode: PermissionMode) {
        for gate in self.active_gates(conversation_id) {
            gate.set_mode(mode);
        }
    }

    /// Compatibility delegates to the independent DB-driven model resolver.
    pub async fn resolve_model(
        &self,
        provider_spec: &str,
    ) -> Result<(Box<dyn ChatModel>, String, String), AppError> {
        self.model_resolver().resolve_model(provider_spec).await
    }
    pub async fn resolve_context_window(
        &self,
        model: &dyn ChatModel,
        provider_id: &str,
        model_id: &str,
    ) -> Option<u32> {
        self.model_resolver()
            .resolve_context_window(model, provider_id, model_id)
            .await
    }
    pub async fn resolve_input_modalities(
        &self,
        provider_id: &str,
        model_id: &str,
    ) -> crate::modality::InputModalities {
        self.model_resolver()
            .resolve_input_modalities(provider_id, model_id)
            .await
    }

    /// Create the run's persistence row, subscribe to its event hub,
    /// assemble the agent context, and hand the whole run to the supervisor.
    /// The SSE response stream is live from the very first event. On any
    /// assembly failure the row is settled as failed before the consumed
    /// guard drops, releasing the conversation.
    pub async fn start_root_run(&self, req: RootRunRequest) -> AppResult<StartedRun> {
        // Read the session entry the guard guarantees and subscribe to its
        // hub BEFORE spawning so the response stream is live from the very
        // first event (the hub log would replay them anyway — subscribing
        // first is just the cheaper path).
        let handle = self
            .sessions
            .get(&req.conversation_id)
            .map(|entry| entry.value().clone())
            .expect("the occupancy guard guarantees the session entry");
        let stream = handle.hub.subscribe();

        let run_id = self.run_root_with_guard(req).await?;

        Ok(StartedRun {
            events: stream,
            run_id,
        })
    }

    /// The supervised launch itself, minus the SSE subscription: creates the
    /// run row, assembles the context (attaching the `task` tool when asked)
    /// and spawns the supervisor with the caller's guard. This is the entry
    /// the resume-recovery orchestrator uses — it already holds the guard
    /// and its hub has live subscribers, so it must NOT re-occupy.
    pub async fn run_root_with_guard(&self, req: RootRunRequest) -> AppResult<String> {
        crate::application::execution::launch_root(self, req).await
    }

    /// Launch a subagent run over its (already-registered and occupied)
    /// conversation: common supervision persists the epoch-guarded outcome,
    /// then a child observer publishes/cleans its index before occupancy
    /// releases. WorkerAssembler derives the child toolset from the
    /// parent's live toolset when dispatched through the `task` tool, or
    /// rebuilt from the same assembly sources (`default_tools_with` +
    /// `mcp.tools_for_run`) during recovery, where the crashed parent's
    /// toolset is gone; both equal parent-minus-task in effect.
    pub async fn start_child_run(&self, req: ChildRunRequest) -> AppResult<ChildHandle> {
        crate::application::execution::launch_child(self, req).await
    }
}

/// Server-startup recovery: `agent_runs` rows a previous
/// process left `running` are marked `lost` — the executing instance is
/// gone, while the persisted history and the conversation's resume
/// eligibility stay untouched. The same sweep covers `background_tasks`
/// rows: PDEATHSIG already terminated the commands when the
/// process died; their notifications are delivered by the send/resume
/// catch-up. Call once per process, before traffic.
pub async fn bootstrap(pool: &SqlitePool) -> anyhow::Result<()> {
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
        Err(e) => return Err(e),
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
        Err(e) => return Err(e),
    }
    Ok(())
}

/// Cloneable conversation admission without model/tool/application services.
#[derive(Clone)]
pub(crate) struct RunOccupancy {
    sessions: Arc<DashMap<String, RunHandle>>,
    lifecycle: lifecycle::Lifecycle,
}

impl RunOccupancy {
    pub(crate) fn occupy_with(
        &self,
        id: &str,
        cancel: CancellationToken,
    ) -> Result<OccupancyGuard, AppError> {
        self.lifecycle.check("conversation", id)?;
        self.occupy_unchecked(id, cancel)
    }

    fn occupy_unchecked(
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
                });
                Ok(OccupancyGuard {
                    sessions: self.sessions.clone(),
                    conv_id: id.to_string(),
                })
            }
        }
    }
}
