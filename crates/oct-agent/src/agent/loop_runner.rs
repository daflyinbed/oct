//! Agent-loop phase order and run-wide outcome bookkeeping.
mod maintenance;
mod model_turn;
mod tool_batch;

use crate::agent::{AgentContext, RunHandle, RunOutcome, RunStatus, RunUsage};
use crate::attachments::RequestAssembler;
use crate::context::projection::MediaPolicy;
use crate::context::{ContextState, LlmView};
use crate::events::AgentEvent;
use maintenance::Maintenance;
use oct_llm_provider::core::GenerateOptions;
use oct_llm_provider::model::ChatRequest;
use tracing::info;

/// Final-message fields from the run's assistant-row bookkeeping `(id, text)`:
/// the row id always (when one exists), the text only when non-empty.
fn final_fields(last_assistant: Option<(String, String)>) -> (Option<String>, Option<String>) {
    match last_assistant {
        Some((id, text)) => (Some(id), (!text.is_empty()).then_some(text)),
        None => (None, None),
    }
}

#[derive(Default)]
struct UsageAccumulator {
    seen: bool,
    total: RunUsage,
}

impl UsageAccumulator {
    /// The usage aggregate iff at least one Usage event arrived during the run.
    fn into_option(self) -> Option<RunUsage> {
        self.seen.then_some(self.total)
    }
}

/// Stream errors count a partial text answer; other interruptions count any
/// persisted assistant row (reasoning/tool-only rows included).
enum LoopStop {
    Cancelled,
    EventBudget,
    Failed { message: String, text_only: bool },
}

impl LoopStop {
    fn failed(message: String) -> Self {
        Self::Failed {
            message,
            text_only: false,
        }
    }
    fn stream_failed(message: String) -> Self {
        Self::Failed {
            message,
            text_only: true,
        }
    }
}

#[derive(Default)]
struct RunProgress {
    // RunOutcome bookkeeping: the most recent successfully
    // persisted assistant row `(id, text)` — text may be empty when the row
    // carried only reasoning/tool parts — and the usage aggregate over every
    // Usage event seen during the run (absent ⇒ unknown, not zero).
    last_assistant: Option<(String, String)>,
    usage: UsageAccumulator,
}

impl RunProgress {
    /// Envelope for every non-Finish termination (cancel or failure): final
    /// fields come from the last persisted assistant row. `partial` marks
    /// persisted output: stream failures require text (a reasoning-only or
    /// tool-only tail is not a user-visible partial answer); cancellation
    /// and failures in other phases count any persisted assistant row.
    fn interrupted(self, stop: LoopStop, handle: &RunHandle) -> RunOutcome {
        let (status, error, partial) = match stop {
            LoopStop::Cancelled => {
                handle.hub.publish(AgentEvent::Cancelled);
                (RunStatus::Cancelled, None, self.last_assistant.is_some())
            }
            LoopStop::EventBudget => (
                RunStatus::Failed,
                Some("Run event budget exceeded".into()),
                self.last_assistant.is_some(),
            ),
            LoopStop::Failed { message, text_only } => {
                handle.hub.publish(AgentEvent::Error(message.clone()));
                let partial = if text_only {
                    self.last_assistant
                        .as_ref()
                        .is_some_and(|(_, text)| !text.is_empty())
                } else {
                    self.last_assistant.is_some()
                };
                (RunStatus::Failed, Some(message), partial)
            }
        };
        let (final_message_id, final_text) = final_fields(self.last_assistant);
        RunOutcome {
            status,
            final_message_id,
            final_text,
            partial,
            usage: self.usage.into_option(),
            error,
        }
    }
}

pub async fn run_agent_loop(
    ctx: AgentContext,
    conv_id: String,
    view: LlmView,
    handle: RunHandle,
) -> RunOutcome {
    // The run's mutable view of the conversation: system prompt + ordered
    // entries + usage anchor, kept in lockstep with the DB by construction
    // (see the `context` module doc).
    let mut context = ContextState::new(ctx.system_prompt.clone(), view)
        .with_input_modalities(ctx.input_modalities.clone());

    let mut maintenance = Maintenance::new(ctx.idle);
    let mut progress = RunProgress::default();
    let tool_specs: Vec<_> = ctx
        .tools
        .iter()
        .map(|t| crate::tool_contract::to_tool_spec(t.as_ref()))
        .collect();

    let assembler = RequestAssembler::new(ctx.attachments.as_deref(), MediaPolicy::default());
    loop {
        if handle.cancel.is_cancelled() {
            return progress.interrupted(LoopStop::Cancelled, &handle);
        }
        // Tool execution can exhaust the event budget between model turns.
        // Stop before maintenance can request a summary or open another stream.
        if handle.hub.is_overflowed() {
            return progress.interrupted(LoopStop::EventBudget, &handle);
        }
        let messages = match maintenance
            .prepare(
                &ctx,
                &conv_id,
                &handle,
                &mut context,
                &tool_specs,
                &assembler,
            )
            .await
        {
            Ok(messages) => messages,
            Err(stop) => return progress.interrupted(stop, &handle),
        };
        let request = ChatRequest {
            messages,
            tools: tool_specs.clone(),
            options: GenerateOptions {
                reasoning: ctx.reasoning.clone(),
                max_output_tokens: ctx.max_output_tokens,
                ..GenerateOptions::default()
            },
        };
        let turn = match model_turn::collect(&ctx, &handle, request, &mut progress.usage).await {
            Ok(turn) => turn,
            Err(stop) => return progress.interrupted(stop, &handle),
        };
        let turn = match model_turn::persist(&ctx, &conv_id, &mut context, turn).await {
            Ok(turn) => turn,
            Err(stop) => return progress.interrupted(stop, &handle),
        };
        if let Some(assistant) = &turn.assistant {
            progress.last_assistant = Some(assistant.clone());
        }
        if turn.tool_calls.is_empty() {
            info!(
                "Agent loop finished with reason: {:?}",
                Some(turn.finish_reason)
            );
            handle.hub.publish(AgentEvent::Finish);
            // Completed envelope from THIS round's stored assistant row (it
            // is by construction the run's final message); a round that
            // persisted nothing (empty stream) leaves both fields None.
            let (final_message_id, final_text) = final_fields(turn.assistant);
            return RunOutcome {
                status: RunStatus::Completed,
                final_message_id,
                final_text,
                partial: false,
                usage: progress.usage.into_option(),
                error: None,
            };
        }
        if let Err(stop) =
            tool_batch::execute(&ctx, &conv_id, &handle, &mut context, turn.tool_calls).await
        {
            return progress.interrupted(stop, &handle);
        }
    }
}
