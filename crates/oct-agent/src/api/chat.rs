use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use axum::Json;
use futures_util::StreamExt;
use serde::Deserialize;
use std::convert::Infallible;
use std::sync::Arc;
use tokio_stream::wrappers::ReceiverStream;
use utoipa::ToSchema;

use oct_llm_provider::core::{ContentPart, Message, Role};

use super::AppState;
use crate::agent::loop_runner::run_agent_loop;
use crate::agent::prompt::system_prompt;
use crate::agent::AgentEvent;
use crate::db::{conversations as conv_db, messages as msg_db};
use crate::tools;

#[derive(Debug, Deserialize, ToSchema)]
pub struct SendMessageRequest {
    /// The user's message content.
    pub content: String,
}

/// Send a message and stream the agent's response via SSE.
#[utoipa::path(
    post,
    path = "/conversations/{id}/messages",
    params(("id" = String, Path, description = "Conversation ID")),
    request_body = SendMessageRequest,
    responses(
        (status = 200, description = "SSE stream of agent events"),
        (status = 404, description = "Conversation not found")
    ),
    tag = "chat"
)]
pub async fn send_message(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<SendMessageRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    // Load conversation
    let conversation = conv_db::get_conversation(&state.pool, &id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    // Save user message
    let user_msg = Message::text(Role::User, &req.content);
    msg_db::insert_message(&state.pool, &id, &user_msg)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Load conversation history
    let messages = msg_db::load_messages_for_llm(&state.pool, &id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Resolve model from the conversation's provider_spec
    let model = state
        .registry
        .resolve_chat_model(&conversation.provider_spec)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let model = Arc::from(model);
    let working_dir = std::path::PathBuf::from(&conversation.working_dir);
    let agent_tools = Arc::new(tools::default_tools(working_dir.clone()));
    let prompt = system_prompt(&working_dir);

    // Create channel for SSE
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(256);

    let pool = state.pool.clone();
    let conv_id = id.clone();

    // Spawn agent loop
    tokio::spawn(async move {
        let mut stream = run_agent_loop(model, agent_tools, messages, prompt);

        let mut assistant_text = String::new();
        let mut tool_calls = Vec::new();
        let mut tool_results = Vec::new();

        while let Some(event_result) = stream.next().await {
            let event = match event_result {
                Ok(e) => e,
                Err(e) => {
                    let err_event = AgentEvent::Error(e.to_string());
                    let _ = tx
                        .send(Ok(Event::default()
                            .json_data(&err_event)
                            .unwrap_or_else(|_| Event::default())))
                        .await;
                    break;
                }
            };

            // Track state for persistence
            match &event {
                AgentEvent::TextDelta(text) => {
                    assistant_text.push_str(text);
                }
                AgentEvent::ToolCallStart { id, name, arguments } => {
                    tool_calls.push(oct_llm_provider::core::ToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        arguments: arguments.clone(),
                    });
                }
                AgentEvent::ToolResult {
                    call_id,
                    content,
                    is_error,
                } => {
                    tool_results.push(oct_llm_provider::core::ToolResult {
                        call_id: call_id.clone(),
                        content: serde_json::Value::String(content.clone()),
                        is_error: *is_error,
                    });
                }
                AgentEvent::Usage {
                    input_tokens,
                    output_tokens,
                    reasoning_tokens,
                } => {
                    let _ = msg_db::insert_usage(
                        &pool,
                        &conv_id,
                        *input_tokens,
                        *output_tokens,
                        *reasoning_tokens,
                    )
                    .await;
                }
                AgentEvent::Finish => {
                    // Persist the final assistant message if there's content
                    if !assistant_text.is_empty() || !tool_calls.is_empty() {
                        let mut parts = Vec::new();
                        if !assistant_text.is_empty() {
                            parts.push(ContentPart::Text(assistant_text.clone()));
                        }
                        for tc in &tool_calls {
                            parts.push(ContentPart::ToolCall(tc.clone()));
                        }
                        let assistant_msg = Message {
                            role: Role::Assistant,
                            parts,
                        };
                        let _ = msg_db::insert_message(&pool, &conv_id, &assistant_msg).await;
                    }
                    // Persist tool results
                    for tr in &tool_results {
                        let tool_msg = Message {
                            role: Role::Tool,
                            parts: vec![ContentPart::ToolResult(tr.clone())],
                        };
                        let _ = msg_db::insert_message(&pool, &conv_id, &tool_msg).await;
                    }
                }
                _ => {}
            }

            // Send SSE event
            let sse_event = Event::default()
                .json_data(&event)
                .unwrap_or_else(|_| Event::default());

            if tx.send(Ok(sse_event)).await.is_err() {
                break; // Client disconnected
            }
        }
    });

    Ok(Sse::new(ReceiverStream::new(rx)).keep_alive(KeepAlive::default()))
}

/// Get all messages in a conversation.
#[utoipa::path(
    get,
    path = "/conversations/{id}/messages",
    params(("id" = String, Path, description = "Conversation ID")),
    responses(
        (status = 200, description = "List of messages", body = Vec<msg_db::StoredMessage>),
        (status = 404, description = "Conversation not found")
    ),
    tag = "chat"
)]
pub async fn get_messages(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<msg_db::StoredMessage>>, StatusCode> {
    // Verify conversation exists
    conv_db::get_conversation(&state.pool, &id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    let messages = msg_db::list_messages(&state.pool, &id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(messages))
}
