use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use utoipa::ToSchema;

use oct_llm_provider::core::{Message, Role};
use tracing::debug;

use super::AppState;
use super::error::{ApiResult, AppError};
use crate::agent::loop_runner::run_agent_loop;
use crate::agent::prompt::system_prompt;
use crate::agent::title;
use crate::agent::{AgentContext, AgentEvent, EventHub, RunHandle};
use crate::db::{
    conversations as conv_db, messages as msg_db, projects as project_db, providers as provider_db,
};
use crate::tools;

#[derive(Debug, Deserialize, ToSchema)]
pub struct SendMessageRequest {
    pub content: String,
    pub provider_spec: String,
}

/// Whether an agent run is currently active for a conversation.
#[derive(Debug, Serialize, ToSchema)]
pub struct RunningStatus {
    pub running: bool,
}

fn parse_provider_spec(spec: &str) -> Result<(&str, &str), AppError> {
    spec.split_once(':').ok_or_else(|| {
        AppError::BadRequest(format!(
            "Invalid provider_spec format: '{}'. Expected 'provider_id:model_id'",
            spec
        ))
    })
}

async fn resolve_model(
    state: &AppState,
    provider_spec: &str,
) -> Result<(Box<dyn oct_llm_provider::model::ChatModel>, String, String), AppError> {
    let spec = provider_spec.to_string();

    let (provider_id, model_id) = parse_provider_spec(&spec)?;

    let provider = provider_db::get_provider(&state.pool, provider_id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Provider '{}' not found", provider_id)))?;

    if provider.api_key.is_empty() {
        return Err(AppError::BadRequest(format!(
            "Provider '{}' has no API key configured",
            provider_id
        )));
    }

    let model = match provider.adapter_type.as_str() {
        "anthropic" => state.registry.resolve_chat_model(&spec)?,
        "openai_compatible" => {
            let base_url = provider.base_url.as_deref().unwrap_or("");
            state.registry.resolve_custom_openai_model(
                provider_id,
                model_id,
                base_url,
                &provider.api_key,
            )?
        }
        other => {
            return Err(AppError::BadRequest(format!(
                "Unknown adapter_type: '{}'",
                other
            )));
        }
    };

    Ok((model, provider_id.to_string(), model_id.to_string()))
}

/// Shared tail of the chat endpoints: take the session entry the caller
/// inserted (a real run handle with a fresh event hub), subscribe to the hub,
/// spawn the agent loop, and return the SSE stream of the run's events.
/// `state.sessions` must already hold the entry for `id` (so concurrent
/// starts get a 409); the entry stays until the loop returns, which keeps
/// GET /run reporting `running` and GET /events able to replay.
fn start_agent_run(
    state: &AppState,
    id: &str,
    project: &project_db::Project,
    chat_model: Box<dyn oct_llm_provider::model::ChatModel>,
    messages: Vec<Message>,
) -> ApiResult<Response> {
    let handle = state
        .sessions
        .get(id)
        .map(|entry| entry.value().clone())
        .expect("caller inserted the session entry");

    // Subscribe before spawning so the response stream is live from the very
    // first event (the hub log would replay them anyway — subscribing first
    // is just the cheaper path).
    let stream = handle
        .hub
        .subscribe()
        .map(|e| Ok::<_, Infallible>(sse_event(&e)));

    let ctx = AgentContext {
        model: Arc::from(chat_model),
        tools: Arc::new(tools::default_tools(PathBuf::from(&project.working_dir))),
        pool: state.pool.clone(),
        system_prompt: system_prompt(&PathBuf::from(&project.working_dir)),
    };

    let conv_id = id.to_string();
    let sessions = state.sessions.clone();

    tokio::spawn(async move {
        run_agent_loop(ctx, conv_id.clone(), messages, handle).await;
        // Dropping the last run handle releases the hub: every subscriber
        // stream reaches EOF (the SSE bodies end), matching the old broadcast
        // termination semantics.
        sessions.remove(&conv_id);
    });

    Ok(Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response())
}

/// Serialize an AgentEvent as an SSE `data:` payload.
fn sse_event(e: &AgentEvent) -> Event {
    Event::default()
        .json_data(e)
        .unwrap_or_else(|_| Event::default())
}

/// Insert the session entry for a new run: a REAL handle with a fresh event
/// hub (not a placeholder channel). Unlike the previous placeholder scheme,
/// a GET /events arriving in the run-startup window already replays from the
/// same hub this run will publish to, so no handle swap is ever needed.
fn insert_session_entry(state: &AppState, id: &str) -> Result<(), AppError> {
    match state.sessions.entry(id.to_string()) {
        dashmap::mapref::entry::Entry::Occupied(_) => Err(AppError::Conflict(
            "Agent is already running for this conversation".into(),
        )),
        dashmap::mapref::entry::Entry::Vacant(entry) => {
            entry.insert(RunHandle {
                hub: Arc::new(EventHub::new()),
                cancel: CancellationToken::new(),
                start_message_id: None,
            });
            Ok(())
        }
    }
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
) -> ApiResult<Response> {
    insert_session_entry(&state, &id)?;

    let conversation = conv_db::get_conversation(&state.pool, &id)
        .await
        .map_err(AppError::from)?
        .ok_or_else(|| {
            state.sessions.remove(&id);
            AppError::NotFound("Conversation not found".into())
        })?;

    let project = project_db::get_project(&state.pool, &conversation.project_id)
        .await
        .map_err(|e| {
            state.sessions.remove(&id);
            AppError::from(e)
        })?
        .ok_or_else(|| {
            state.sessions.remove(&id);
            AppError::NotFound("Project not found".into())
        })?;

    let (chat_model, provider_id, model_id) = match resolve_model(&state, &req.provider_spec).await
    {
        Ok(m) => m,
        Err(e) => {
            state.sessions.remove(&id);
            return Err(e);
        }
    };

    // Poison guard: a previous run interrupted mid-tool (e.g. backend crash)
    // leaves assistant ToolCalls without tool messages; repairing BEFORE the
    // new user message is appended keeps the stored history a legal
    // conversation for the provider.
    if let Err(e) = msg_db::repair_dangling_tool_calls(&state.pool, &id).await {
        state.sessions.remove(&id);
        return Err(AppError::from(e));
    }

    let user_msg = Message::text(Role::User, &req.content);
    let stored_user = match msg_db::insert_message(
        &state.pool,
        &id,
        &user_msg,
        Some(&provider_id),
        Some(&model_id),
        None,
    )
    .await
    {
        Ok(m) => m,
        Err(e) => {
            state.sessions.remove(&id);
            return Err(AppError::from(e));
        }
    };

    // Record the run boundary on the (already live) session entry: the user
    // message row is the divide between DB history and this run's event log.
    // GET /events reports it as run_meta so a reconnecting frontend truncates
    // its history exactly here before replaying the run's events.
    if let Some(mut entry) = state.sessions.get_mut(&id) {
        entry.value_mut().start_message_id = Some(stored_user.id.clone());
    }

    let messages = match msg_db::load_messages_for_llm(&state.pool, &id).await {
        Ok(m) => m,
        Err(e) => {
            state.sessions.remove(&id);
            return Err(AppError::from(e));
        }
    };

    // One-shot title generation: an independent background task firing
    // alongside the run (never inside it). Guards: the conversation still
    // carries its default title, and the user message just persisted is its
    // first — a failed attempt is not retried on later turns. Spawned only
    // after every fallible step has succeeded, so an aborted send never
    // burns the one-shot on a message that was never stored. The weak hub
    // reference keeps a slow title call from delaying SSE EOF past the
    // run's end.
    if conversation.title_source == "default"
        && matches!(msg_db::count_user_messages(&state.pool, &id).await, Ok(1))
    {
        let hub = state
            .sessions
            .get(&id)
            .map(|entry| Arc::downgrade(&entry.value().hub));
        match resolve_model(&state, &req.provider_spec).await {
            Ok((model, _, _)) => {
                if let Some(hub) = hub {
                    title::spawn(title::TitleJob {
                        model,
                        pool: state.pool.clone(),
                        conversation_id: id.clone(),
                        first_user_content: req.content.clone(),
                        hub,
                    });
                }
            }
            // The run's own resolution succeeded above, so this is exotic
            // (e.g. the provider row vanished between calls) — skip quietly.
            Err(e) => debug!("skipping title generation: {e}"),
        }
    }

    start_agent_run(&state, &id, &project, chat_model, messages)
}

#[utoipa::path(
    post,
    path = "/conversations/{id}/resume",
    params(("id" = String, Path, description = "Conversation ID")),
    responses(
        (status = 200, description = "SSE stream of agent events for the resumed turn"),
        (status = 400, description = "No provider/model recorded for the conversation"),
        (status = 404, description = "Conversation not found"),
        (status = 409, description = "Nothing to resume, or agent is already running")
    ),
    tag = "chat"
)]
pub async fn resume_turn(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    insert_session_entry(&state, &id)?;

    let conversation = conv_db::get_conversation(&state.pool, &id)
        .await
        .map_err(AppError::from)?
        .ok_or_else(|| {
            state.sessions.remove(&id);
            AppError::NotFound("Conversation not found".into())
        })?;

    let project = project_db::get_project(&state.pool, &conversation.project_id)
        .await
        .map_err(|e| {
            state.sessions.remove(&id);
            AppError::from(e)
        })?
        .ok_or_else(|| {
            state.sessions.remove(&id);
            AppError::NotFound("Project not found".into())
        })?;

    // Read-only resumability probe: a dangling ToolCall OR a trailing
    // unanswered user message both mean the last turn never completed.
    // Nothing is persisted yet — an empty/answered conversation must 409
    // regardless of provider state.
    let dangling = match msg_db::find_dangling_tool_calls(&state.pool, &id).await {
        Ok(d) => d,
        Err(e) => {
            state.sessions.remove(&id);
            return Err(AppError::from(e));
        }
    };
    let last_is_user = match msg_db::last_message_is_user(&state.pool, &id).await {
        Ok(v) => v,
        Err(e) => {
            state.sessions.remove(&id);
            return Err(AppError::from(e));
        }
    };
    if dangling.is_empty() && !last_is_user {
        state.sessions.remove(&id);
        return Err(AppError::Conflict("Nothing to resume".into()));
    }

    // The resumed turn reuses the provider/model the conversation last ran
    // with; there is no request body to carry a fresh spec. Resolved before
    // the repair below: persisting placeholder rows and then failing to
    // resolve would leave the conversation permanently unresumable (nothing
    // dangling anymore, last message now a tool row → "Nothing to resume").
    let provider_spec = match msg_db::get_last_provider_spec(&state.pool, &id).await {
        Ok(Some((provider_id, model_id))) => format!("{provider_id}:{model_id}"),
        Ok(None) => {
            state.sessions.remove(&id);
            return Err(AppError::BadRequest(
                "Conversation has no recorded provider/model to resume with".into(),
            ));
        }
        Err(e) => {
            state.sessions.remove(&id);
            return Err(AppError::from(e));
        }
    };

    let (chat_model, _provider_id, _model_id) = match resolve_model(&state, &provider_spec).await {
        Ok(m) => m,
        Err(e) => {
            state.sessions.remove(&id);
            return Err(e);
        }
    };

    // Only now persist the repair (idempotent): placeholder ToolResults for
    // the dangling calls found above.
    if let Err(e) = msg_db::repair_dangling_tool_calls(&state.pool, &id).await {
        state.sessions.remove(&id);
        return Err(AppError::from(e));
    }

    // No new user message: the loop continues from the existing history.
    let messages = match msg_db::load_messages_for_llm(&state.pool, &id).await {
        Ok(m) => m,
        Err(e) => {
            state.sessions.remove(&id);
            return Err(AppError::from(e));
        }
    };

    start_agent_run(&state, &id, &project, chat_model, messages)
}

#[utoipa::path(
    post,
    path = "/conversations/{id}/cancel",
    params(("id" = String, Path, description = "Conversation ID")),
    responses(
        (status = 200, description = "Cancellation requested"),
        (status = 404, description = "No agent running for this conversation")
    ),
    tag = "chat"
)]
pub async fn cancel_agent(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    match state.sessions.get(&id) {
        Some(handle) => {
            handle.cancel.cancel();
            Ok(StatusCode::OK)
        }
        None => Err(AppError::NotFound(
            "No agent running for this conversation".into(),
        )),
    }
}

/// Poll probe for a reconnecting frontend (after a refresh): is a run still
/// active for this conversation?
#[utoipa::path(
    get,
    path = "/conversations/{id}/run",
    params(("id" = String, Path, description = "Conversation ID")),
    responses(
        (status = 200, description = "Whether an agent run is currently active", body = RunningStatus)
    ),
    tag = "chat"
)]
pub async fn get_run_status(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<RunningStatus>> {
    Ok(Json(RunningStatus {
        running: state.sessions.contains_key(&id),
    }))
}

/// Reconnect to a running agent: replay the run's events from its start,
/// then continue live. The frontend treats 404 as "the run already ended"
/// and simply refreshes its history from the DB.
#[utoipa::path(
    get,
    path = "/conversations/{id}/events",
    params(("id" = String, Path, description = "Conversation ID")),
    responses(
        (status = 200, description = "SSE stream: a synthetic run_meta event (when the run records its start message) followed by the run's full event log replayed from its start, then live events"),
        (status = 404, description = "No agent running for this conversation (the run has ended)")
    ),
    tag = "chat"
)]
pub async fn stream_run_events(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    // Clone what we need and drop the shard guard immediately; the returned
    // SSE stream must NOT keep the run handle alive, or the hub would never
    // be dropped when the run ends and the stream would never reach EOF.
    let (hub, start_message_id) = {
        let Some(handle) = state.sessions.get(&id) else {
            return Err(AppError::NotFound(
                "No agent running for this conversation".into(),
            ));
        };
        (handle.hub.clone(), handle.start_message_id.clone())
    };

    // Synthetic boundary event for THIS subscriber only — never published to
    // the hub. It tells the frontend where the run's event log begins so it
    // can truncate its DB-loaded history before applying the replay.
    let meta = start_message_id.map(|start_message_id| AgentEvent::RunMeta { start_message_id });

    let stream = futures_util::stream::iter(meta)
        .chain(hub.subscribe())
        .map(|e| Ok::<_, Infallible>(sse_event(&e)));

    Ok(Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response())
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
) -> ApiResult<Json<Vec<msg_db::StoredMessage>>> {
    conv_db::get_conversation(&state.pool, &id)
        .await?
        .ok_or_else(|| AppError::NotFound("Conversation not found".into()))?;

    let messages = msg_db::list_messages(&state.pool, &id).await?;

    Ok(Json(messages))
}
