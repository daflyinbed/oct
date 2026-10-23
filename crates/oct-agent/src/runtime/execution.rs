//! Execution ownership and durable settlement. This kernel receives a fully
//! assembled context and a narrow completion observer, never an application
//! service, assembler or subagent manager.

use std::sync::Arc;

use oct_llm_provider::wire;
use sqlx::SqlitePool;

use crate::agent::AgentContext;
use crate::agent::RunHandle;
use crate::agent::RunOutcome;
use crate::agent::RunStatus;
use crate::agent::loop_runner::run_agent_loop;
use crate::context::LlmView;
use crate::db::runs as run_db;
use crate::events::EventHub;
use crate::permissions::gate::publish_cancelled_approvals;
use crate::permissions::registry::ApprovalRegistry;

use super::OccupancyGuard;

pub(crate) struct AssembledRun {
    pub ctx: AgentContext,
    pub view: LlmView,
}

#[derive(Clone)]
pub(crate) struct ExecutionIdentity {
    pub run_id: String,
    pub conversation_id: String,
    /// Roots start a new row; recovered workers claim a new epoch on the
    /// same row. A late worker must never settle a newer execution.
    pub epoch: Option<i64>,
}

impl ExecutionIdentity {
    fn kind(&self) -> &'static str {
        if self.epoch.is_some() {
            "subagent"
        } else {
            "root"
        }
    }
}

/// Invoked only after this execution's guarded terminal write committed.
/// The observer cannot change ownership or control the persistence boundary.
pub(crate) trait ExecutionObserver: Send + Sync {
    fn settled(&self, identity: &ExecutionIdentity, outcome: &RunOutcome, hub: &EventHub);
}

#[derive(Clone)]
pub(crate) struct RunExecutor {
    pool: SqlitePool,
    approvals: Arc<ApprovalRegistry>,
}

impl RunExecutor {
    pub fn new(pool: SqlitePool, approvals: Arc<ApprovalRegistry>) -> Self {
        Self { pool, approvals }
    }

    /// Terminal writes are shared by startup failures, root loops and worker
    /// loops. Transient storage failures retry inside finish_run; stale epochs
    /// never publish a completion or evict an active instance.
    pub async fn finish(
        &self,
        identity: &ExecutionIdentity,
        outcome: &RunOutcome,
        hub: &EventHub,
        observer: Option<&dyn ExecutionObserver>,
    ) -> crate::error::AppResult<bool> {
        finish_execution(&self.pool, identity, outcome, hub, observer).await
    }

    /// The loop has its own task so a panic becomes a Failed outcome rather
    /// than unwinding past settlement. Occupancy releases last, after the
    /// durable terminal write, approval cleanup and completion notification;
    /// dropping the last hub owner then lets every subscription reach EOF.
    pub async fn supervise(
        self,
        identity: ExecutionIdentity,
        assembled: AssembledRun,
        guard: OccupancyGuard,
        handle: RunHandle,
        observer: Option<Box<dyn ExecutionObserver>>,
    ) {
        let hub = handle.hub.clone();
        let outcome = match tokio::spawn(wire::with_tags(
            wire::Tags {
                conversation_id: Some(identity.conversation_id.clone()),
                run_id: Some(identity.run_id.clone()),
                run_kind: Some(identity.kind().into()),
                purpose: None,
            },
            run_agent_loop(
                assembled.ctx,
                identity.conversation_id.clone(),
                assembled.view,
                handle,
            ),
        ))
        .await
        {
            Ok(outcome) => outcome,
            Err(error) => RunOutcome {
                status: RunStatus::Failed,
                final_message_id: None,
                final_text: None,
                partial: false,
                usage: None,
                error: Some(format!("agent loop panicked: {error}")),
            },
        };

        // Preserve worker forwarding order: close forwarded approvals before
        // the parent observes the child's terminal transition.
        if identity.epoch.is_some() {
            publish_cancelled_approvals(&self.approvals, &identity.run_id, &hub);
        }
        if let Err(error) = self
            .finish(&identity, &outcome, &hub, observer.as_deref())
            .await
        {
            tracing::error!(run_id = %identity.run_id, %error, "failed to persist terminal state");
        }
        if identity.epoch.is_none() {
            publish_cancelled_approvals(&self.approvals, &identity.run_id, &hub);
        }
        drop(guard);
    }
}

/// Shared terminal write for execution and pre-assembly delegation failures.
pub(crate) async fn finish_execution(
    pool: &SqlitePool,
    identity: &ExecutionIdentity,
    outcome: &RunOutcome,
    hub: &EventHub,
    observer: Option<&dyn ExecutionObserver>,
) -> crate::error::AppResult<bool> {
    let status = match outcome.status {
        RunStatus::Completed => run_db::RunFinalStatus::Completed,
        RunStatus::Failed => run_db::RunFinalStatus::Failed,
        RunStatus::Cancelled => run_db::RunFinalStatus::Cancelled,
    };
    let committed = run_db::finish_run(
        pool,
        &identity.run_id,
        status,
        outcome.final_message_id.as_deref(),
        outcome.error.as_deref(),
        outcome
            .usage
            .as_ref()
            .map(|usage| i64::try_from(usage.input_tokens).unwrap_or(i64::MAX)),
        outcome
            .usage
            .as_ref()
            .map(|usage| i64::try_from(usage.output_tokens).unwrap_or(i64::MAX)),
        identity.epoch,
    )
    .await?;
    if committed && let Some(observer) = observer {
        observer.settled(identity, outcome, hub);
    }
    Ok(committed)
}
