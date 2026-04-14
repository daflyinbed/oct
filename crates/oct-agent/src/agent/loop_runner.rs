use anyhow::Result;
use async_stream::try_stream;
use futures_core::Stream;
use futures_util::StreamExt;
use std::pin::Pin;
use std::sync::Arc;
use tracing::{error, info};

use oct_llm_provider::core::{
    ContentPart, FinishReason, Message, Role, StreamEvent, ToolCall, ToolResult,
};
use oct_llm_provider::model::{ChatModel, ChatRequest};
use oct_llm_provider::core::GenerateOptions;

use crate::agent::AgentEvent;
use crate::tools::{self, AgentTool};

/// Run the agent loop, yielding `AgentEvent`s as a stream.
///
/// The loop continues until the model stops requesting tool calls (no iteration limit).
pub fn run_agent_loop(
    model: Arc<dyn ChatModel>,
    tools: Arc<Vec<Box<dyn AgentTool>>>,
    mut messages: Vec<Message>,
    system_prompt: String,
) -> Pin<Box<dyn Stream<Item = Result<AgentEvent>> + Send>> {
    Box::pin(try_stream! {
        // Prepend system message
        let mut full_messages = vec![Message::text(Role::System, &system_prompt)];
        full_messages.append(&mut messages);

        let tool_specs: Vec<_> = tools.iter().map(|t| tools::to_tool_spec(t.as_ref())).collect();

        loop {
            let request = ChatRequest {
                messages: full_messages.clone(),
                tools: tool_specs.clone(),
                options: GenerateOptions::default(),
            };

            let stream_result = model.stream(request).await;
            let mut event_stream = match stream_result {
                Ok(s) => s,
                Err(e) => {
                    error!("Failed to start stream: {e}");
                    yield AgentEvent::Error(format!("LLM error: {e}"));
                    return;
                }
            };

            let mut assistant_parts: Vec<ContentPart> = Vec::new();
            let mut pending_tool_calls: Vec<ToolCall> = Vec::new();
            let mut finish_reason = FinishReason::Stop;
            let mut current_text = String::new();

            while let Some(event) = event_stream.next().await {
                match event {
                    Ok(StreamEvent::TextDelta(text)) => {
                        current_text.push_str(&text);
                        yield AgentEvent::TextDelta(text);
                    }
                    Ok(StreamEvent::ReasoningDelta(text)) => {
                        yield AgentEvent::ReasoningDelta(text);
                    }
                    Ok(StreamEvent::ToolCallDelta { .. }) => {
                        // Deltas are accumulated internally; we wait for complete ToolCall events
                    }
                    Ok(StreamEvent::ToolCall(tc)) => {
                        pending_tool_calls.push(tc);
                    }
                    Ok(StreamEvent::Usage(usage)) => {
                        yield AgentEvent::Usage {
                            input_tokens: usage.input_tokens,
                            output_tokens: usage.output_tokens,
                            reasoning_tokens: usage.reasoning_tokens,
                        };
                    }
                    Ok(StreamEvent::Finish(reason)) => {
                        finish_reason = reason;
                    }
                    Ok(StreamEvent::ToolResult(_)) => {
                        // Should not appear from model stream
                    }
                    Err(e) => {
                        error!("Stream error: {e}");
                        yield AgentEvent::Error(format!("Stream error: {e}"));
                        return;
                    }
                }
            }

            // Build assistant message from collected parts
            if !current_text.is_empty() {
                assistant_parts.push(ContentPart::Text(current_text.clone()));
            }
            for tc in &pending_tool_calls {
                assistant_parts.push(ContentPart::ToolCall(tc.clone()));
            }

            if !assistant_parts.is_empty() {
                full_messages.push(Message {
                    role: Role::Assistant,
                    parts: assistant_parts,
                });
            }

            // If no tool calls, we're done
            if pending_tool_calls.is_empty() || !matches!(finish_reason, FinishReason::ToolCalls) {
                info!("Agent loop finished with reason: {:?}", finish_reason);
                yield AgentEvent::Finish;
                return;
            }

            // Execute tool calls
            let mut tool_results = Vec::new();
            for tc in &pending_tool_calls {
                yield AgentEvent::ToolCallStart {
                    id: tc.id.clone(),
                    name: tc.name.clone(),
                    arguments: tc.arguments.clone(),
                };

                let args: serde_json::Value = serde_json::from_str(&tc.arguments)
                    .unwrap_or(serde_json::Value::Object(Default::default()));

                let tool = tools.iter().find(|t| t.name() == tc.name);
                let result = match tool {
                    Some(tool) => match tool.execute(args).await {
                        Ok(output) => output,
                        Err(e) => crate::tools::ToolOutput::error(format!("Tool execution error: {e}")),
                    },
                    None => crate::tools::ToolOutput::error(format!("Unknown tool: {}", tc.name)),
                };

                yield AgentEvent::ToolResult {
                    call_id: tc.id.clone(),
                    content: result.content.clone(),
                    is_error: result.is_error,
                };

                tool_results.push(ToolResult {
                    call_id: tc.id.clone(),
                    content: serde_json::Value::String(result.content),
                    is_error: result.is_error,
                });
            }

            // Add tool result messages
            for tr in tool_results {
                full_messages.push(Message {
                    role: Role::Tool,
                    parts: vec![ContentPart::ToolResult(tr)],
                });
            }

            // Loop continues — no iteration limit
        }
    })
}
