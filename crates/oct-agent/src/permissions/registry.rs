//! The Ask arm's parking lot (ADR 0004 §4): in-memory pending approvals,
//! answered through a oneshot. Deliberately primitive — waiting, the
//! `select!` over oneshot/cancel/timeout and rule persistence for
//! `allow_always` live in the gate (a later PR); this type only registers,
//! answers, lists and cleans up.

use dashmap::DashMap;
use tokio::sync::oneshot;

use crate::permissions::evaluator::{PermissionAction, rule_suggestion};

/// What the user answered to an approval request — the REST decide body
/// (§5). `AllowAlways` makes the gate persist a rule before proceeding;
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
/// share (§4: title/summary with the outside mark, real path, command).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingApprovalMeta {
    pub id: String,
    pub conversation_id: String,
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
    /// A conservative "always allow" pattern for this action (§3), offered
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
}

/// The decide outcome for an id the registry does not hold. Unknown and
/// already-resolved are deliberately indistinguishable (§4's idempotent
/// decide endpoint answers both the same way).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("approval is unknown or already resolved")]
pub struct UnknownApproval;

/// In-memory registry of pending approvals (§4): memory-only by design —
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
        let id = format!("apr_{}", uuid::Uuid::new_v4());
        let (responder, receiver) = oneshot::channel();
        let meta = PendingApprovalMeta::from_action(
            id.clone(),
            conversation_id,
            run_id,
            call_id,
            tool,
            action,
        );
        self.pending.insert(
            id.clone(),
            PendingApproval { meta, responder },
        );
        (id, receiver)
    }

    /// Answer a pending approval (the REST decide endpoint). `Ok` when the
    /// id was live and undecided; the oneshot carries the decision to the
    /// gate. A send failure just means the waiter is already gone (a
    /// cancelled run whose cleanup raced us) — the approval is settled
    /// either way.
    pub fn resolve(&self, id: &str, decision: ApprovalDecision) -> Result<(), UnknownApproval> {
        let (_, approval) = self.pending.remove(id).ok_or(UnknownApproval)?;
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

    /// Drop every pending approval of a run — the settlement/cancel/crash
    /// cleanup (§4's guard duty). Their waiters see the oneshot close.
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
    /// (§4's refresh-after-reload truth source). Iteration order is the
    /// map's, not insertion — fine for a list the front end re-renders
    /// from; parallel tool calls stack, they do not queue.
    pub fn pending_for_conversation(&self, conversation_id: &str) -> Vec<PendingApprovalMeta> {
        self.pending
            .iter()
            .filter(|entry| entry.value().meta.conversation_id == conversation_id)
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
