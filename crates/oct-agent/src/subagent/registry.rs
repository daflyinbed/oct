//! Active child execution index and completion publication. This observer
//! knows no RunService, assembler, database pool or delegation launcher.
use dashmap::DashMap;
use std::sync::Arc;
use tokio::sync::watch;

use super::ChildRun;
use crate::agent::{AgentEvent, EventHub, RunOutcome, RunStatus, SubagentRunStatus};
use crate::runtime::execution::{ExecutionIdentity, ExecutionObserver};

#[derive(Default)]
pub(crate) struct ChildRegistry {
    children: DashMap<String, Arc<ChildRun>>,
    by_agent: DashMap<String, String>,
}

impl ChildRegistry {
    pub fn register(&self, child: ChildRun) {
        self.by_agent
            .insert(child.agent_id.clone(), child.run_id.clone());
        self.children.insert(child.run_id.clone(), Arc::new(child));
    }

    pub fn is_active(&self, agent_id: &str) -> bool {
        self.by_agent.contains_key(agent_id)
    }

    pub fn child_by_agent(&self, agent_id: &str) -> Option<Arc<ChildRun>> {
        let run_id = self.by_agent.get(agent_id)?.value().clone();
        self.children
            .get(&run_id)
            .map(|entry| entry.value().clone())
    }
}

pub(crate) struct ChildCompletion {
    pub registry: Arc<ChildRegistry>,
    pub agent_id: String,
    pub parent_call_id: String,
    pub description: String,
    pub parent_hub: Arc<EventHub>,
    pub done: watch::Sender<bool>,
}

impl ExecutionObserver for ChildCompletion {
    fn settled(&self, identity: &ExecutionIdentity, outcome: &RunOutcome, hub: &EventHub) {
        let Some(epoch) = identity.epoch else {
            return;
        };
        let status = match outcome.status {
            RunStatus::Completed => "completed",
            RunStatus::Failed => "failed",
            RunStatus::Cancelled => "cancelled",
        };

        // Best-effort activity: the last tool call the child hub saw.
        let activity = hub.snapshot().iter().rev().find_map(|e| match e {
            AgentEvent::ToolCallStart { title, .. } => Some(title.clone()),
            _ => None,
        });

        self.parent_hub.publish(AgentEvent::SubagentUpdated {
            run_id: identity.run_id.clone(),
            agent_id: self.agent_id.clone(),
            parent_call_id: self.parent_call_id.clone(),
            status: SubagentRunStatus::from_run_status(status),
            execution_epoch: u64::try_from(epoch).unwrap_or(u64::MAX),
            description: self.description.clone(),
            activity,
        });

        // Done watch first (waiters wake and re-read the settled row), then
        // drop the in-memory entries — a terminal child's hub must not be
        // kept alive by the manager.
        let _ = self.done.send(true);
        self.registry
            .children
            .remove_if(&identity.run_id, |_, child| {
                Some(child.epoch) == identity.epoch
            });
        self.registry
            .by_agent
            .remove_if(&self.agent_id, |_, id| id == &identity.run_id);
    }
}
