//! The Ask arm's parking lot: in-memory pending approvals,
//! answered through a oneshot. Deliberately primitive — waiting, the
//! `select!` over oneshot/cancel/timeout and rule persistence for
//! `allow_always` live in the gate (a later PR); this type only registers,
//! answers, lists and cleans up.

use dashmap::DashMap;
use std::sync::{Arc, Weak};
use tokio::sync::oneshot;

use crate::agent::{AgentEvent, ApprovalOutcome, EventHub};
use crate::permissions::evaluator::{PermissionAction, rule_suggestion};

/// What the user answered to an approval request — the REST decide body.
/// `AllowAlways` makes the gate persist a rule before proceeding;
/// the registry itself only transports the answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Allow,
    AllowAlways,
    Deny,
}

/// Display summary of one parked approval — the payload the
/// `ApprovalRequested` event, the pending-list DTO and the cancel cleanup
/// share (title/summary with the outside mark, real path, command).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingApprovalMeta {
    pub id: String,
    pub conversation_id: String,
    /// Parent surface for a worker approval; the waiter still belongs to
    /// conversation_id and run_id.
    pub parent_conversation_id: Option<String>,
    pub run_id: String,
    /// The tool call id the approval parks.
    pub call_id: String,
    /// Tool name as the model sees it (e.g. `write_file`).
    pub tool: String,
    /// Action class label for rendering (see
    /// [`PermissionAction::kind_str`]).
    pub kind: String,
    /// One-line summary: the resolved path, the command, or the tool name.
    pub title: String,
    /// Workspace containment verdict, file actions only.
    pub outside: Option<bool>,
    pub path: Option<String>,
    pub command: Option<String>,
    /// A conservative "always allow" pattern for this action, offered
    /// with the approval so the user can edit it before submitting; the
    /// decide endpoint persists it verbatim on `allow_always`.
    pub rule_suggestion: Option<String>,
    /// When the approval was registered (display field of the pending list).
    pub created_at: chrono::NaiveDateTime,
}

impl PendingApprovalMeta {
    /// Derive the display summary from the action being approved — the
    /// single place that knows which display field carries what.
    pub fn from_action(
        id: String,
        conversation_id: &str,
        run_id: &str,
        call_id: &str,
        tool: &str,
        action: &PermissionAction,
    ) -> Self {
        let (outside, path, command) = match action {
            PermissionAction::Read { path, outside }
            | PermissionAction::Write { path, outside }
            | PermissionAction::Edit { path, outside } => {
                (Some(*outside), Some(path.display().to_string()), None)
            }
            PermissionAction::Command { command } => (None, None, Some(command.clone())),
            // Trusted actions never reach an approval; kept exhaustive
            // rather than guessable.
            PermissionAction::Mcp { .. } | PermissionAction::Trusted => (None, None, None),
        };
        Self {
            id,
            conversation_id: conversation_id.to_string(),
            parent_conversation_id: None,
            run_id: run_id.to_string(),
            call_id: call_id.to_string(),
            tool: tool.to_string(),
            kind: action.kind_str().to_string(),
            title: action.title(),
            outside,
            path,
            command,
            rule_suggestion: rule_suggestion(action),
            created_at: chrono::Utc::now().naive_utc(),
        }
    }
}

/// One parked approval: the meta (event/DTO payload) plus the oneshot that
/// answers the waiting gate. Dropping the sender (run settlement cleanup)
/// closes the waiter's channel — the gate's `select!` treats that as
/// cancelled.
pub struct PendingApproval {
    pub meta: PendingApprovalMeta,
    pub responder: oneshot::Sender<ApprovalDecision>,
    hubs: Vec<Weak<EventHub>>,
    decision_lock: Arc<tokio::sync::Mutex<()>>,
}

impl PendingApproval {
    fn publish_resolution(
        &self,
        outcome: ApprovalOutcome,
        rule_id: Option<String>,
        fallback: Option<&EventHub>,
    ) {
        let event = AgentEvent::ApprovalResolved {
            approval_id: self.meta.id.clone(),
            agent_id: self
                .meta
                .parent_conversation_id
                .as_ref()
                .map(|_| self.meta.conversation_id.clone()),
            outcome,
            rule_id,
        };
        for hub in self.hubs.iter().filter_map(Weak::upgrade) {
            hub.publish(event.clone());
        }
        if self.hubs.is_empty()
            && let Some(hub) = fallback
        {
            hub.publish(event);
        }
    }
}

/// The decide outcome for an id the registry does not hold. Unknown and
/// already-resolved are deliberately indistinguishable (the idempotent
/// decide endpoint answers both the same way).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("approval is unknown or already resolved")]
pub struct UnknownApproval;

/// In-memory registry of pending approvals: memory-only by design —
/// the only durable traces of an approval are the rules it produced and
/// the ToolResult it gated. Mounts on `RunService` in the wiring PR; safe
/// to share across threads.
#[derive(Default)]
pub struct ApprovalRegistry {
    pending: DashMap<String, PendingApproval>,
}

impl ApprovalRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a pending approval for one tool call. Returns its id
    /// (`apr_{uuid}` — the `ApprovalRequested` event and the decide URL
    /// carry it) and the receiving end the gate `select!`s on.
    pub fn create(
        &self,
        conversation_id: &str,
        run_id: &str,
        call_id: &str,
        tool: &str,
        action: &PermissionAction,
    ) -> (String, oneshot::Receiver<ApprovalDecision>) {
        self.create_with_delivery(conversation_id, run_id, call_id, tool, action, None, &[])
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_with_delivery(
        &self,
        conversation_id: &str,
        run_id: &str,
        call_id: &str,
        tool: &str,
        action: &PermissionAction,
        parent_conversation_id: Option<&str>,
        hubs: &[Arc<EventHub>],
    ) -> (String, oneshot::Receiver<ApprovalDecision>) {
        let id = format!("apr_{}", uuid::Uuid::new_v4());
        let (responder, receiver) = oneshot::channel();
        let mut meta = PendingApprovalMeta::from_action(
            id.clone(),
            conversation_id,
            run_id,
            call_id,
            tool,
            action,
        );
        meta.parent_conversation_id = parent_conversation_id.map(str::to_owned);
        let approval = self.pending.entry(id.clone()).or_insert(PendingApproval {
            meta,
            responder,
            hubs: hubs.iter().map(Arc::downgrade).collect(),
            decision_lock: Arc::new(tokio::sync::Mutex::new(())),
        });
        // Keep registration locked until the request has reached BOTH logs:
        // a concurrent snapshot/decision must not publish resolved first.
        let meta = &approval.meta;
        let requested = AgentEvent::ApprovalRequested {
            approval_id: meta.id.clone(),
            agent_id: meta
                .parent_conversation_id
                .as_ref()
                .map(|_| meta.conversation_id.clone()),
            call_id: meta.call_id.clone(),
            tool: meta.tool.clone(),
            kind: meta.kind.clone(),
            title: meta.title.clone(),
            path: meta.path.clone(),
            command: meta.command.clone(),
            outside: meta.outside,
            rule_suggestion: meta.rule_suggestion.clone(),
        };
        for hub in hubs {
            hub.publish(requested.clone());
        }
        drop(approval);
        (id, receiver)
    }

    /// Serialize parent/child decisions across rule persistence, then recheck
    /// membership under this lock. Cancellation never waits for the lock.
    pub fn decision_lock(&self, id: &str) -> Option<Arc<tokio::sync::Mutex<()>>> {
        self.pending
            .get(id)
            .map(|approval| approval.decision_lock.clone())
    }

    /// Answer a pending approval (the REST decide endpoint). `Ok` when the
    /// id was live and undecided; the oneshot carries the decision to the
    /// gate. A send failure just means the waiter is already gone (a
    /// cancelled run whose cleanup raced us) — the approval is settled
    /// either way.
    pub fn resolve(&self, id: &str, decision: ApprovalDecision) -> Result<(), UnknownApproval> {
        self.resolve_with_rule(id, decision, None)
    }

    pub fn resolve_with_rule(
        &self,
        id: &str,
        decision: ApprovalDecision,
        rule_id: Option<String>,
    ) -> Result<(), UnknownApproval> {
        let (_, approval) = self.pending.remove(id).ok_or(UnknownApproval)?;
        let outcome = match decision {
            ApprovalDecision::Allow | ApprovalDecision::AllowAlways => ApprovalOutcome::Approved,
            ApprovalDecision::Deny => ApprovalOutcome::Denied,
        };
        // Publish before releasing the waiter, so ToolResult cannot overtake
        // its resolution on either the child or the parent stream.
        approval.publish_resolution(outcome, rule_id, None);
        let _ = approval.responder.send(decision);
        Ok(())
    }

    /// Drop one pending approval without answering it — the gate's timeout
    /// arm (a self-denial must not disturb a sibling approval parked by a
    /// parallel tool call of the same run). Returns the meta so the caller
    /// can publish the `expired` resolution.
    pub fn remove(&self, id: &str) -> Option<PendingApprovalMeta> {
        self.pending.remove(id).map(|(_, approval)| approval.meta)
    }

    pub fn expire(&self, id: &str) {
        if let Some((_, approval)) = self.pending.remove(id) {
            approval.publish_resolution(ApprovalOutcome::Expired, None, None);
        }
    }

    pub fn cancel_run(&self, run_id: &str, fallback: &EventHub) {
        let ids: Vec<String> = self
            .pending
            .iter()
            .filter(|entry| entry.meta.run_id == run_id)
            .map(|entry| entry.key().clone())
            .collect();
        for id in ids {
            if let Some((_, approval)) = self.pending.remove(&id) {
                approval.publish_resolution(ApprovalOutcome::Cancelled, None, Some(fallback));
            }
        }
    }

    /// Drop every pending approval of a run — the settlement/cancel/crash
    /// cleanup. Their waiters see the oneshot close.
    /// Returns the metas so the caller can publish
    /// `ApprovalResolved { cancelled }` events for each.
    pub fn remove_for_run(&self, run_id: &str) -> Vec<PendingApprovalMeta> {
        let mut removed = Vec::new();
        self.pending.retain(|_, approval| {
            if approval.meta.run_id == run_id {
                removed.push(approval.meta.clone());
                false
            } else {
                true
            }
        });
        removed
    }

    /// The pending approvals of a conversation, for the pending-list DTO
    /// (the refresh-after-reload truth source). Iteration order is the
    /// map's, not insertion — fine for a list the front end re-renders
    /// from; parallel tool calls stack, they do not queue.
    pub fn pending_for_conversation(&self, conversation_id: &str) -> Vec<PendingApprovalMeta> {
        self.pending
            .iter()
            .filter(|entry| {
                entry.value().meta.conversation_id == conversation_id
                    || entry.value().meta.parent_conversation_id.as_deref() == Some(conversation_id)
            })
            .map(|entry| entry.value().meta.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn write_action() -> PermissionAction {
        PermissionAction::Write {
            path: PathBuf::from("/home/u/proj/src/main.rs"),
            outside: true,
        }
    }

    fn command_action() -> PermissionAction {
        PermissionAction::Command {
            command: "cargo build".into(),
        }
    }

    /// create → visible in the conversation's pending list; resolve once
    /// delivers through the oneshot; a second resolve (and an unknown id)
    /// are errors; the entry is gone afterwards.
    #[test]
    fn worker_expiry_resolves_both_surfaces_and_leaves_sibling_waiting() {
        let registry = ApprovalRegistry::new();
        let parent = Arc::new(EventHub::new());
        let child = Arc::new(EventHub::new());
        let hubs = [child.clone(), parent.clone()];
        let (first, mut first_rx) = registry.create_with_delivery(
            "child",
            "child-run",
            "first",
            "write_file",
            &write_action(),
            Some("parent"),
            &hubs,
        );
        let (second, mut second_rx) = registry.create_with_delivery(
            "child",
            "child-run",
            "second",
            "write_file",
            &write_action(),
            Some("parent"),
            &hubs,
        );
        registry.expire(&first);
        for hub in &hubs {
            let events = hub.snapshot();
            assert!(
                matches!(&events[0], AgentEvent::ApprovalRequested { approval_id, .. } if approval_id == &first)
            );
            assert!(
                matches!(&events[2], AgentEvent::ApprovalResolved { approval_id, agent_id: Some(agent), outcome: ApprovalOutcome::Expired, .. } if approval_id == &first && agent == "child")
            );
        }
        assert_eq!(registry.pending_for_conversation("parent")[0].id, second);
        assert!(matches!(
            first_rx.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        ));
        assert!(matches!(
            second_rx.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert_eq!(Arc::strong_count(&parent), 2); // only this test's Arc and hubs
    }

    #[test]
    fn create_resolve_once_and_only_once() {
        let registry = ApprovalRegistry::new();
        let (id, mut receiver) =
            registry.create("conv-1", "run-1", "call-1", "write_file", &write_action());

        let pending = registry.pending_for_conversation("conv-1");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, id);
        assert_eq!(pending[0].kind, "write");
        assert_eq!(pending[0].tool, "write_file");
        assert_eq!(pending[0].outside, Some(true));
        assert_eq!(pending[0].path.as_deref(), Some("/home/u/proj/src/main.rs"));
        assert_eq!(pending[0].title, "/home/u/proj/src/main.rs");
        assert!(pending[0].command.is_none());
        // Other conversations see nothing.
        assert!(registry.pending_for_conversation("conv-2").is_empty());

        assert!(registry.resolve(&id, ApprovalDecision::AllowAlways).is_ok());
        assert_eq!(
            receiver.try_recv().unwrap(),
            ApprovalDecision::AllowAlways
        );

        // Second resolve and unknown id: the same indistinguishable error.
        assert_eq!(
            registry.resolve(&id, ApprovalDecision::Deny),
            Err(UnknownApproval)
        );
        assert_eq!(
            registry.resolve("apr_nope", ApprovalDecision::Allow),
            Err(UnknownApproval)
        );
        assert!(registry.pending_for_conversation("conv-1").is_empty());
    }

    /// remove_for_run drops exactly that run's approvals, reports their
    /// metas for the cancelled events, and closes the waiters' channels
    /// (the sender side is dropped with the entry).
    #[test]
    fn remove_for_run_settles_and_signals_waiters() {
        let registry = ApprovalRegistry::new();
        let (_, mut rx_a) =
            registry.create("conv-1", "run-1", "call-1", "write_file", &write_action());
        let (_, mut rx_b) = registry.create(
            "conv-1",
            "run-1",
            "call-2",
            "execute_command",
            &command_action(),
        );
        let (_, mut rx_other) =
            registry.create("conv-1", "run-2", "call-3", "write_file", &write_action());

        let removed = registry.remove_for_run("run-1");
        assert_eq!(removed.len(), 2);
        assert!(removed.iter().all(|m| m.run_id == "run-1"));

        // The dropped senders close the channels: the gate's select! arm.
        assert!(matches!(rx_a.try_recv(), Err(oneshot::error::TryRecvError::Closed)));
        assert!(matches!(rx_b.try_recv(), Err(oneshot::error::TryRecvError::Closed)));
        // Other runs' approvals survive...
        assert!(matches!(rx_other.try_recv(), Err(oneshot::error::TryRecvError::Empty)));
        // ...and an empty removal is a no-op.
        assert!(registry.remove_for_run("run-1").is_empty());
        assert_eq!(registry.pending_for_conversation("conv-1").len(), 1);
    }
}
