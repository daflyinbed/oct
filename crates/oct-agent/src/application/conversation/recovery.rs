//! Crash-recovery orchestration, driven by the resume application command.
//!
//! The application resolves the parent model BEFORE any write, subscribes to
//! the (already occupied) conversation's hub and returns the subscription
//! immediately — HTTP keep-alive covers the wait, children publish
//! `subagent_updated` onto the same hub so the UI is live. This orchestrator
//! then:
//!
//! 1. identifies the managed `task` delegations among the conversation's
//!    dangling calls;
//! 2. repairs only the UNmanaged dangling calls (a pending task must never
//!    be turned into an error placeholder);
//! 3. per managed run: backfills terminal results, attaches to active ones,
//!    or resumes `lost` ones on the SAME run row (epoch bumped) — including
//!    the "child effectively completed but the run row never settled"
//!    shortcut, which finishes the row without calling the model again;
//! 4. once every task is settled and every call/result pair closed, appends
//!    pending background notifications and starts the parent loop as a NEW
//!    root run over the guard this orchestrator already holds (no re-occupy).
//!
//! Preparation failures (child model resolution etc.) publish an Error on
//! the parent hub and release the occupancy, leaving the persisted state
//! recoverable. Each child's read-only preparation finishes before claiming
//! its execution epoch; startup errors after the claim settle that instance.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use tokio_util::sync::CancellationToken;

use oct_llm_provider::model::ChatModel;

use crate::application::{
    child_startup::ChildParent, execution::RootRunRequest, services::ApplicationServices,
};
use crate::db::projects::Project;
use crate::db::runs::AgentRun;
use crate::db::{messages as msg_db, runs as run_db};
use crate::events::AgentEvent;
use crate::events::EventHub;
use crate::runtime::OccupancyGuard;

use crate::subagent::build_task_envelope;

/// Everything the orchestrator needs; built by the resume command after its
/// read-only probes and model resolution.
pub struct RecoveryJob {
    pub runs: Arc<ApplicationServices>,
    /// The parent conversation's hub — already streaming to the returned SSE
    /// response; errors and child transitions publish here.
    pub hub: Arc<EventHub>,
    /// The orchestrator-level token: the session entry carries it (so a user
    /// cancel reaches this phase), children derive `child_token()`s from it,
    /// and the parent run started at the end reuses the same session handle.
    /// Cancelling the HTTP request isn't modelled in v1; this token IS the
    /// cancel surface during the recovery phase.
    pub cancel: CancellationToken,
    /// Consumed by the parent run started at the end — or dropped on error,
    /// releasing the occupancy and letting the SSE stream end.
    pub guard: OccupancyGuard,
    pub conversation_id: String,
    pub project: Project,
    pub model: Box<dyn ChatModel>,
    pub provider_id: String,
    pub model_id: String,
    pub context_window: Option<u32>,
    pub idle: Option<Duration>,
    /// 恢复的父 run 沿用被中断回合的思考档位（resume 从 user 消息
    /// details 里读出）；中途中断的对话不应静默改变思考行为。
    pub reasoning: Option<oct_llm_provider::core::ReasoningRequest>,
    /// The dangling call ids found by the eligibility probe.
    pub dangling: Vec<String>,
}

/// Run the whole recovery; spawned by resume before the subscription
/// is returned. An Error event lands on the hub on failure, and the guard
/// drops with the job. Child startup never leaves an unowned running row.
pub async fn orchestrate(job: RecoveryJob) {
    // Cloned out for the error path: once `orchestrate_inner` returns Err
    // the job (and with it the guard) is gone, but the hub clone keeps the
    // SSE response open just long enough to deliver the Error event before
    // EOF.
    let hub = job.hub.clone();
    let conversation_id = job.conversation_id.clone();
    if let Err(e) = orchestrate_inner(job).await {
        hub.publish(AgentEvent::Error(format!("resume failed: {e:#}")));
        tracing::error!(
            conversation_id = %conversation_id,
            "subagent recovery aborted: {e:#}"
        );
    }
}

async fn orchestrate_inner(job: RecoveryJob) -> Result<()> {
    let pool = job.runs.pool.clone();

    // Same MCP reconcile a chat turn does, once: both the recovery child
    // rebuild (mcp.tools_for_run) and the parent run assembly read the live
    // connections.
    if !job
        .runs
        .mcp
        .ensure_connections_cancelled(&pool, Some(&job.project.id), &job.cancel)
        .await
    {
        job.hub.publish(AgentEvent::Cancelled);
        return Ok(());
    }

    // 1. Identify the managed task delegations among the dangling calls.
    let managed = run_db::runs_for_parent_calls(&pool, &job.conversation_id, &job.dangling)
        .await
        .context("finding managed subagent runs")?;

    // 2. Repair only the unmanaged dangling calls (placeholder error
    //    results); the managed ones get their real envelopes below.
    let managed_call_ids: Vec<String> = managed
        .iter()
        .filter_map(|r| r.parent_tool_call_id.clone())
        .collect();
    job.runs
        .assembly_services()
        .repair_dangling_tools(&job.conversation_id, &managed_call_ids)
        .await
        .context("repairing unmanaged dangling tool calls")?;

    // 3. Settle every managed run (backfill / attach / resume). Each child is
    //    prepared before its own execution epoch is claimed.
    for run in managed {
        recover_one(&job, run).await?;
    }

    // 4. All tasks settled: only now may background notifications append a
    //    user message. Use send's preparation order so no entry point can
    //    split a tool-call/result batch with a notification.
    super::preparation::prepare_history(&job.runs, &job.conversation_id)
        .await
        .context("preparing the recovered parent history")?;

    // Start the parent loop as a NEW root run over the guard this
    // orchestrator already holds (no re-occupy; the session entry and its
    // hub are the ones the SSE response subscribed to).
    let view = msg_db::load_llm_view(&pool, &job.conversation_id)
        .await
        .context("loading the parent view")?;
    let RecoveryJob {
        runs,
        hub: _,
        cancel: _,
        guard,
        conversation_id,
        project,
        model,
        provider_id,
        model_id,
        context_window,
        idle,
        reasoning,
        dangling: _,
    } = job;
    super::launch_root(
        &runs,
        RootRunRequest {
            guard,
            conversation_id,
            project,
            model,
            provider_id,
            model_id,
            view,
            context_window,
            idle,
            reasoning,
            attach_task_tool: true,
        },
    )
    .await
    .map_err(|e| anyhow!("starting the resumed parent run: {e}"))?;
    Ok(())
}

/// Settle ONE managed run: backfill if terminal, attach if active in
/// memory, resume if `lost` (or a stale `running` row with no in-memory
/// handle).
async fn recover_one(job: &RecoveryJob, run: AgentRun) -> Result<()> {
    let pool = &job.runs.pool;
    let call_id = run
        .parent_tool_call_id
        .clone()
        .ok_or_else(|| anyhow!("managed run {} has no parent call id", run.id))?;

    // Startup owns terminal reuse, active waits, read-only preflight and the
    // same-run epoch claim. Recovery only backfills the settled task envelope.
    let settled = job
        .runs
        .child_startup()
        .recover(
            &run,
            ChildParent {
                project: job.project.clone(),
                provider_id: job.provider_id.clone(),
                model_id: job.model_id.clone(),
                cancel: job.cancel.clone(),
                hub: job.hub.clone(),
                // No crashed parent's live instances remain; the sealed worker
                // snapshot restores tools and the root cache supplies the prompt.
                live: None,
            },
        )
        .await
        .context("recovering the child run")?;
    backfill(pool, &job.conversation_id, &settled, &call_id).await
}

/// Write the envelope into the PARENT history as the ToolResult answering
/// `call_id` — exactly once (the insert only happens while the call is
/// still dangling).
async fn backfill(
    pool: &sqlx::SqlitePool,
    parent_conversation_id: &str,
    run: &AgentRun,
    call_id: &str,
) -> Result<()> {
    let envelope = build_task_envelope(pool, run)
        .await
        .context("building the task envelope")?;
    let inserted = msg_db::answer_tool_call_if_absent(
        pool,
        parent_conversation_id,
        call_id,
        &envelope.content,
        envelope.is_error,
        Some(envelope.details.clone()),
    )
    .await
    .context("backfilling the task result")?;
    if !inserted {
        tracing::debug!(
            run_id = %run.id,
            "task result already present in the parent history; kept the existing row"
        );
    }
    Ok(())
}
