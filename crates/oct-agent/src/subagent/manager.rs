//! Active child registry, cancellation and epoch-guarded settlement.
//!
//! Memory holds ACTIVE objects only — a terminal child's info lives in the
//! DB (`agent_runs` + the child conversation); the in-memory maps are
//! cleaned at settlement so nothing keeps a finished child's hub alive (the
//! SSE EOF guarantee). The manager has no application service callback;
//! active child hubs are weak while occupancy owns the strong handle.

use crate::agent::RunOutcome;
use crate::db::runs as run_db;
use crate::events::AgentEvent;
use crate::events::EventHub;
use anyhow::Result;
use std::sync::{Arc, Weak};
use std::time::Duration;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

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

/// Whether a persisted run status is terminal (`lost` is NOT terminal — it
/// is recoverable).
pub(crate) fn is_terminal_status(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "cancelled")
}

pub struct SubagentManager {
    /// Active children and agent → run lookup. Completion observers only
    /// retain this index, never the full manager or its service callback.
    pub(crate) registry: Arc<super::registry::ChildRegistry>,
    /// Protect registration through publishing the live handle. Released
    /// before waiting for execution, so independent children remain parallel.
    pub(crate) registration: Arc<tokio::sync::Mutex<()>>,
}

impl Default for SubagentManager {
    fn default() -> Self {
        Self {
            registry: Arc::new(super::registry::ChildRegistry::default()),
            registration: Arc::new(tokio::sync::Mutex::new(())),
        }
    }
}

impl SubagentManager {
    /// Record a starting child (called by the application child launcher
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
}

/// Activity of a live child (best-effort from its hub snapshot), used by the subagent list endpoint.
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
