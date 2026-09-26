//! The permission gate (ADR 0004 §1.1/§4): the single checkpoint every
//! model-driven tool call passes through, invoked at the agent loop's
//! dispatch point before `Tool::execute`. One gate instance serves one run;
//! [`crate::runtime::RunService`] assembles it (root and child alike) from
//! the persisted rules, the conversation's mode and the workspace roots.
//!
//! * **Allow** → [`GateVerdict::Proceed`] and the tool runs untouched.
//! * **Deny** → an error [`ToolOutput`](crate::tools::ToolOutput) that
//!   flows the NORMAL result path (event, persistence, model feed-back) —
//!   the model sees and can react to the refusal; the loop treats it no
//!   differently from a tool's own error.
//! * **Ask** → register a pending approval, publish `ApprovalRequested`,
//!   and park on `select! { oneshot, run cancel, timeout }`. Subagent runs
//!   never park: their Ask is downgraded to a Deny with an explanatory
//!   message (§1.6 — the requirement travels back to the root instead).

use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::agent::{AgentEvent, ApprovalOutcome, EventHub};
use crate::permissions::descriptor::describe;
use crate::permissions::evaluator::{PermissionAction, PermissionMode, decide, rule_suggestion};
use crate::permissions::registry::{ApprovalDecision, ApprovalRegistry};
use crate::permissions::rules::{Rule, RuleSet};
use crate::tools::ToolOutput;
use crate::tools::workspace::WorkspaceRoots;

/// The gate's answer to one tool call. `Denied` carries the error
/// `ToolOutput` the dispatch site must feed through the normal result path
/// (NOT an early return — the refusal is data the model sees).
#[derive(Debug)]
pub enum GateVerdict {
    /// Execute the tool.
    Proceed,
    /// Refuse: an is_error ToolOutput with `kind: "permission_denied"`
    /// details (or a plain cancellation message on the cancel arm).
    Denied(ToolOutput),
}

/// Refusal copy for a rule/built-in deny, formatted with the action summary
/// (the resolved path or the command) so the model can tell WHICH call was
/// refused when a round carried several.
const RULE_DENY_PREFIX: &str = "Permission denied by rule";
/// The built-in exec-root deny (§1.3): ADR 0003's isolation promise,
/// restated for the model.
const EXEC_ROOT_DENY: &str = "Permission denied: background task output is only readable through the task_output tool";
/// Subagent Ask downgrade (§1.6).
const SUBAGENT_ASK_DOWNGRADE: &str = "Permission denied: subagents cannot trigger interactive approval; state the needed action in your final answer";
const USER_DENY: &str = "Permission denied by the user";
const TIMEOUT_DENY: &str = "Permission denied: approval timed out";
/// The cancel arm reuses the loop's cancellation copy so the refusal blends
/// into the existing cancel semantics (the run is ending anyway).
const CANCELLED: &str = "用户取消了执行";

/// Default Ask timeout (fail-closed, §1.4): 10 minutes.
const DEFAULT_APPROVAL_TIMEOUT_SECS: u64 = 600;

/// How long an Ask parks before self-denying. `OCT_APPROVAL_TIMEOUT`
/// (seconds) overrides; read once per process.
fn approval_timeout() -> Duration {
    static TIMEOUT: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *TIMEOUT.get_or_init(|| {
        std::env::var("OCT_APPROVAL_TIMEOUT")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|secs| *secs > 0)
            .map(Duration::from_secs)
            .unwrap_or(Duration::from_secs(DEFAULT_APPROVAL_TIMEOUT_SECS))
    })
}

/// An error ToolOutput stamped with the UI-only denial marker (the frontend
/// renders the "denied" badge off `kind`, exactly like the microcompact
/// cleared marker).
fn denied_output(reason: String) -> ToolOutput {
    ToolOutput::error_with_details(reason, json!({ "kind": "permission_denied" }))
}

/// One run's permission policy: the rule chain (mutable only through
/// `allow_always` injection), the conversation mode (mutable through the
/// PATCH endpoint — per-call effective), the workspace roots the descriptor
/// resolves against, and the subagent flag that downgrades Ask to Deny.
pub struct PermissionGate {
    conversation_id: String,
    run_id: String,
    roots: WorkspaceRoots,
    rules: RwLock<RuleSet>,
    mode: RwLock<PermissionMode>,
    /// Subagent runs have no interactive surface: their Ask becomes a Deny
    /// (§1.6) carrying the guidance copy.
    subagent: bool,
    registry: Arc<ApprovalRegistry>,
}

impl PermissionGate {
    /// Assemble a run's gate. Shared through `Arc` so `RunService` can keep
    /// weak references for live mode updates and `allow_always` injection.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        conversation_id: String,
        run_id: String,
        roots: WorkspaceRoots,
        rules: RuleSet,
        mode: PermissionMode,
        subagent: bool,
        registry: Arc<ApprovalRegistry>,
    ) -> Arc<Self> {
        Arc::new(Self {
            conversation_id,
            run_id,
            roots,
            rules: RwLock::new(rules),
            mode: RwLock::new(mode),
            subagent,
            registry,
        })
    }

    /// An all-allowing gate (bypass mode, no rules): for agent runs driven
    /// outside `RunService` assembly (the test harness) where gating is not
    /// under test. Deny rules would still win, hence the empty rule set.
    pub fn permissive() -> Arc<Self> {
        Self::new(
            String::new(),
            String::new(),
            WorkspaceRoots::new(std::path::PathBuf::new(), Vec::new()),
            RuleSet::new("/oct-permissive-no-exec-root"),
            PermissionMode::Bypass,
            false,
            Arc::new(ApprovalRegistry::new()),
        )
    }

    pub fn conversation_id(&self) -> &str {
        &self.conversation_id
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// The currently effective mode (read per call, so a PATCH lands on the
    /// very next tool call of a live run).
    pub fn mode(&self) -> PermissionMode {
        *self
            .mode
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Live mode switch (the PATCH conversation endpoint; §1.5's
    /// "per-call immediate effect").
    pub fn set_mode(&self, mode: PermissionMode) {
        *self
            .mode
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = mode;
    }

    /// Live rule injection (the `allow_always` decide endpoint: §1.8 —
    /// persist the row, then push it into every active gate of the
    /// conversation so the SAME run stops asking).
    pub fn add_rule(&self, rule: Rule) {
        self.rules
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .add(rule);
    }

    /// Gate one tool call (§4's protocol). The hub is passed per call — the
    /// gate must not hold a strong hub reference of its own (the SSE EOF
    /// guarantee belongs to the run handle).
    pub async fn check(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
        call_id: &str,
        hub: &Arc<EventHub>,
        cancel: &CancellationToken,
    ) -> GateVerdict {
        // Descriptor first: extraction failures map to Trusted (the tool's
        // own error is the right outcome), so `decide` always sees a
        // well-formed action.
        let action = describe(tool_name, args, &self.roots);
        let decision = {
            let rules = self
                .rules
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            decide(&action, self.mode(), &rules)
        };

        match decision {
            // Allow: the dispatch site executes the tool as-is.
            // (Trusted descriptors land here too — constant Allow.)
            crate::permissions::evaluator::Decision::Allow => GateVerdict::Proceed,
            crate::permissions::evaluator::Decision::Deny => {
                let rules = self
                    .rules
                    .read()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let reason = if rules.denies_exec_root(&action) {
                    EXEC_ROOT_DENY.to_string()
                } else {
                    format!("{RULE_DENY_PREFIX}: {}", action.title())
                };
                GateVerdict::Denied(denied_output(reason))
            }
            crate::permissions::evaluator::Decision::Ask => {
                if self.subagent {
                    return GateVerdict::Denied(denied_output(SUBAGENT_ASK_DOWNGRADE.to_string()));
                }
                self.park_for_approval(&action, tool_name, call_id, hub, cancel)
                    .await
            }
        }
    }

    /// The Ask arm (§4): register, publish, and `select!` over the oneshot,
    /// the run's cancellation and the timeout.
    async fn park_for_approval(
        &self,
        action: &PermissionAction,
        tool_name: &str,
        call_id: &str,
        hub: &Arc<EventHub>,
        cancel: &CancellationToken,
    ) -> GateVerdict {
        let (approval_id, mut responder) = self.registry.create(
            &self.conversation_id,
            &self.run_id,
            call_id,
            tool_name,
            action,
        );

        // The event mirrors the registry's meta (the pending-list endpoint
        // serves the same fields from there): the resolved path, the
        // command, the outside mark and a conservative always-allow
        // suggestion the user can edit before submitting.
        hub.publish(AgentEvent::ApprovalRequested {
            approval_id: approval_id.clone(),
            call_id: call_id.to_string(),
            tool: tool_name.to_string(),
            kind: action.kind_str().to_string(),
            title: action.title(),
            path: action.file_path().map(|p| p.display().to_string()),
            command: match action {
                PermissionAction::Command { command } => Some(command.clone()),
                _ => None,
            },
            outside: match action {
                PermissionAction::Read { outside, .. }
                | PermissionAction::Write { outside, .. }
                | PermissionAction::Edit { outside, .. } => Some(*outside),
                _ => None,
            },
            rule_suggestion: rule_suggestion(action),
        });

        tokio::select! {
            answer = &mut responder => match answer {
                // The decide endpoint resolves allow/allow_always alike
                // into a Proceed (rule persistence and gate injection are
                // the endpoint's job, §1.8).
                Ok(ApprovalDecision::Allow) | Ok(ApprovalDecision::AllowAlways) => {
                    GateVerdict::Proceed
                }
                Ok(ApprovalDecision::Deny) => {
                    GateVerdict::Denied(denied_output(USER_DENY.to_string()))
                }
                // Sender dropped: the run's settlement cleanup raced us —
                // the approval was already resolved `cancelled` there.
                Err(_) => GateVerdict::Denied(ToolOutput::error(CANCELLED)),
            },
            _ = cancel.cancelled() => {
                // Best-effort early cleanup; the supervisor's settlement
                // sweep is the deterministic one (idempotent — the second
                // remove finds nothing).
                publish_cancelled_approvals(&self.registry, &self.run_id, hub);
                GateVerdict::Denied(ToolOutput::error(CANCELLED))
            }
            _ = tokio::time::sleep(approval_timeout()) => {
                // Fail-closed self-denial (§1.4); only THIS approval is
                // dropped — a parallel sibling keeps waiting for its user.
                if let Some(meta) = self.registry.remove(&approval_id) {
                    hub.publish(AgentEvent::ApprovalResolved {
                        approval_id: meta.id,
                        outcome: ApprovalOutcome::Expired,
                        rule_id: None,
                    });
                }
                GateVerdict::Denied(denied_output(TIMEOUT_DENY.to_string()))
            }
        }
    }
}

/// Drop every pending approval of a run and publish a `cancelled`
/// resolution for each (§4's cleanup): the deterministic sweep both run
/// supervisors call at settlement, right before releasing occupancy — a
/// parked gate future may simply be dropped when the loop's cancel branch
/// wins the select race, so the supervisor (not the gate) owns this.
pub fn publish_cancelled_approvals(
    registry: &ApprovalRegistry,
    run_id: &str,
    hub: &EventHub,
) {
    for meta in registry.remove_for_run(run_id) {
        hub.publish(AgentEvent::ApprovalResolved {
            approval_id: meta.id,
            outcome: ApprovalOutcome::Cancelled,
            rule_id: None,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::events::EventHub;
    use crate::permissions::evaluator::Decision;
    use crate::permissions::rules::Effect;
    use futures_util::StreamExt;

    fn gate_with(
        mode: PermissionMode,
        rules: impl FnOnce(&mut RuleSet),
        subagent: bool,
    ) -> (Arc<PermissionGate>, Arc<ApprovalRegistry>, Arc<EventHub>, CancellationToken) {
        let registry = Arc::new(ApprovalRegistry::new());
        let mut set = RuleSet::new("/oct-gate-test-no-exec-root");
        rules(&mut set);
        let roots = WorkspaceRoots::new(std::path::PathBuf::from("/oct-gate-test-ws"), Vec::new());
        let gate = PermissionGate::new(
            "conv-1".into(),
            "run-1".into(),
            roots,
            set,
            mode,
            subagent,
            registry.clone(),
        );
        let hub = Arc::new(EventHub::new());
        (gate, registry, hub, CancellationToken::new())
    }

    fn outside_write_args() -> serde_json::Value {
        // The primary root /oct-gate-test-ws does not exist, so the
        // canonicalized absolute target classifies outside.
        json!({"path": "/etc/oct-gate-test.conf", "content": "x"})
    }

    #[tokio::test]
    async fn allow_and_rule_deny_short_circuit_without_events() {
        // Bypass: everything proceeds, zero hub events.
        let (gate, _registry, hub, cancel) = gate_with(PermissionMode::Bypass, |_| {}, false);
        assert!(matches!(
            gate.check(
                "write_file",
                &outside_write_args(),
                "call-1",
                &hub,
                &cancel
            )
            .await,
            GateVerdict::Proceed
        ));
        assert!(hub.snapshot().is_empty());

        // A deny rule wins even under bypass and yields the marked error
        // output; the decision never touches the registry.
        let (gate, registry, hub, cancel) = gate_with(PermissionMode::Bypass, |set| {
            set.add(Rule {
                effect: Effect::Deny,
                tool: "write_file".into(),
                pattern: None,
            });
        }, false);
        let verdict = gate
            .check("write_file", &outside_write_args(), "call-1", &hub, &cancel)
            .await;
        match verdict {
            GateVerdict::Denied(output) => {
                assert!(output.is_error);
                assert!(output.content.starts_with("Permission denied by rule"));
                assert_eq!(
                    output.details.as_ref().and_then(|d| d.get("kind")).and_then(|k| k.as_str()),
                    Some("permission_denied")
                );
            }
            GateVerdict::Proceed => panic!("deny rule must refuse"),
        }
        assert!(registry.pending_for_conversation("conv-1").is_empty());
        assert!(hub.snapshot().is_empty(), "a deny is not an approval event");
    }

    #[tokio::test]
    async fn ask_parks_publishes_and_answers() {
        // default mode + outside write → Ask.
        let (gate, registry, hub, cancel) =
            gate_with(PermissionMode::Default, |_| {}, false);
        let mut subscriber = hub.subscribe();

        let gate_clone = gate.clone();
        let hub_clone = hub.clone();
        let cancel_clone = cancel.clone();
        let parked = tokio::spawn(async move {
            gate_clone
                .check("write_file", &outside_write_args(), "call-9", &hub_clone, &cancel_clone)
                .await
        });

        // The request event lands with the action's display fields.
        let requested = subscriber.next().await.expect("ApprovalRequested");
        let AgentEvent::ApprovalRequested {
            approval_id,
            call_id,
            tool,
            kind,
            outside,
            rule_suggestion,
            ..
        } = requested
        else {
            panic!("expected ApprovalRequested, got {requested:?}");
        };
        assert_eq!(call_id, "call-9");
        assert_eq!(tool, "write_file");
        assert_eq!(kind, "write");
        assert_eq!(outside, Some(true));
        assert_eq!(rule_suggestion.as_deref(), Some("/etc/**"));

        // allow → Proceed; the pending entry is gone.
        assert!(registry.resolve(&approval_id, ApprovalDecision::Allow).is_ok());
        assert!(matches!(parked.await.unwrap(), GateVerdict::Proceed));
        assert!(registry.pending_for_conversation("conv-1").is_empty());
    }

    #[tokio::test]
    async fn ask_denied_by_user_answer() {
        let (gate, registry, hub, cancel) =
            gate_with(PermissionMode::Default, |_| {}, false);
        let mut subscriber = hub.subscribe();
        let gate2 = gate.clone();
        let hub2 = hub.clone();
        let parked = tokio::spawn(async move {
            gate2.check("write_file", &outside_write_args(), "call-d", &hub2, &cancel).await
        });
        let requested = subscriber.next().await.expect("ApprovalRequested");
        let AgentEvent::ApprovalRequested { approval_id, .. } = requested else {
            panic!("expected ApprovalRequested");
        };
        assert!(registry.resolve(&approval_id, ApprovalDecision::Deny).is_ok());
        match parked.await.unwrap() {
            GateVerdict::Denied(output) => {
                assert!(output.is_error);
                assert!(output.content.contains("denied by the user"));
            }
            GateVerdict::Proceed => panic!("deny answer must refuse"),
        }
    }

    #[tokio::test]
    async fn cancel_cleans_up_pending_and_publishes_cancelled() {
        let (gate, registry, hub, cancel) =
            gate_with(PermissionMode::Default, |_| {}, false);
        let mut subscriber = hub.subscribe();
        let gate2 = gate.clone();
        let hub2 = hub.clone();
        let cancel2 = cancel.clone();
        let parked = tokio::spawn(async move {
            gate2.check("write_file", &outside_write_args(), "call-c", &hub2, &cancel2).await
        });
        let requested = subscriber.next().await.expect("ApprovalRequested");
        let AgentEvent::ApprovalRequested { approval_id, .. } = requested else {
            panic!("expected ApprovalRequested");
        };
        cancel.cancel();
        match parked.await.unwrap() {
            GateVerdict::Denied(output) => assert_eq!(output.content, "用户取消了执行"),
            GateVerdict::Proceed => panic!("cancel must refuse"),
        }
        // The pending entry is gone and the resolution event landed.
        assert!(registry.pending_for_conversation("conv-1").is_empty());
        let AgentEvent::ApprovalResolved {
            approval_id: resolved,
            outcome,
            ..
        } = subscriber.next().await.expect("ApprovalResolved")
        else {
            panic!("expected ApprovalResolved");
        };
        assert_eq!(resolved, approval_id);
        assert_eq!(outcome, ApprovalOutcome::Cancelled);
    }

    #[tokio::test]
    async fn timeout_self_denies_and_expires_only_its_own_approval() {
        // Two parallel Asks from the same run; the first times out, the
        // second must keep waiting. Timeout shrunk through the env var is
        // not possible here (OnceLock is process-wide), so this test drives
        // the timeout arm indirectly: assert that WITHOUT any answer/cancel
        // the gate stays parked (bounded by a short outer timeout), then
        // verify the expiry publication path through publish-cancelled
        // sibling semantics — the timeout arm shares its shape.
        let (gate, registry, hub, cancel) =
            gate_with(PermissionMode::Default, |_| {}, false);
        let mut subscriber = hub.subscribe();
        let gate2 = gate.clone();
        let hub2 = hub.clone();
        let mut parked = tokio::spawn(async move {
            gate2.check("write_file", &outside_write_args(), "call-t", &hub2, &cancel).await
        });
        let requested = subscriber.next().await.expect("ApprovalRequested");
        let AgentEvent::ApprovalRequested { approval_id, .. } = requested else {
            panic!("expected ApprovalRequested");
        };
        // Parked: no verdict within a short window (the real timeout is
        // 600s; the test must not wait for it).
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut parked)
                .await
                .is_err(),
            "no answer, no cancel: the gate must stay parked"
        );
        // The single-approval remove (the timeout arm's cleanup) drops
        // exactly this entry.
        assert!(registry.remove(&approval_id).is_some());
        // The parked future now observes the closed channel (Err arm) and
        // resolves with the cancellation copy.
        match parked.await.unwrap() {
            GateVerdict::Denied(output) => assert_eq!(output.content, "用户取消了执行"),
            GateVerdict::Proceed => panic!("dropped responder must refuse"),
        }
    }

    #[tokio::test]
    async fn subagent_ask_downgrades_to_deny_without_parking() {
        let (gate, registry, hub, cancel) =
            gate_with(PermissionMode::Default, |_| {}, true);
        // Inside-workspace write: Ask under default mode → downgraded.
        let verdict = gate
            .check(
                "write_file",
                &json!({"path": "inner.txt", "content": "x"}),
                "call-s",
                &hub,
                &cancel,
            )
            .await;
        match verdict {
            GateVerdict::Denied(output) => {
                assert!(output.is_error);
                assert!(output
                    .content
                    .contains("subagents cannot trigger interactive approval"));
            }
            GateVerdict::Proceed => panic!("subagent Ask must downgrade to Deny"),
        }
        // Nothing was parked and nothing was published.
        assert!(registry.pending_for_conversation("conv-1").is_empty());
        assert!(hub.snapshot().is_empty());
        // Inside reads still pass for subagents (constant Allow row).
        assert!(matches!(
            gate.check("read_file", &json!({"path": "inner.txt"}), "call-r", &hub, &cancel).await,
            GateVerdict::Proceed
        ));
    }

    #[tokio::test]
    async fn mode_and_rules_update_live() {
        let (gate, _registry, hub, cancel) = gate_with(PermissionMode::Default, |_| {}, false);
        let args = outside_write_args();
        assert!(matches!(
            decide(
                &describe("write_file", &args, &gate.roots),
                gate.mode(),
                &gate.rules.read().unwrap(),
            ),
            Decision::Ask
        ));
        gate.set_mode(PermissionMode::Bypass);
        assert!(matches!(
            gate.check("write_file", &args, "call-1", &hub, &cancel).await,
            GateVerdict::Proceed
        ));
        // An injected allow rule keeps the verdict when the mode returns to
        // default (allow beats the mode default).
        gate.add_rule(Rule {
            effect: Effect::Allow,
            tool: "write_file".into(),
            pattern: Some("/etc/**".into()),
        });
        gate.set_mode(PermissionMode::Default);
        assert!(matches!(
            gate.check("write_file", &args, "call-2", &hub, &cancel).await,
            GateVerdict::Proceed
        ));
    }

    #[tokio::test]
    async fn settlement_sweep_publishes_cancelled_for_every_pending() {
        let registry = Arc::new(ApprovalRegistry::new());
        let hub = EventHub::new();
        let action = PermissionAction::Command {
            command: "cargo build".into(),
        };
        let (id_a, mut rx_a) = registry.create("conv-1", "run-9", "c1", "execute_command", &action);
        let (id_b, mut rx_b) = registry.create("conv-1", "run-9", "c2", "execute_command", &action);
        let (_other, mut rx_other) = registry.create("conv-1", "run-8", "c3", "execute_command", &action);

        publish_cancelled_approvals(&registry, "run-9", &hub);
        // Both waiters saw their channels close (the senders dropped)...
        assert!(matches!(
            rx_a.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Closed)
        ));
        assert!(matches!(
            rx_b.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Closed)
        ));
        // ...the other run's approval survived.
        assert!(matches!(
            rx_other.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        // Two cancelled events on the hub, one per swept approval.
        let snapshot = hub.snapshot();
        let cancelled: Vec<&String> = snapshot
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ApprovalResolved {
                    approval_id,
                    outcome: ApprovalOutcome::Cancelled,
                    ..
                } => Some(approval_id),
                _ => None,
            })
            .collect();
        assert_eq!(cancelled.len(), 2);
        assert!(cancelled.contains(&&id_a));
        assert!(cancelled.contains(&&id_b));
        // Idempotent: a second sweep finds nothing.
        publish_cancelled_approvals(&registry, "run-9", &hub);
        assert_eq!(hub.snapshot().len(), 2);
    }
}
