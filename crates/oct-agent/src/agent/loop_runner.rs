use std::collections::HashSet;

use futures_util::stream::FuturesUnordered;
use futures_util::StreamExt;
use serde_json::Value;
use tracing::{error, info};

use oct_llm_provider::core::{
    ContentPart, FinishReason, Message, Role, StreamEvent, ToolCall, ToolResult,
};
use oct_llm_provider::core::GenerateOptions;
use oct_llm_provider::model::ChatRequest;

use crate::agent::{AgentContext, AgentEvent, RunHandle};
use crate::db::messages as msg_db;
use crate::tools;

pub async fn run_agent_loop(
    ctx: AgentContext,
    conv_id: String,
    messages: Vec<Message>,
    handle: RunHandle,
) {
    let mut full_messages = vec![Message::text(Role::System, &ctx.system_prompt)];
    full_messages.extend(messages);

    let tool_specs: Vec<_> = ctx
        .tools
        .iter()
        .map(|t| tools::to_tool_spec(t.as_ref()))
        .collect();

    loop {
        if handle.cancel.is_cancelled() {
            let _ = handle.event_tx.send(AgentEvent::Cancelled);
            return;
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
                let _ = handle
                    .event_tx
                    .send(AgentEvent::Error(format!("LLM error: {e}")));
                return;
            }
        };

        let mut current_text = String::new();
        let mut pending_tool_calls: Vec<ToolCall> = Vec::new();
        let mut finish_reason = FinishReason::Stop;
        let mut last_usage: Option<(Option<u32>, Option<u32>, Option<u32>)> = None;

        loop {
            tokio::select! {
                event = event_stream.next() => {
                    let Some(event) = event else { break };
                    match event {
                        Ok(StreamEvent::TextDelta(text)) => {
                            current_text.push_str(&text);
                            let _ = handle.event_tx.send(AgentEvent::TextDelta(text));
                        }
                        Ok(StreamEvent::ReasoningDelta(text)) => {
                            let _ = handle
                                .event_tx
                                .send(AgentEvent::ReasoningDelta(text));
                        }
                        Ok(StreamEvent::ToolCallDelta { .. }) => {}
                        Ok(StreamEvent::ToolCall(tc)) => {
                            pending_tool_calls.push(tc);
                        }
                        Ok(StreamEvent::Usage(usage)) => {
                            last_usage = Some((
                                usage.input_tokens,
                                usage.output_tokens,
                                usage.reasoning_tokens,
                            ));
                            let _ = handle.event_tx.send(AgentEvent::Usage {
                                input_tokens: usage.input_tokens,
                                output_tokens: usage.output_tokens,
                                reasoning_tokens: usage.reasoning_tokens,
                            });
                        }
                        Ok(StreamEvent::Finish(reason)) => {
                            finish_reason = reason;
                        }
                        Ok(StreamEvent::ToolResult(_)) => {}
                        Err(e) => {
                            error!("Stream error: {e}");
                            let _ = handle.event_tx.send(AgentEvent::Error(format!(
                                "Stream error: {e}"
                            )));
                            return;
                        }
                    }
                }
                _ = handle.cancel.cancelled() => {
                    info!("Agent loop cancelled during LLM streaming");
                    let _ = handle.event_tx.send(AgentEvent::Cancelled);
                    return;
                }
            }
        }

        let mut assistant_parts: Vec<ContentPart> = Vec::new();
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
            match msg_db::insert_message(&ctx.pool, &conv_id, &assistant_msg).await {
                Ok(stored) => {
                    if let Some((input, output, reasoning)) = last_usage {
                        if let Err(e) = msg_db::update_message_usage(
                            &ctx.pool,
                            &stored.id,
                            input,
                            output,
                            reasoning,
                        )
                        .await
                        {
                            error!("Failed to persist usage for message {}: {e}", stored.id);
                        }
                    }
                }
                Err(e) => {
                    error!("Failed to persist assistant message: {e}");
                }
            }
            full_messages.push(assistant_msg);
        }

        if !has_tool_calls {
            info!("Agent loop finished with reason: {:?}", finish_reason);
            let _ = handle.event_tx.send(AgentEvent::Finish);
            return;
        }

        let indexed_calls: Vec<(usize, ToolCall)> = pending_tool_calls
            .into_iter()
            .enumerate()
            .collect();
        let total = indexed_calls.len();

        let mut futures = FuturesUnordered::new();
        for (idx, tc) in &indexed_calls {
            let tc = tc.clone();
            let idx = *idx;
            let tools = ctx.tools.clone();
            let args: Value = serde_json::from_str(&tc.arguments)
                .unwrap_or(Value::Object(Default::default()));

            futures.push(async move {
                let tool = tools.iter().find(|t| t.name() == tc.name);
                let output = match tool {
                    Some(t) => match t.execute(args).await {
                        Ok(o) => o,
                        Err(e) => crate::tools::ToolOutput::error(format!(
                            "Tool execution error: {e}"
                        )),
                    },
                    None => {
                        crate::tools::ToolOutput::error(format!("Unknown tool: {}", tc.name))
                    }
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
                            let _ = handle.event_tx.send(AgentEvent::ToolCallStart {
                                id: tc.id.clone(),
                                name: tc.name.clone(),
                                arguments: tc.arguments.clone(),
                            });
                            let _ = handle.event_tx.send(AgentEvent::ToolResult {
                                call_id: tc.id.clone(),
                                content: output.content.clone(),
                                is_error: output.is_error,
                            });

                            let tool_msg = Message {
                                role: Role::Tool,
                                parts: vec![ContentPart::ToolResult(ToolResult {
                                    call_id: tc.id.clone(),
                                    content: Value::String(output.content),
                                    is_error: output.is_error,
                                })],
                            };
                            if let Err(e) =
                                msg_db::insert_message(&ctx.pool, &conv_id, &tool_msg).await
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
                        msg_db::insert_message(&ctx.pool, &conv_id, &cancel_msg).await
                    {
                        error!("Failed to persist cancel result: {e}");
                    }
                }
            }
            let _ = handle.event_tx.send(AgentEvent::Cancelled);
            return;
        }
    }
}
