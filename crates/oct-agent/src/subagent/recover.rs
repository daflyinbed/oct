//! Crash-recovery orchestration, driven by `resume_turn`.
//!
//! The HTTP layer resolves the parent model BEFORE any write, subscribes to
//! the (already occupied) conversation's hub and returns the SSE response
//! immediately — keep-alive covers the wait, children publish
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
//! 4. once every task is settled and every call/result pair closed, starts
//!    the parent loop as a NEW root run over the guard this orchestrator
//!    already holds (no re-occupy).
//!
//! Preparation failures (child model resolution etc.) publish an Error on
//! the parent hub and release the occupancy, leaving the persisted state
//! recoverable — nothing is half-started (先完成准备再启动).

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use tokio_util::sync::CancellationToken;

use oct_llm_provider::core::ContentPart;
use oct_llm_provider::model::ChatModel;

use crate::agent::{AgentEvent, EventHub, SubagentRunStatus};
use crate::db::projects::Project;
use crate::db::runs::AgentRun;
use crate::db::{messages as msg_db, runs as run_db};
use crate::runtime::{OccupancyGuard, RootRunRequest, RunService, workspace_roots};

use super::manager::{build_task_envelope, is_terminal_status};

/// Everything the orchestrator needs; built by `resume_turn` after its
/// read-only probes and model resolution.
pub struct RecoveryJob {
    pub runs: Arc<RunService>,
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
    /// The dangling call ids found by the eligibility probe.
    pub dangling: Vec<String>,
}

/// Run the whole recovery; spawned by `resume_turn` after the SSE response
/// is returned. Errors abort the WHOLE resume (all-prep-before-start): an
/// Error event lands on the hub and the guard drops with the job.
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
    job.runs
        .mcp
        .ensure_connections(&pool, Some(&job.project.id))
        .await;

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
        .repair_dangling_tools(&job.conversation_id, &managed_call_ids)
        .await
        .context("repairing unmanaged dangling tool calls")?;

    // 3. Settle every managed run (backfill / attach / resume). Sequential:
    //    a preparation failure must abort before anything new started.
    for run in managed {
        recover_one(&job, run).await?;
    }

    // 4. All tasks settled and every call/result pair closed — start the
    //    parent loop as a NEW root run over the guard this orchestrator
    //    already holds (no re-occupy; the session entry and its hub are the
    //    ones the SSE response subscribed to).
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
        dangling: _,
    } = job;
    runs.run_root_with_guard(RootRunRequest {
        guard,
        conversation_id,
        project,
        model,
        provider_id,
        model_id,
        view,
        context_window,
        idle,
        attach_task_tool: true,
    })
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

    // Terminal: backfill the envelope (covers 子已完成父未接收).
    if is_terminal_status(&run.status) {
        return backfill(pool, &job.conversation_id, &run, &call_id).await;
    }

    // Active in memory: attach to the wait — never restart a running child.
    if let Some(child) = job.runs.subagents.child_by_agent(&run.conversation_id) {
        let mut done = child.done.clone();
        let _ = done.wait_for(|d| *d).await;
        let settled = run_db::get_run(pool, &run.id)
            .await
            .context("re-reading the child run row")?
            .ok_or_else(|| anyhow!("child run row {} vanished", run.id))?;
        return backfill(pool, &job.conversation_id, &settled, &call_id).await;
    }

    // Lost / stale-not-in-memory: resume on the SAME run row.
    let Some(epoch) = run_db::bump_execution_epoch(pool, &run.id).await? else {
        // Settled concurrently (e.g. the cancel-lost endpoint): backfill.
        let settled = run_db::get_run(pool, &run.id)
            .await
            .context("re-reading the child run row")?
            .ok_or_else(|| anyhow!("child run row {} vanished", run.id))?;
        return backfill(pool, &job.conversation_id, &settled, &call_id).await;
    };

    // Repair the child conversation's dangling NORMAL tool calls (a child
    // toolset has no `task`, so every dangling call there is ordinary).
    job.runs
        .repair_dangling_tools(&run.conversation_id, &[])
        .await
        .context("repairing the child conversation's dangling tool calls")?;

    // Resolve the child model BEFORE anything starts (all-prep-before-start;
    // the run row's snapshot wins, the parent's ids are the fallback).
    let provider_id = run
        .provider_id
        .clone()
        .unwrap_or_else(|| job.provider_id.clone());
    let model_id = run.model_id.clone().unwrap_or_else(|| job.model_id.clone());
    let spec = format!("{provider_id}:{model_id}");
    let (model, provider_id, model_id) = job
        .runs
        .resolve_model(&spec)
        .await
        .map_err(|e| anyhow!("resolving the child model {spec} (run {}): {e}", run.id))?;

    let child_dangling = msg_db::find_dangling_tool_calls(pool, &run.conversation_id).await?;
    let child_messages = msg_db::list_messages(pool, &run.conversation_id).await?;

    // "Child effectively completed" shortcut: the crash
    // happened between the final message persisting and the run row
    // settling — no dangling calls, last row an assistant without tool
    // calls. Settle the row as completed without calling the model again.
    let effectively_completed = child_dangling.is_empty()
        && child_messages
            .last()
            .is_some_and(|m| m.role == "assistant" && !parts_carry_tool_call(&m.parts_json));
    if effectively_completed {
        let final_id = child_messages.last().expect("checked above").id.clone();
        run_db::finish_run(
            pool,
            &run.id,
            run_db::RunFinalStatus::Completed,
            Some(&final_id),
            None,
            None,
            None,
            Some(epoch),
        )
        .await
        .context("settling the effectively-completed child run")?;
        publish_settled(&job.hub, &run, epoch, "completed");
        let settled = run_db::get_run(pool, &run.id)
            .await
            .context("re-reading the child run row")?
            .ok_or_else(|| anyhow!("child run row {} vanished", run.id))?;
        return backfill(pool, &job.conversation_id, &settled, &call_id).await;
    }

    // Continue the SAME run row (epoch bumped) through the child supervisor.
    let start_message_id = msg_db::first_user_message_id(pool, &run.conversation_id)
        .await?
        .ok_or_else(|| {
            anyhow!(
                "child conversation {} has no delegation message",
                run.conversation_id
            )
        })?;
    let view = msg_db::load_llm_view(pool, &run.conversation_id)
        .await
        .context("loading the child view")?;
    let cancel = job.cancel.child_token();
    let guard = job
        .runs
        .occupy_with(&run.conversation_id, cancel.clone())
        .map_err(|e| anyhow!("child conversation is busy: {e}"))?;
    let (roots, _additional) = workspace_roots(&job.project);
    let locks = job.runs.file_locks_for(&job.project.id);
    let description = run.description.clone().unwrap_or_default();
    let handle = job
        .runs
        .start_child_run(crate::subagent::ChildRunRequest {
            guard,
            run_id: run.id.clone(),
            epoch,
            conversation_id: run.conversation_id.clone(),
            start_message_id,
            project: job.project.clone(),
            model,
            provider_id,
            model_id,
            view,
            context_window: run.context_window.and_then(|v| u32::try_from(v).ok()),
            agent_config: crate::agent_settings::WorkerSnapshot::decode(
                run.agent_config_json.as_deref(),
            )?,
            cancel,
            parent_hub: job.hub.clone(),
            parent_call_id: call_id.clone(),
            description,
            roots,
            locks,
            // Recovery rebuild: the crashed process's parent toolset is
            // gone; start_child_run rebuilds from default_tools_with +
            // mcp.tools_for_run — the same assembly sources, equal to
            // parent-minus-task in effect.
            parent_toolset: None,
            // No live parent to inherit from: the child resolves the
            // conversation's shared frozen environment from the cache.
            parent_env: None,
        })
        .await
        .map_err(|e| anyhow!("starting the recovered child run: {e}"))?;

    // Await settlement (a watch, never polling), then backfill.
    let mut done = handle.done.clone();
    let _ = done.wait_for(|d| *d).await;
    let settled = run_db::get_run(pool, &run.id)
        .await
        .context("re-reading the child run row")?
        .ok_or_else(|| anyhow!("child run row {} vanished", run.id))?;
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

/// Publish the terminal transition for a run settled outside the supervisor
/// (the effectively-completed shortcut).
fn publish_settled(hub: &Arc<EventHub>, run: &AgentRun, epoch: i64, status: &str) {
    hub.publish(AgentEvent::SubagentUpdated {
        run_id: run.id.clone(),
        agent_id: run.conversation_id.clone(),
        parent_call_id: run.parent_tool_call_id.clone().unwrap_or_default(),
        status: SubagentRunStatus::from_run_status(status),
        execution_epoch: u64::try_from(epoch).unwrap_or(u64::MAX),
        description: run.description.clone().unwrap_or_default(),
        activity: None,
    });
}

/// Whether a serialized `Vec<ContentPart>` carries any ToolCall part.
fn parts_carry_tool_call(parts_json: &str) -> bool {
    serde_json::from_str::<Vec<ContentPart>>(parts_json)
        .map(|parts| parts.iter().any(|p| matches!(p, ContentPart::ToolCall(_))))
        .unwrap_or(false)
}
