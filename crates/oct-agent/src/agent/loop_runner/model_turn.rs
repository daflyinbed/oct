//! One model response: ordered stream collection and durable assistant append.
use super::{LoopStop, UsageAccumulator};
use crate::agent::{AgentContext, RunHandle};
use crate::context::ContextState;
use crate::db::messages as msg_db;
use crate::events::AgentEvent;
use futures_util::StreamExt;
use oct_llm_provider::core::{
    ContentPart, FinishReason, Message, Role, StreamEvent, ToolCall, Usage,
};
use oct_llm_provider::model::ChatRequest;
use serde_json::json;
use std::time::Instant;
use tracing::{error, info};

pub(super) struct ModelTurn {
    message: Message,
    text: String,
    tool_calls: Vec<ToolCall>,
    finish_reason: FinishReason,
    usage: Option<Usage>,
    anchor: Option<u64>,
    reasoning_duration_ms: Option<u64>,
}

pub(super) struct PersistedTurn {
    /// This round's persisted assistant row `(id, text)`: the Finish
    /// envelope reads it as the final message, while a copy feeds the
    /// run-wide last-assistant record for interrupted paths.
    pub(super) assistant: Option<(String, String)>,
    pub(super) tool_calls: Vec<ToolCall>,
    pub(super) finish_reason: FinishReason,
}

pub(super) async fn collect(
    ctx: &AgentContext,
    handle: &RunHandle,
    request: ChatRequest,
    usage: &mut UsageAccumulator,
) -> Result<ModelTurn, LoopStop> {
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
            return Err(LoopStop::Cancelled);
        }
    };
    let mut event_stream = match stream_result {
        Ok(s) => s,
        Err(e) => {
            let message = format!("LLM error: {e}");
            error!("Failed to start stream: {e}");
            return Err(LoopStop::stream_failed(message));
        }
    };

    let mut current_text = String::new();
    let mut reasoning_text = String::new();
    // Preserve block order, including authenticated reasoning. Display
    // deltas become a durable signed block when the adapter completes it.
    let mut assistant_parts: Vec<ContentPart> = Vec::new();
    let mut reasoning_started_at: Option<Instant> = None;
    let mut reasoning_ended_at: Option<Instant> = None;
    let mut pending_tool_calls: Vec<ToolCall> = Vec::new();
    let mut finish_reason = None;
    let mut last_usage: Option<Usage> = None;
    // input+output of the request currently streaming; committed as the
    // usage anchor once the assistant message lands in full_messages.
    let mut pending_anchor: Option<u64> = None;

    loop {
        if handle.hub.is_overflowed() {
            return Err(LoopStop::EventBudget);
        }
        tokio::select! {
            event = event_stream.next() => {
                let Some(event) = event else { break };
                // Thinking ends at the first non-reasoning stream event so
                // the UI duration covers thinking only, not the answer
                // that follows it.
                if reasoning_started_at.is_some()
                    && reasoning_ended_at.is_none()
                    && !matches!(event, Ok(StreamEvent::ReasoningDelta(_) | StreamEvent::ReasoningBlock(_)))
                {
                    reasoning_ended_at = Some(Instant::now());
                }
                match event {
                    Ok(StreamEvent::TextDelta(text)) => {
                        current_text.push_str(&text);
                        if let Some(ContentPart::Text(previous)) = assistant_parts.last_mut() {
                            previous.push_str(&text);
                        } else if !text.is_empty() {
                            assistant_parts.push(ContentPart::Text(text.clone()));
                        }
                        handle.hub.publish(AgentEvent::TextDelta(text));
                    }
                    Ok(StreamEvent::ReasoningDelta(text)) => {
                        reasoning_started_at.get_or_insert_with(Instant::now);
                        reasoning_text.push_str(&text);
                        if let Some(ContentPart::Reasoning(previous)) = assistant_parts.last_mut() {
                            previous.push_str(&text);
                        } else if !text.is_empty() {
                            assistant_parts.push(ContentPart::Reasoning(text.clone()));
                        }
                        handle.hub.publish(AgentEvent::ReasoningDelta(text));
                    }
                    Ok(StreamEvent::ReasoningBlock(block)) => {
                        if matches!(assistant_parts.last(), Some(ContentPart::Reasoning(_))) {
                            assistant_parts.pop();
                        }
                        assistant_parts.push(ContentPart::ReasoningBlock(block));
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
                        assistant_parts.push(ContentPart::ToolCall(tc.clone()));
                        pending_tool_calls.push(tc);
                    }
                    Ok(StreamEvent::Usage(event_usage)) => {
                        let previous_input = last_usage.as_ref().and_then(request_input_tokens).unwrap_or(0);
                        let previous_output = last_usage.as_ref().and_then(|usage| usage.output_tokens).unwrap_or(0);
                        // Providers may split input/cache counts and final output across events.
                        let snapshot = last_usage.get_or_insert_with(Usage::default);
                        snapshot.input_tokens = event_usage.input_tokens.or(snapshot.input_tokens);
                        snapshot.output_tokens = event_usage.output_tokens.or(snapshot.output_tokens);
                        snapshot.reasoning_tokens = event_usage.reasoning_tokens.or(snapshot.reasoning_tokens);
                        snapshot.cached_input_tokens = event_usage.cached_input_tokens.or(snapshot.cached_input_tokens);
                        snapshot.provider_details.extend(event_usage.provider_details.clone());
                        let input = request_input_tokens(snapshot);
                        // Run aggregates replace this request's cumulative counts instead of adding each event.
                        usage.seen = true;
                        usage.total.input_tokens = usage.total.input_tokens - u64::from(previous_input)
                            + u64::from(input.unwrap_or(0));
                        usage.total.output_tokens = usage.total.output_tokens - u64::from(previous_output)
                            + u64::from(snapshot.output_tokens.unwrap_or(0));
                        // Provider-truth anchor for the NEXT estimate:
                        // everything after this request is a local
                        // increment on top of these tokens.
                        pending_anchor = input.filter(|input| *input > 0).map(|input| {
                            u64::from(input) + u64::from(snapshot.output_tokens.unwrap_or(0))
                        });
                        handle.hub.publish(AgentEvent::Usage {
                            input_tokens: input,
                            output_tokens: snapshot.output_tokens,
                            reasoning_tokens: snapshot.reasoning_tokens,
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
                        return Err(LoopStop::stream_failed(message));
                    }
                }
            }
            _ = handle.cancel.cancelled() => {
                info!("Agent loop cancelled during LLM streaming");
                return Err(LoopStop::Cancelled);
            }
        }
    }

    if finish_reason.is_none() {
        let message = "Model stream ended before a completion event".to_string();
        return Err(LoopStop::failed(message));
    }

    let reasoning_duration_ms = reasoning_started_at
        .filter(|_| !reasoning_text.is_empty())
        .map(|started| {
            reasoning_ended_at
                .unwrap_or_else(Instant::now)
                .saturating_duration_since(started)
                .as_millis() as u64
        });
    Ok(ModelTurn {
        message: Message {
            role: Role::Assistant,
            parts: assistant_parts,
        },
        text: current_text,
        tool_calls: pending_tool_calls,
        finish_reason: finish_reason.expect("completion was checked above"),
        usage: last_usage,
        anchor: pending_anchor,
        reasoning_duration_ms,
    })
}

pub(super) async fn persist(
    ctx: &AgentContext,
    conv_id: &str,
    context: &mut ContextState,
    turn: ModelTurn,
) -> Result<PersistedTurn, LoopStop> {
    let mut reasoning_details = turn
        .reasoning_duration_ms
        .map(|ms| {
            // UI-only thinking duration for the frontend card; stored in
            // details_json so it never enters the model-visible parts.
            json!({ "reasoning_duration_ms": ms })
        })
        .unwrap_or_else(|| json!({}));
    // Anchor bookkeeping rides in details_json too: the generation of
    // the prompt THIS request runs under, so a later assembly can reject
    // usage anchors measured under a different prompt (an in-flight
    // request completing after a `/reload` keeps the OLD generation here
    // and is correctly rejected by the new entry).
    reasoning_details["prompt_generation"] = json!(ctx.prompt_generation);
    // Keep the request's input gate with its usage: later COW replay
    // must deduct only content that THIS model actually received.
    reasoning_details["input_modalities"] = json!(ctx.input_modalities);
    reasoning_details["projection"] = json!(context.projection());
    if let Some(usage) = &turn.usage {
        reasoning_details["usage"] = usage_details(usage);
    }
    let reasoning_details = Some(reasoning_details);
    // Parts and text were assembled together while streaming. Move the
    // message into context after persistence, keeping its text for the
    // run-outcome bookkeeping and final-message record below.

    let assistant = if turn.message.parts.is_empty() {
        None
    } else {
        // On success the entry lands at the ordering the insert returned;
        // a failed durable append stops the run before any new side effect.
        let stored = msg_db::insert_message(
            &ctx.pool,
            conv_id,
            &turn.message,
            None,
            None,
            reasoning_details.as_ref(),
        )
        .await
        .map_err(|error| {
            LoopStop::failed(format!("Failed to persist assistant message: {error}"))
        })?;
        if let Some(usage) = turn.usage
            && let Err(error) = msg_db::update_message_usage(
                &ctx.pool,
                &stored.id,
                request_input_tokens(&usage),
                usage.output_tokens,
                usage.reasoning_tokens,
            )
            .await
        {
            error!("Failed to persist usage for message {}: {error}", stored.id);
        }
        context.append(turn.message, stored.ordering);
        // Commit the anchor from this request's usage now that the
        // message sits at its final index. (Skipped naturally when the
        // stream reported no usage — the old anchor stays valid.)
        if let Some(tokens) = turn.anchor {
            context.set_anchor(tokens, ctx.prompt_generation);
        }
        Some((stored.id, turn.text))
    };
    Ok(PersistedTurn {
        assistant,
        tool_calls: if matches!(turn.finish_reason, FinishReason::ToolCalls) {
            turn.tool_calls
        } else {
            Vec::new()
        },
        finish_reason: turn.finish_reason,
    })
}

fn cache_creation_tokens(usage: &Usage) -> Option<u64> {
    let raw = usage.provider_details.get("raw");
    let creation = usage
        .provider_details
        .get("cache_creation_input_tokens")
        .and_then(serde_json::Value::as_u64);
    let initial_creation = raw
        .and_then(|v| v.get("cache_creation_input_tokens"))
        .and_then(serde_json::Value::as_u64);
    match (creation, initial_creation) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

/// Anthropic excludes cache reads/writes from input_tokens; OpenAI-compatible providers include them.
fn request_input_tokens(usage: &Usage) -> Option<u32> {
    let creation = cache_creation_tokens(usage);
    let raw = usage.provider_details.get("raw");
    let separate_cache =
        creation.is_some() || raw.and_then(|v| v.get("cache_read_input_tokens")).is_some();
    usage.input_tokens.and_then(|input| {
        let cache = if separate_cache {
            u64::from(usage.cached_input_tokens.unwrap_or(0)).saturating_add(creation.unwrap_or(0))
        } else {
            0
        };
        u32::try_from(u64::from(input).saturating_add(cache)).ok()
    })
}

fn usage_details(usage: &Usage) -> serde_json::Value {
    json!({
        "input_tokens": request_input_tokens(usage),
        "output_tokens": usage.output_tokens,
        "reasoning_tokens": usage.reasoning_tokens,
        "cached_input_tokens": usage.cached_input_tokens,
        "cache_creation_input_tokens": cache_creation_tokens(usage),
    })
}
