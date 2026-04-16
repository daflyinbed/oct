use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use axum::Json;
use futures_util::StreamExt;
use serde::Deserialize;
use std::convert::Infallible;
use std::sync::Arc;
use std::path::PathBuf;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_util::sync::CancellationToken;
use utoipa::ToSchema;

use oct_llm_provider::core::{Message, Role};

use super::AppState;
use crate::agent::loop_runner::run_agent_loop;
use crate::agent::prompt::system_prompt;
use crate::agent::{AgentContext, RunHandle};
use crate::db::{conversations as conv_db, messages as msg_db};
use crate::tools;

#[derive(Debug, Deserialize, ToSchema)]
pub struct SendMessageRequest {
    pub content: String,
}

#[utoipa::path(
    post,
    path = "/conversations/{id}/messages",
    params(("id" = String, Path, description = "Conversation ID")),
    request_body = SendMessageRequest,
    responses(
        (status = 200, description = "SSE stream of agent events"),
        (status = 404, description = "Conversation not found"),
        (status = 409, description = "Agent is already running for this conversation")
    ),
    tag = "chat"
)]
pub async fn send_message(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<SendMessageRequest>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    if state.sessions.contains_key(&id) {
        return Err((
            StatusCode::CONFLICT,
            "Agent is already running for this conversation".into(),
        ));
    }

    let conversation = conv_db::get_conversation(&state.pool, &id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "Conversation not found".into()))?;

    let user_msg = Message::text(Role::User, &req.content);
    msg_db::insert_message(&state.pool, &id, &user_msg)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let messages = msg_db::load_messages_for_llm(&state.pool, &id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let model = state
        .registry
        .resolve_chat_model(&conversation.provider_spec)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let (event_tx, _) = broadcast::channel(256);
    let cancel = CancellationToken::new();
    let handle = RunHandle {
        event_tx: event_tx.clone(),
        cancel,
    };

    let rx = handle.subscribe();
    state.sessions.insert(id.clone(), handle.clone());

    let ctx = AgentContext {
        model: Arc::from(model),
        tools: Arc::new(tools::default_tools(PathBuf::from(&conversation.working_dir))),
        pool: state.pool.clone(),
        system_prompt: system_prompt(&PathBuf::from(&conversation.working_dir)),
    };

    let conv_id = id.clone();
    let sessions = state.sessions.clone();

    tokio::spawn(async move {
        run_agent_loop(ctx, conv_id.clone(), messages, handle).await;
        sessions.remove(&conv_id);
    });

    let stream = BroadcastStream::new(rx).map(|e| match e {
        Ok(e) => Ok::<_, Infallible>(
            Event::default()
                .json_data(&e)
                .unwrap_or_else(|_| Event::default()),
        ),
        Err(_) => Ok(Event::default()),
    });

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

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
    conv_db::get_conversation(&state.pool, &id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    let messages = msg_db::list_messages(&state.pool, &id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(messages))
}
