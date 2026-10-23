//! Composition resources for application use cases. HTTP receives commands
//! and query components, never this assembly owner.
use crate::agent::RunHandle;
use crate::agent::repo_instructions::FrozenPromptEnv;
use crate::mcp::McpManager;
use crate::permissions::gate::PermissionGate;
use crate::permissions::registry::ApprovalRegistry;
use crate::runtime::{ModelResolver, RunOccupancy, background, lifecycle};
use crate::subagent::SubagentManager;
use crate::tools::shared::WorkspaceFileLocks;
use dashmap::DashMap;
use oct_llm_provider::provider::ProviderRegistry;
use sqlx::SqlitePool;
use std::sync::{Arc, Weak};

/// Inputs supplied by the composition root; retain clones for shared resources.
#[derive(Clone)]
pub struct ServiceResources {
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

/// Application resources shared by independent command handlers and launchers.
/// Holds clones of the same `Arc`s `AppState` holds — the `sessions` map in
/// particular is one shared instance so the reconnect/status endpoints keep
/// observing the same occupancy.
pub struct ApplicationServices {
    pub(crate) lifecycle: lifecycle::Lifecycle,
    pub(crate) pool: SqlitePool,
    pub(crate) registry: Arc<ProviderRegistry>,
    pub(crate) mcp: Arc<McpManager>,
    /// Same instance as `AppState.sessions`: the conversation occupancy map
    /// shared by send/resume/compact and every SSE reconnect reader.
    pub(crate) sessions: Arc<DashMap<String, RunHandle>>,
    /// Process-wide canonical-path locks, shared by root/child runs and by
    /// overlapping projects so aliases across project boundaries serialize.
    pub(crate) file_locks: Arc<DashMap<String, Arc<WorkspaceFileLocks>>>,
    /// Active child registry and guarded settlement. Delegation orchestration
    /// uses worker-only application capabilities; the index has no callback.
    pub(crate) subagents: Arc<SubagentManager>,
    /// Background process registry/lifecycle. Settlement emits a wake address
    /// consumed by the composition root's application notification drainer.
    pub(crate) background: Arc<background::BackgroundTaskManager>,
    /// Chat attachment store: run assembly grants the conversation's sent
    /// assets to the file tools as read-only files, conversation deletion
    /// sweeps it, and request assembly hydrates `oct-attachment://` references.
    pub(crate) attachments: Arc<crate::attachments::AttachmentService>,
    /// Pending tool-approval registry: every gate of this
    /// service parks its Ask arms here; one registry process-wide is all
    /// the decide endpoint needs.
    pub(crate) approvals: Arc<ApprovalRegistry>,
    /// Live permission gates by conversation id: weak so a
    /// settled run's gate dies with its loop; `allow_always` decisions and
    /// mode PATCHes reach the ACTIVE runs of a conversation through here.
    pub(crate) gates: Arc<DashMap<String, Vec<Weak<PermissionGate>>>>,
    /// Frozen prompt-head environments by ROOT conversation id: the
    /// repo-instruction block and MCP identity snapshot every run of that
    /// tree assembles from, reused byte-for-byte while clean by the shared
    /// assembly environment. `Arc` so dirty-flag handles can outlive a borrow.
    pub(crate) prompt_envs: Arc<DashMap<String, FrozenPromptEnv>>,
    /// Per-root-conversation serialization of environment resolution vs
    /// `/reload`: a reload in flight makes a concurrently STARTING run wait
    /// and then resolve the fresh entry, instead of racing the old one for
    /// its whole lifetime. Locks cover only the resolution section (never
    /// the run itself), and the guard extends to backend-initiated entry
    /// points (wake runs, recovery) that no frontend coordination reaches.
    pub(crate) prompt_env_locks: Arc<DashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl ApplicationServices {
    pub fn new(resources: ServiceResources) -> Self {
        Self {
            lifecycle: resources.lifecycle,
            pool: resources.pool,
            registry: resources.registry,
            mcp: resources.mcp,
            sessions: resources.sessions,
            file_locks: resources.file_locks,
            subagents: resources.subagents,
            background: resources.background,
            attachments: resources.attachments,
            approvals: resources.approvals,
            gates: resources.gates,
            prompt_envs: resources.prompt_envs,
            prompt_env_locks: resources.prompt_env_locks,
        }
    }

    pub(crate) fn assembly_services(&self) -> crate::application::assembly::AssemblyServices {
        crate::application::assembly::AssemblyServices {
            pool: self.pool.clone(),
            models: ModelResolver::new(self.pool.clone(), self.registry.clone()),
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

    pub fn occupancy(&self) -> RunOccupancy {
        RunOccupancy::new(self.sessions.clone(), self.lifecycle.clone())
    }

    pub(crate) fn delegation_service(&self) -> crate::application::delegation::DelegationService {
        crate::application::delegation::DelegationService::new(
            self.assembly_services(),
            self.occupancy(),
            self.subagents.clone(),
        )
    }
}
