use std::collections::HashSet;
use std::time::Instant;

use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use serde_json::{Value, json};
use tracing::{error, info};

use oct_llm_provider::core::GenerateOptions;
use oct_llm_provider::core::{
    ContentPart, FinishReason, Message, Role, StreamEvent, ToolCall, ToolResult,
};
use oct_llm_provider::model::ChatRequest;

use crate::agent::compact::{self, CompactAttempt};
use crate::agent::micro::{self, MicroAttempt};
use crate::agent::{
    AgentContext, AgentEvent, MicrocompactTrigger, RunHandle, RunOutcome, RunStatus, RunUsage,
};
use crate::context::{ContextState, LlmView};
use crate::db::messages as msg_db;
use crate::tools;

/// Final-message fields from the run's assistant-row bookkeeping `(id, text)`:
/// the row id always (when one exists), the text only when non-empty.
fn final_fields(last_assistant: Option<(String, String)>) -> (Option<String>, Option<String>) {
    match last_assistant {
        Some((id, text)) => (Some(id), (!text.is_empty()).then_some(text)),
        None => (None, None),
    }
}

/// The usage aggregate iff at least one Usage event arrived during the run.
fn usage_opt(seen: bool, total: &RunUsage) -> Option<RunUsage> {
    seen.then(|| total.clone())
}

/// Envelope for every non-Finish termination (cancel or failure): final
/// fields come from the last persisted assistant row. `partial` marks
/// persisted output and follows the caller's rule — for a cancel ANY
/// assistant row counts; for a failure only one carrying text (a
/// reasoning-only/tool-only tail is not a user-visible partial answer).
fn interrupted_outcome(
    status: RunStatus,
    partial: bool,
    error: Option<String>,
    last_assistant: Option<(String, String)>,
    usage: Option<RunUsage>,
) -> RunOutcome {
    let (final_message_id, final_text) = final_fields(last_assistant);
    RunOutcome {
        status,
        final_message_id,
        final_text,
        partial,
        usage,
        error,
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
    let mut context = ContextState::new(ctx.system_prompt.clone(), view);

    // Auto-compaction bookkeeping: `compact_floor` is the estimated context
    // right after the last compaction this run — re-triggering requires
    // growing past it by another tail budget (prevents compact loops). A
    // failed summary call disables auto compaction for the rest of the run.
    let mut compact_floor: u64 = 0;
    let mut auto_compact_disabled = false;

    // Microcompact bookkeeping: the idle entry is one-shot (first turn top
    // only); a failed swap batch disables micro for the rest of the run.
    let mut idle_micro_due = ctx.idle.is_some_and(|d| d >= micro::IDLE_TRIGGER);
    let mut micro_disabled = false;

    // RunOutcome bookkeeping: the most recent successfully
    // persisted assistant row `(id, text)` — text may be empty when the row
    // carried only reasoning/tool parts — and the usage aggregate over every
    // Usage event seen during the run (absent ⇒ unknown, not zero).
    let mut last_assistant: Option<(String, String)> = None;
    let mut usage_total = RunUsage::default();
    let mut usage_seen = false;

    let tool_specs: Vec<_> = ctx
        .tools
        .iter()
        .map(|t| tools::to_tool_spec(t.as_ref()))
        .collect();

    loop {
        if handle.cancel.is_cancelled() {
            handle.hub.publish(AgentEvent::Cancelled);
            let partial = last_assistant.is_some();
            return interrupted_outcome(
                RunStatus::Cancelled,
                partial,
                None,
                last_assistant,
                usage_opt(usage_seen, &usage_total),
            );
        }

        // Tool execution can exhaust the event budget between model turns.
        // Stop before maintenance can request a summary or open another stream.
        if handle.hub.is_overflowed() {
            return interrupted_outcome(
                RunStatus::Failed,
                last_assistant.is_some(),
                Some("Run event budget exceeded".into()),
                last_assistant,
                usage_opt(usage_seen, &usage_total),
            );
        }

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
        if idle_micro_due {
            idle_micro_due = false;
            match micro::run_microcompaction(
                &ctx.pool,
                &conv_id,
                &handle.hub,
                MicrocompactTrigger::Idle,
                &mut context,
                &tool_specs,
            )
            .await
            {
                MicroAttempt::Done(_) | MicroAttempt::Nothing => {}
                MicroAttempt::Failed => micro_disabled = true,
            }
        }

        if let Some(window) = ctx.context_window {
            let threshold = compact::trigger_threshold(window);
            let tail_keep = compact::tail_keep_tokens(window);
            let mut est = context.estimate(&tool_specs);
            // Waterline micro is gated only on its own failure switch: a
            // failed summary call disables auto compaction but micro must
            // stay available as the fallback tier (see the micro module doc).
            if !micro_disabled && est >= micro::trigger_threshold(window) {
                match micro::run_microcompaction(
                    &ctx.pool,
                    &conv_id,
                    &handle.hub,
                    MicrocompactTrigger::Waterline,
                    &mut context,
                    &tool_specs,
                )
                .await
                {
                    MicroAttempt::Done(est_after) => est = est_after,
                    MicroAttempt::Nothing => {}
                    MicroAttempt::Failed => micro_disabled = true,
                }
            }
            if !auto_compact_disabled
                && est >= threshold
                && est >= compact_floor.saturating_add(tail_keep)
            {
                let attempt = compact::run_auto_compaction(
                    ctx.model.as_ref(),
                    &ctx.pool,
                    &conv_id,
                    &handle.hub,
                    &handle.cancel,
                    window,
                    &tool_specs,
                    &mut context,
                    est,
                )
                .await;
                match attempt {
                    CompactAttempt::Done(floor) => compact_floor = floor,
                    CompactAttempt::Nothing => {}
                    CompactAttempt::Failed => auto_compact_disabled = true,
                    CompactAttempt::Cancelled => {
                        handle.hub.publish(AgentEvent::Cancelled);
                        let partial = last_assistant.is_some();
                        return interrupted_outcome(
                            RunStatus::Cancelled,
                            partial,
                            None,
                            last_assistant,
                            usage_opt(usage_seen, &usage_total),
                        );
                    }
                }
            }
        }

        let request = ChatRequest {
            messages: context.messages_for_request(),
            tools: tool_specs.clone(),
            options: GenerateOptions::default(),
        };

        // The request-establishment await must respond to cancellation too:
        // a cancelled run must not hang inside connection
        // setup waiting for a stream that may never open.
        // The with_tags scope covers the stream() call itself, where the
        // wire ctx captures purpose=chat for every log line of this request.
        let mut chat_tags = oct_llm_provider::wire::current_tags();
        chat_tags.purpose = Some("chat".to_string());
        let stream_result = tokio::select! {
            result = oct_llm_provider::wire::with_tags(chat_tags, ctx.model.stream(request)) => result,
            _ = handle.cancel.cancelled() => {
                info!("Agent loop cancelled while establishing the LLM stream");
                handle.hub.publish(AgentEvent::Cancelled);
                let partial = last_assistant.is_some();
                return interrupted_outcome(
                    RunStatus::Cancelled,
                    partial,
                    None,
                    last_assistant,
                    usage_opt(usage_seen, &usage_total),
                );
            }
        };
        let mut event_stream = match stream_result {
            Ok(s) => s,
            Err(e) => {
                let message = format!("LLM error: {e}");
                error!("Failed to start stream: {e}");
                handle.hub.publish(AgentEvent::Error(message.clone()));
                let partial = last_assistant
                    .as_ref()
                    .is_some_and(|(_, text)| !text.is_empty());
                return interrupted_outcome(
                    RunStatus::Failed,
                    partial,
                    Some(message),
                    last_assistant,
                    usage_opt(usage_seen, &usage_total),
                );
            }
        };

        let mut current_text = String::new();
        let mut reasoning_text = String::new();
        let mut reasoning_started_at: Option<Instant> = None;
        let mut reasoning_ended_at: Option<Instant> = None;
        let mut pending_tool_calls: Vec<ToolCall> = Vec::new();
        let mut finish_reason = None;
        let mut last_usage: Option<(Option<u32>, Option<u32>, Option<u32>)> = None;
        // input+output of the request currently streaming; committed as the
        // usage anchor once the assistant message lands in full_messages.
        let mut pending_anchor: Option<u64> = None;
        // This round's persisted assistant row (id, text): the Finish
        // envelope reads it directly below (by construction the final
        // message), while a copy feeds the run-wide `last_assistant` for the
        // interrupted paths.
        let mut round_assistant: Option<(String, String)> = None;

        loop {
            if handle.hub.is_overflowed() {
                return interrupted_outcome(
                    RunStatus::Failed,
                    last_assistant.is_some(),
                    Some("Run event budget exceeded".into()),
                    last_assistant,
                    usage_opt(usage_seen, &usage_total),
                );
            }
            tokio::select! {
                event = event_stream.next() => {
                    let Some(event) = event else { break };
                    // Thinking ends at the first non-reasoning stream event so
                    // the UI duration covers thinking only, not the answer
                    // that follows it.
                    if reasoning_started_at.is_some()
                        && reasoning_ended_at.is_none()
                        && !matches!(event, Ok(StreamEvent::ReasoningDelta(_)))
                    {
                        reasoning_ended_at = Some(Instant::now());
                    }
                    match event {
                        Ok(StreamEvent::TextDelta(text)) => {
                            current_text.push_str(&text);
                            handle.hub.publish(AgentEvent::TextDelta(text));
                        }
                        Ok(StreamEvent::ReasoningDelta(text)) => {
                            reasoning_started_at.get_or_insert_with(Instant::now);
                            reasoning_text.push_str(&text);
                            handle.hub.publish(AgentEvent::ReasoningDelta(text));
                        }
                        Ok(StreamEvent::ToolCallDelta {
                            call_id,
                            name,
                            arguments_delta,
                        }) => {
                            // Forward partial tool-call arguments verbatim so
                            // the frontend can render the call as it streams.
                            handle.hub.publish(AgentEvent::ToolCallDelta {
                                call_id,
                                name,
                                arguments_delta,
                            });
                        }
                        Ok(StreamEvent::ToolCall(tc)) => {
                            pending_tool_calls.push(tc);
                        }
                        Ok(StreamEvent::Usage(usage)) => {
                            // Run aggregate for the RunOutcome (alongside the
                            // per-request anchor bookkeeping below).
                            usage_seen = true;
                            usage_total.input_tokens +=
                                u64::from(usage.input_tokens.unwrap_or(0));
                            usage_total.output_tokens +=
                                u64::from(usage.output_tokens.unwrap_or(0));
                            last_usage = Some((
                                usage.input_tokens,
                                usage.output_tokens,
                                usage.reasoning_tokens,
                            ));
                            // Provider-truth anchor for the NEXT estimate:
                            // everything after this request is a local
                            // increment on top of these tokens.
                            if usage.input_tokens.is_some() || usage.output_tokens.is_some() {
                                pending_anchor = Some(
                                    u64::from(usage.input_tokens.unwrap_or(0))
                                        + u64::from(usage.output_tokens.unwrap_or(0)),
                                );
                            }
                            handle.hub.publish(AgentEvent::Usage {
                                input_tokens: usage.input_tokens,
                                output_tokens: usage.output_tokens,
                                reasoning_tokens: usage.reasoning_tokens,
                                context_window_tokens: ctx.context_window,
                            });
                        }
                        Ok(StreamEvent::Finish(reason)) => {
                            finish_reason = Some(reason);
                        }
                        Ok(StreamEvent::ToolResult(_)) => {}
                        Err(e) => {
                            let message = format!("Stream error: {e}");
                            error!("Stream error: {e}");
                            handle.hub.publish(AgentEvent::Error(message.clone()));
                            let partial = last_assistant
                                .as_ref()
                                .is_some_and(|(_, text)| !text.is_empty());
                            return interrupted_outcome(
                                RunStatus::Failed,
                                partial,
                                Some(message),
                                last_assistant,
                                usage_opt(usage_seen, &usage_total),
                            );
                        }
                    }
                }
                _ = handle.cancel.cancelled() => {
                    info!("Agent loop cancelled during LLM streaming");
                    handle.hub.publish(AgentEvent::Cancelled);
                    let partial = last_assistant.is_some();
                    return interrupted_outcome(
                        RunStatus::Cancelled,
                        partial,
                        None,
                        last_assistant,
                        usage_opt(usage_seen, &usage_total),
                    );
                }
            }
        }

        if finish_reason.is_none() {
            let message = "Model stream ended before a completion event".to_string();
            handle.hub.publish(AgentEvent::Error(message.clone()));
            return interrupted_outcome(
                RunStatus::Failed,
                last_assistant.is_some(),
                Some(message),
                last_assistant,
                usage_opt(usage_seen, &usage_total),
            );
        }

        let mut assistant_parts: Vec<ContentPart> = Vec::new();
        let reasoning_details = reasoning_started_at
            .filter(|_| !reasoning_text.is_empty())
            .map(|started| {
                let ms = reasoning_ended_at
                    .unwrap_or_else(Instant::now)
                    .saturating_duration_since(started)
                    .as_millis() as u64;
                // UI-only thinking duration for the frontend card; stored in
                // details_json so it never enters the model-visible parts.
                json!({ "reasoning_duration_ms": ms })
            });
        if !reasoning_text.is_empty() {
            assistant_parts.push(ContentPart::Reasoning(reasoning_text));
        }
        if !current_text.is_empty() {
            // Cloned (not moved): the run-outcome bookkeeping below still
            // needs this round's text for the final-message record.
            assistant_parts.push(ContentPart::Text(current_text.clone()));
        }
        for tc in &pending_tool_calls {
            assistant_parts.push(ContentPart::ToolCall(tc.clone()));
        }

        let has_content = !assistant_parts.is_empty();
        let has_tool_calls = !pending_tool_calls.is_empty()
            && matches!(finish_reason, Some(FinishReason::ToolCalls));

        if has_content {
            let assistant_msg = Message {
                role: Role::Assistant,
                parts: assistant_parts,
            };
            // On success the entry lands at the ordering the insert returned;
            // a failed durable append stops the run before any new side effect.
            let ordering = match msg_db::insert_message(
                &ctx.pool,
                &conv_id,
                &assistant_msg,
                None,
                None,
                reasoning_details.as_ref(),
            )
            .await
            {
                Ok(stored) => {
                    if let Some((input, output, reasoning)) = last_usage
                        && let Err(e) = msg_db::update_message_usage(
                            &ctx.pool, &stored.id, input, output, reasoning,
                        )
                        .await
                    {
                        error!("Failed to persist usage for message {}: {e}", stored.id);
                    }
                    let entry = (stored.id.clone(), current_text.clone());
                    round_assistant = Some(entry.clone());
                    last_assistant = Some(entry);
                    stored.ordering
                }
                Err(e) => {
                    let message = format!("Failed to persist assistant message: {e}");
                    handle.hub.publish(AgentEvent::Error(message.clone()));
                    return interrupted_outcome(
                        RunStatus::Failed,
                        last_assistant.is_some(),
                        Some(message),
                        last_assistant,
                        usage_opt(usage_seen, &usage_total),
                    );
                }
            };
            context.append(assistant_msg, ordering);
            // Commit the anchor from this request's usage now that the
            // message sits at its final index. (Skipped naturally when the
            // stream reported no usage — the old anchor stays valid.)
            if let Some(tokens) = pending_anchor.take() {
                context.set_anchor(tokens);
            }
        }

        if !has_tool_calls {
            info!("Agent loop finished with reason: {:?}", finish_reason);
            handle.hub.publish(AgentEvent::Finish);
            // Completed envelope from THIS round's stored assistant row (it
            // is by construction the run's final message); a round that
            // persisted nothing (empty stream) leaves both fields None.
            let (final_message_id, final_text) = final_fields(round_assistant);
            return RunOutcome {
                status: RunStatus::Completed,
                final_message_id,
                final_text,
                partial: false,
                usage: usage_opt(usage_seen, &usage_total),
                error: None,
            };
        }

        let indexed_calls: Vec<(usize, ToolCall)> =
            pending_tool_calls.into_iter().enumerate().collect();
        let total = indexed_calls.len();

        let mut futures = FuturesUnordered::new();
        // ToolContext per call, parallel to `indexed_calls` (same idx). Kept
        // here so the completion branch can flush the smoother's remainder
        // before the final ToolResult event.
        let mut contexts: Vec<tools::ToolContext> = Vec::with_capacity(total);

        for (idx, tc) in &indexed_calls {
            let tc = tc.clone();
            let idx = *idx;
            let tools = ctx.tools.clone();
            let args: Value =
                serde_json::from_str(&tc.arguments).unwrap_or(Value::Object(Default::default()));

            // Best-effort human-readable title for the UI; falls back to the
            // tool name (unknown tool, unparseable args).
            let title = tools
                .iter()
                .find(|t| t.name() == tc.name)
                .map_or_else(|| tc.name.clone(), |t| t.title(&args));

            // The run's cancel token rides along so long-running tools
            // (execute, MCP) can abort in-flight work the moment the user
            // cancels, instead of running to their own timeout.
            let tool_ctx =
                tools::ToolContext::new(tc.id.clone(), handle.hub.clone(), handle.cancel.clone());
            contexts.push(tool_ctx.clone());

            // Announce the call at dispatch time so the frontend learns the tool
            // has started before (not after) its result arrives.
            handle.hub.publish(AgentEvent::ToolCallStart {
                id: tool_ctx.call_id().to_string(),
                name: tc.name.clone(),
                arguments: tc.arguments.clone(),
                title,
            });

            // The permission gate rides along with every
            // call: Allow proceeds to `execute`, Deny produces an error
            // ToolOutput that flows the NORMAL result path below (event +
            // persistence + model feed-back), and Ask parks the future on
            // an approval until answered, cancelled or timed out.
            let permission = ctx.permission.clone();
            let gate_hub = handle.hub.clone();
            let gate_cancel = handle.cancel.clone();

            futures.push(async move {
                let tool = tools.iter().find(|t| t.name() == tc.name);
                let output = match permission
                    .check(&tc.name, &args, &tc.id, &gate_hub, &gate_cancel)
                    .await
                {
                    crate::permissions::gate::GateVerdict::Denied(output) => output,
                    crate::permissions::gate::GateVerdict::Proceed => match tool {
                        Some(t) => match t.execute(args, &tool_ctx).await {
                            Ok(o) => o,
                            Err(e) => crate::tools::ToolOutput::error(format!(
                                "Tool execution error: {e}"
                            )),
                        },
                        None => {
                            crate::tools::ToolOutput::error(format!("Unknown tool: {}", tc.name))
                        }
                    },
                };
                (idx, tc, output)
            });
        }

        let mut completed: HashSet<usize> = HashSet::new();

        loop {
            tokio::select! {
                result = futures.next() => {
                    match result {
                        Some((idx, tc, output)) => {
                            // Flush the smoother's buffered remainder first so
                            // the frontend has seen all streamed output when
                            // the closing ToolResult arrives.
                            contexts[idx].flush();

                            let result_event = AgentEvent::ToolResult {
                                call_id: tc.id.clone(),
                                content: output.content.clone(),
                                is_error: output.is_error,
                                images: output.images.clone(),
                                details: output.details.clone(),
                            };

                            let tool_msg = Message {
                                role: Role::Tool,
                                parts: vec![ContentPart::ToolResult(ToolResult {
                                    call_id: tc.id.clone(),
                                    content: Value::String(output.content),
                                    is_error: output.is_error,
                                    // Multimodal pass-through: images ride the
                                    // ToolResult (persisted via parts_json,
                                    // replayed for later turns) next to the
                                    // text content.
                                    images: output
                                        .images
                                        .into_iter()
                                        .map(From::from)
                                        .collect(),
                                })],
                            };
                            // details are UI-only: persisted in their own
                            // column, never inside parts_json.
                            let ordering = match crate::db::persist_critical(&ctx.pool, || msg_db::insert_message(
                                &ctx.pool,
                                &conv_id,
                                &tool_msg,
                                None,
                                None,
                                output.details.as_ref(),
                            ))
                            .await
                            {
                                Ok(stored) => stored.ordering,
                                Err(e) => {
                                    let message = format!("Failed to persist tool result: {e}");
                                    handle.hub.publish(AgentEvent::Error(message.clone()));
                                    return interrupted_outcome(RunStatus::Failed, last_assistant.is_some(),
                                        Some(message), last_assistant, usage_opt(usage_seen, &usage_total));
                                }
                            };
                            handle.hub.publish(result_event);
                            context.append(tool_msg, ordering);
                            completed.insert(idx);
                        }
                        None => break,
                    }
                }
                _ = handle.cancel.cancelled() => {
                    info!(
                        "Agent loop cancelled during tool execution, {}/{} completed",
                        completed.len(),
                        total
                    );
                    break;
                }
            }
        }

        if handle.cancel.is_cancelled() {
            for (idx, tc) in &indexed_calls {
                if !completed.contains(idx) {
                    let cancel_msg = Message {
                        role: Role::Tool,
                        parts: vec![ContentPart::ToolResult(ToolResult {
                            call_id: tc.id.clone(),
                            content: Value::String("用户取消了执行".into()),
                            is_error: true,
                            images: Vec::new(),
                        })],
                    };
                    if let Err(e) =
                        msg_db::insert_message(&ctx.pool, &conv_id, &cancel_msg, None, None, None)
                            .await
                    {
                        error!("Failed to persist cancel result: {e}");
                    }
                }
            }
            handle.hub.publish(AgentEvent::Cancelled);
            let partial = last_assistant.is_some();
            return interrupted_outcome(
                RunStatus::Cancelled,
                partial,
                None,
                last_assistant,
                usage_opt(usage_seen, &usage_total),
            );
        }
    }
}
