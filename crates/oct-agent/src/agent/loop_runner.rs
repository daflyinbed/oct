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
use crate::agent::{AgentContext, AgentEvent, RunHandle};
use crate::db::messages as msg_db;
use crate::db::messages::LlmView;
use crate::tools;

pub async fn run_agent_loop(ctx: AgentContext, conv_id: String, view: LlmView, handle: RunHandle) {
    let mut full_messages = vec![Message::text(Role::System, &ctx.system_prompt)];
    full_messages.extend(view.messages);

    // Provider-truth anchor for compaction estimates (see compact::
    // estimate_context). +1 offsets the system message prepended above.
    let mut usage_anchor: Option<(u64, usize)> =
        view.anchor.map(|a| (a.tokens, a.message_index + 1));

    // Auto-compaction bookkeeping: `compact_floor` is the estimated context
    // right after the last compaction this run — re-triggering requires
    // growing past it by another tail budget (prevents compact loops). A
    // failed summary call disables auto compaction for the rest of the run.
    let mut compact_floor: u64 = 0;
    let mut auto_compact_disabled = false;

    let tool_specs: Vec<_> = ctx
        .tools
        .iter()
        .map(|t| tools::to_tool_spec(t.as_ref()))
        .collect();

    loop {
        if handle.cancel.is_cancelled() {
            handle.hub.publish(AgentEvent::Cancelled);
            return;
        }

        // Turn-top compaction check — BEFORE the request, so both the first
        // turn of an over-long conversation and every tool round in between
        // are covered. Usage arriving during a stream only refreshes the
        // anchor: the decision must see everything accumulated SINCE the
        // last request (tool results are the big increment).
        if let Some(window) = ctx.context_window
            && !auto_compact_disabled
        {
            let threshold = compact::trigger_threshold(window);
            let tail_keep = compact::tail_keep_tokens(window);
            let est = compact::estimate_context(&full_messages, &tool_specs, usage_anchor);
            if est >= threshold && est >= compact_floor.saturating_add(tail_keep) {
                let attempt = compact::run_auto_compaction(
                    ctx.model.as_ref(),
                    &ctx.pool,
                    &conv_id,
                    &ctx.system_prompt,
                    &handle.hub,
                    &handle.cancel,
                    window,
                    &tool_specs,
                    &mut full_messages,
                    &mut usage_anchor,
                    est,
                )
                .await;
                match attempt {
                    CompactAttempt::Done(floor) => compact_floor = floor,
                    CompactAttempt::Nothing => {}
                    CompactAttempt::Failed => auto_compact_disabled = true,
                    CompactAttempt::Cancelled => {
                        handle.hub.publish(AgentEvent::Cancelled);
                        return;
                    }
                }
            }
        }

        let request = ChatRequest {
            messages: full_messages.clone(),
            tools: tool_specs.clone(),
            options: GenerateOptions::default(),
        };

        let stream_result = ctx.model.stream(request).await;
        let mut event_stream = match stream_result {
            Ok(s) => s,
            Err(e) => {
                error!("Failed to start stream: {e}");
                handle
                    .hub
                    .publish(AgentEvent::Error(format!("LLM error: {e}")));
                return;
            }
        };

        let mut current_text = String::new();
        let mut reasoning_text = String::new();
        let mut reasoning_started_at: Option<Instant> = None;
        let mut reasoning_ended_at: Option<Instant> = None;
        let mut pending_tool_calls: Vec<ToolCall> = Vec::new();
        let mut finish_reason = FinishReason::Stop;
        let mut last_usage: Option<(Option<u32>, Option<u32>, Option<u32>)> = None;
        // input+output of the request currently streaming; committed as the
        // usage anchor once the assistant message lands in full_messages.
        let mut pending_anchor: Option<u64> = None;

        loop {
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
                            finish_reason = reason;
                        }
                        Ok(StreamEvent::ToolResult(_)) => {}
                        Err(e) => {
                            error!("Stream error: {e}");
                            handle.hub.publish(AgentEvent::Error(format!(
                                "Stream error: {e}"
                            )));
                            return;
                        }
                    }
                }
                _ = handle.cancel.cancelled() => {
                    info!("Agent loop cancelled during LLM streaming");
                    handle.hub.publish(AgentEvent::Cancelled);
                    return;
                }
            }
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
            assistant_parts.push(ContentPart::Text(current_text));
        }
        for tc in &pending_tool_calls {
            assistant_parts.push(ContentPart::ToolCall(tc.clone()));
        }

        let has_content = !assistant_parts.is_empty();
        let has_tool_calls =
            !pending_tool_calls.is_empty() && matches!(finish_reason, FinishReason::ToolCalls);

        if has_content {
            let assistant_msg = Message {
                role: Role::Assistant,
                parts: assistant_parts,
            };
            match msg_db::insert_message(
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
                }
                Err(e) => {
                    error!("Failed to persist assistant message: {e}");
                }
            }
            full_messages.push(assistant_msg);
            // Commit the anchor from this request's usage now that the
            // message sits at its final index. (Skipped naturally when the
            // stream reported no usage — the old anchor stays valid.)
            if let Some(tokens) = pending_anchor.take() {
                usage_anchor = Some((tokens, full_messages.len() - 1));
            }
        }

        if !has_tool_calls {
            info!("Agent loop finished with reason: {:?}", finish_reason);
            handle.hub.publish(AgentEvent::Finish);
            return;
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

            let tool_ctx = tools::ToolContext::new(tc.id.clone(), handle.hub.clone());
            contexts.push(tool_ctx.clone());

            // Announce the call at dispatch time so the frontend learns the tool
            // has started before (not after) its result arrives.
            handle.hub.publish(AgentEvent::ToolCallStart {
                id: tool_ctx.call_id().to_string(),
                name: tc.name.clone(),
                arguments: tc.arguments.clone(),
                title,
            });

            futures.push(async move {
                let tool = tools.iter().find(|t| t.name() == tc.name);
                let output = match tool {
                    Some(t) => match t.execute(args, &tool_ctx).await {
                        Ok(o) => o,
                        Err(e) => {
                            crate::tools::ToolOutput::error(format!("Tool execution error: {e}"))
                        }
                    },
                    None => crate::tools::ToolOutput::error(format!("Unknown tool: {}", tc.name)),
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

                            handle.hub.publish(AgentEvent::ToolResult {
                                call_id: tc.id.clone(),
                                content: output.content.clone(),
                                is_error: output.is_error,
                                details: output.details.clone(),
                            });

                            let tool_msg = Message {
                                role: Role::Tool,
                                parts: vec![ContentPart::ToolResult(ToolResult {
                                    call_id: tc.id.clone(),
                                    content: Value::String(output.content),
                                    is_error: output.is_error,
                                })],
                            };
                            // details are UI-only: persisted in their own
                            // column, never inside parts_json.
                            if let Err(e) = msg_db::insert_message(
                                &ctx.pool,
                                &conv_id,
                                &tool_msg,
                                None,
                                None,
                                output.details.as_ref(),
                            )
                            .await
                            {
                                error!("Failed to persist tool result: {e}");
                            }
                            full_messages.push(tool_msg);
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
            return;
        }
    }
}
