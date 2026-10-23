//! Turn-top projection, microcompaction and summary maintenance.
use super::LoopStop;
use crate::agent::compact::{self, CompactAttempt};
use crate::agent::micro::{self, MicroAttempt};
use crate::agent::{AgentContext, RunHandle};
use crate::attachments::RequestAssembler;
use crate::context::ContextState;
use crate::events::MicrocompactTrigger;
use oct_llm_provider::core::{Message, ToolSpec};

// Auto-compaction bookkeeping: `compact_floor` is the estimated context
// right after the last compaction this run — re-triggering requires
// growing past it by another tail budget (prevents compact loops). A
// failed summary call disables auto compaction for the rest of the run.
// Microcompact bookkeeping: the idle entry is one-shot (first turn top
// only); a failed swap batch disables micro for the rest of the run.
#[derive(Default)]
pub(super) struct Maintenance {
    compact_floor: u64,
    auto_compact_disabled: bool,
    idle_micro_due: bool,
    micro_disabled: bool,
}

impl Maintenance {
    pub(super) fn new(idle: Option<std::time::Duration>) -> Self {
        Self {
            idle_micro_due: idle.is_some_and(|duration| duration >= micro::IDLE_TRIGGER),
            ..Self::default()
        }
    }

    pub(super) async fn prepare(
        &mut self,
        ctx: &AgentContext,
        conv_id: &str,
        handle: &RunHandle,
        context: &mut ContextState,
        tool_specs: &[ToolSpec],
        assembler: &RequestAssembler<'_>,
    ) -> Result<Vec<Message>, LoopStop> {
        // Missing files must be resolved before validating the anchor and
        // testing maintenance thresholds. A metadata-only plan may differ
        // from an unchanged, already measured missing-file omission.
        let initial_messages = assemble_request(assembler, context, handle).await?;
        let mut view_changed = false;

        // Turn-top maintenance — BEFORE the request, so both the first turn
        // of an over-long conversation and every tool round in between are
        // covered. Usage arriving during a stream only refreshes the anchor:
        // the decisions must see everything accumulated SINCE the last
        // request (tool results are the big increment).
        //
        // Microcompact (cheap, local, no model call) runs before the summary
        // check — clearing old tool results may pull the estimate back under
        // the summary threshold entirely, and micro stays available as the
        // fallback tier after a summary failure. The idle entry fires
        // regardless of the window; the waterline entry needs one.
        if self.idle_micro_due {
            self.idle_micro_due = false;
            match micro::run_microcompaction(
                &ctx.pool,
                conv_id,
                &handle.hub,
                MicrocompactTrigger::Idle,
                context,
                tool_specs,
                ctx.prompt_env_refresh.as_ref(),
            )
            .await
            {
                MicroAttempt::Done(_) => view_changed = true,
                MicroAttempt::Nothing => {}
                MicroAttempt::Failed => self.micro_disabled = true,
            }
        }

        let mut compacted = false;
        if let Some(window) = ctx.context_window {
            let threshold = compact::trigger_threshold(window);
            let tail_keep = compact::tail_keep_tokens(window);
            let mut est = context.estimate(tool_specs);
            // Waterline micro is gated only on its own failure switch: a
            // failed summary call disables auto compaction but micro must
            // stay available as the fallback tier (see the micro module doc).
            if !self.micro_disabled && est >= micro::trigger_threshold(window) {
                match micro::run_microcompaction(
                    &ctx.pool,
                    conv_id,
                    &handle.hub,
                    MicrocompactTrigger::Waterline,
                    context,
                    tool_specs,
                    ctx.prompt_env_refresh.as_ref(),
                )
                .await
                {
                    MicroAttempt::Done(est_after) => {
                        est = est_after;
                        view_changed = true;
                    }
                    MicroAttempt::Nothing => {}
                    MicroAttempt::Failed => self.micro_disabled = true,
                }
            }
            if !self.auto_compact_disabled
                && est >= threshold
                && est >= self.compact_floor.saturating_add(tail_keep)
            {
                let attempt = compact::run_auto_compaction(
                    ctx.model.as_ref(),
                    &ctx.pool,
                    conv_id,
                    &handle.hub,
                    &handle.cancel,
                    window,
                    tool_specs,
                    context,
                    est,
                    ctx.prompt_env_refresh.clone(),
                )
                .await;
                match attempt {
                    CompactAttempt::Done(floor) => {
                        self.compact_floor = floor;
                        compacted = true;
                        view_changed = true;
                    }
                    CompactAttempt::Nothing => {}
                    CompactAttempt::Failed => self.auto_compact_disabled = true,
                    CompactAttempt::Cancelled => {
                        return Err(LoopStop::Cancelled);
                    }
                }
            }
        }

        // Reuse the hydrated request unless maintenance rewrote history.
        // Release the old bytes before materializing a replacement, keeping
        // both the final request and estimator on the same selected projection.
        let request_messages = if view_changed {
            drop(initial_messages);
            assemble_request(assembler, context, handle).await?
        } else {
            initial_messages
        };
        if compacted {
            self.compact_floor = context.estimate(tool_specs);
        }
        Ok(request_messages)
    }
}

// Projection failures settle the run while retaining its durable user/tool
// history. No provider call is made with an oversized or unmeasured prefix.
async fn assemble_request(
    assembler: &RequestAssembler<'_>,
    context: &mut ContextState,
    handle: &RunHandle,
) -> Result<Vec<Message>, LoopStop> {
    let mut plan = assembler
        .plan(context)
        .await
        .map_err(|error| LoopStop::failed(error.to_string()))?;
    let messages = tokio::select! {
        messages = assembler.materialize(&mut plan) => messages,
        _ = handle.cancel.cancelled() => return Err(LoopStop::Cancelled),
    };
    context.apply_projection(plan.snapshot);
    Ok(messages)
}
