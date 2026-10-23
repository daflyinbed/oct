//! Concurrent tool dispatch with durable, per-completion result settlement.
use super::LoopStop;
use crate::agent::{AgentContext, RunHandle};
use crate::context::ContextState;
use crate::db::messages as msg_db;
use crate::events::AgentEvent;
use oct_llm_provider::core::{ContentPart, Message, Role, ToolCall, ToolResult};
use serde_json::Value;
use std::collections::HashSet;
use tracing::{error, info};

pub(super) async fn execute(
    ctx: &AgentContext,
    conv_id: &str,
    handle: &RunHandle,
    context: &mut ContextState,
    calls: Vec<ToolCall>,
) -> Result<(), LoopStop> {
    let indexed_calls: Vec<(usize, ToolCall)> = calls.into_iter().enumerate().collect();
    let total = indexed_calls.len();

    // A completed result is persisted while siblings are still running.
    // Poll siblings independently: a sibling may own a SQLite write
    // transaction that must keep progressing while this loop writes.
    // JoinSet aborts unfinished tool futures when this run is dropped.
    let mut futures = tokio::task::JoinSet::new();
    // ToolContext per call, parallel to `indexed_calls` (same idx). Kept
    // here so the completion branch can flush the smoother's remainder
    // before the final ToolResult event.
    let mut contexts: Vec<crate::tool_contract::ToolContext> = Vec::with_capacity(total);

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
        let tool_ctx = crate::tool_contract::ToolContext::new(
            tc.id.clone(),
            handle.hub.clone(),
            handle.cancel.clone(),
        );
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

        futures.spawn(async move {
            let tool = tools.iter().find(|t| t.name() == tc.name);
            let output = crate::tool_host::dispatch::direct(
                tool,
                &tc.name,
                args,
                &tool_ctx,
                &permission,
                &gate_hub,
            )
            .await;
            (idx, tc, output)
        });
    }

    let mut completed: HashSet<usize> = HashSet::new();

    'tool_results: loop {
        tokio::select! {
            result = futures.join_next() => {
                match result {
                    Some(result) => {
                        let (idx, tc, output) = result.expect("tool execution task panicked");
                        // Flush the smoother's buffered remainder first so
                        // the frontend has seen all streamed output when
                        // the closing ToolResult arrives.
                        contexts[idx].flush();

                        // Normalize and externalize media once, regardless
                        // of which tool transport produced the output.
                        let prepared = if let Some(service) = &ctx.attachments {
                            tokio::select! {
                                prepared = service.prepare_tool_result(conv_id, &tc.id, output.clone()) => Some(prepared),
                                _ = handle.cancel.cancelled() => {
                                    futures.shutdown().await;
                                    break 'tool_results;
                                }
                            }
                        } else { None };
                        let output = prepared.as_ref().map(|prepared| prepared.output.clone()).unwrap_or(output);
                        let result_event = AgentEvent::ToolResult {
                            call_id: tc.id.clone(),
                            content: output.content.clone(),
                            is_error: output.is_error,
                            images: output.images.clone(),
                            details: output.details.clone(),
                        };

                        let tool_msg = prepared.as_ref().map(|prepared| prepared.message.clone()).unwrap_or_else(|| Message {
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
                        });
                        // details are UI-only: persisted in their own
                        // column, never inside parts_json.
                        let persisted = if let Some(prepared) = &prepared {
                            crate::db::persist_critical(&ctx.pool, || prepared.commit()).await
                        } else {
                            crate::db::persist_critical(&ctx.pool, || msg_db::insert_message(
                                &ctx.pool, conv_id, &tool_msg, None, None, output.details.as_ref(),
                            )).await
                        };
                        let ordering = match persisted
                        {
                            Ok(stored) => stored.ordering,
                            Err(e) => {
                                let message = format!("Failed to persist tool result: {e}");
                                return Err(LoopStop::failed(message));
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
                // Drop in-flight transactions before persisting cancelled
                // results, and preserve the run's ownership of tool work.
                futures.shutdown().await;
                break;
            }
        }
    }

    if handle.cancel.is_cancelled() {
        for (idx, tc) in &indexed_calls {
            if !completed.contains(idx) {
                let output = ctx
                    .tools
                    .iter()
                    .find(|t| t.name() == tc.name)
                    .and_then(|t| t.interrupted_output(&tc.id))
                    .unwrap_or_else(|| crate::tool_contract::ToolOutput::error("用户取消了执行"));
                let cancel_msg = Message {
                    role: Role::Tool,
                    parts: vec![ContentPart::ToolResult(ToolResult {
                        call_id: tc.id.clone(),
                        content: Value::String(output.content),
                        is_error: true,
                        images: Vec::new(),
                    })],
                };
                if let Err(e) = msg_db::insert_message(
                    &ctx.pool,
                    conv_id,
                    &cancel_msg,
                    None,
                    None,
                    output.details.as_ref(),
                )
                .await
                {
                    error!("Failed to persist cancel result: {e}");
                }
            }
        }
        return Err(LoopStop::Cancelled);
    }
    Ok(())
}
