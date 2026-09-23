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
use crate::agent::compact;
use crate::agent::loop_runner::run_agent_loop;
use crate::agent::prompt::system_prompt;
use crate::agent::title;
use crate::agent::{AgentContext, AgentEvent, CompactionTrigger, EventHub, RunHandle};
use crate::db::{
    conversations as conv_db, messages as msg_db, models as model_db, projects as project_db,
    providers as provider_db,
};
use crate::tools;
use crate::tools::workspace::WorkspaceRoots;

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

/// Context window for compaction decisions: built-in provider info first
/// (Anthropic/Moonshot carry their limits in code), then the models table's
/// `limit_context` (seeded from models.dev or set on custom models). `None`
/// means unknown — auto compaction stays disabled, manual still works.
async fn resolve_context_window(
    state: &AppState,
    model: &dyn oct_llm_provider::model::ChatModel,
    provider_id: &str,
    model_id: &str,
) -> Option<u32> {
    if let Some(window) = model.info().limits.max_input_tokens.filter(|w| *w > 0) {
        return Some(window);
    }
    let row = model_db::get_model(&state.pool, provider_id, model_id)
        .await
        .ok()
        .flatten()?;
    row.limit_context
        .and_then(|v| u32::try_from(v).ok())
        .filter(|w| *w > 0)
}

/// Shared tail of the chat endpoints: take the session entry the caller
/// inserted (a real run handle with a fresh event hub), subscribe to the hub,
/// spawn the agent loop, and return the SSE stream of the run's events.
/// `state.sessions` must already hold the entry for `id` (so concurrent
/// starts get a 409); the entry stays until the loop returns, which keeps
/// GET /run reporting `running` and GET /events able to replay.
/// `idle` is how long the conversation sat idle BEFORE this run started —
/// the microcompact idle trigger.
fn start_agent_run(
    state: &AppState,
    id: &str,
    project: &project_db::Project,
    chat_model: Box<dyn oct_llm_provider::model::ChatModel>,
    view: msg_db::LlmView,
    context_window: Option<u32>,
    idle: Option<std::time::Duration>,
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

    // Re-resolved from the project row on every send/resume, so edits to a
    // project's working directories take effect on the next turn. Additional
    // directories are accessible to the tools via absolute paths only.
    let additional: Vec<PathBuf> = project
        .additional_dirs
        .0
        .iter()
        .map(PathBuf::from)
        .collect();
    let roots = WorkspaceRoots::new(PathBuf::from(&project.working_dir), additional.clone());

    // Default workspace tools plus every connected MCP server's tools that
    // are in scope for THIS project (global + project-scoped; the LLM sees
    // one flat toolset; MCP names are the mcp__{server}__{tool} forms
    // deduplicated across servers).
    let mut run_tools = tools::default_tools(roots);
    run_tools.extend(state.mcp.tools_for_run(Some(&project.id)));

    let ctx = AgentContext {
        model: Arc::from(chat_model),
        tools: Arc::new(run_tools),
        pool: state.pool.clone(),
        system_prompt: system_prompt(&PathBuf::from(&project.working_dir), &additional),
        context_window,
        idle,
    };

    let conv_id = id.to_string();
    let sessions = state.sessions.clone();

    tokio::spawn(async move {
        run_agent_loop(ctx, conv_id.clone(), view, handle).await;
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
    let context_window =
        resolve_context_window(&state, chat_model.as_ref(), &provider_id, &model_id).await;

    // Idle for the microcompact trigger: captured BEFORE the repair and the
    // new user message below — anything later would always look "just
    // active". Clock skew (negative delta) simply means "not idle".
    let idle = conversation_idle(&state, &id).await;

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

    let view = match msg_db::load_llm_view(&state.pool, &id).await {
        Ok(v) => v,
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

    // Bring the MCP connections in sync with the enabled mcp_servers rows
    // before the run's toolset is assembled, from this project's
    // perspective (best-effort: a server that fails to spawn is recorded as
    // Failed and simply contributes no tools).
    state
        .mcp
        .ensure_connections(&state.pool, Some(&project.id))
        .await;

    start_agent_run(
        &state,
        &id,
        &project,
        chat_model,
        view,
        context_window,
        idle,
    )
}

/// How long the conversation sat idle before this request, per the stored
/// history's last message timestamp.
async fn conversation_idle(state: &AppState, id: &str) -> Option<std::time::Duration> {
    msg_db::last_activity(&state.pool, id)
        .await
        .ok()
        .flatten()
        .and_then(|last| (chrono::Utc::now().naive_utc() - last).to_std().ok())
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

    let (chat_model, provider_id, model_id) = match resolve_model(&state, &provider_spec).await {
        Ok(m) => m,
        Err(e) => {
            state.sessions.remove(&id);
            return Err(e);
        }
    };
    let context_window =
        resolve_context_window(&state, chat_model.as_ref(), &provider_id, &model_id).await;

    // Idle captured before the repair writes below (see send_message).
    let idle = conversation_idle(&state, &id).await;

    // Only now persist the repair (idempotent): placeholder ToolResults for
    // the dangling calls found above.
    if let Err(e) = msg_db::repair_dangling_tool_calls(&state.pool, &id).await {
        state.sessions.remove(&id);
        return Err(AppError::from(e));
    }

    // No new user message: the loop continues from the existing history.
    let view = match msg_db::load_llm_view(&state.pool, &id).await {
        Ok(v) => v,
        Err(e) => {
            state.sessions.remove(&id);
            return Err(AppError::from(e));
        }
    };

    // Same MCP reconcile as send_message, again scoped to the project: the
    // resumed turn's toolset must reflect the current enabled servers.
    state
        .mcp
        .ensure_connections(&state.pool, Some(&project.id))
        .await;

    start_agent_run(
        &state,
        &id,
        &project,
        chat_model,
        view,
        context_window,
        idle,
    )
}

/// Query params for manual compaction.
#[derive(Debug, Deserialize, ToSchema)]
pub struct CompactQuery {
    /// `provider_id:model_id`. Optional: defaults to the conversation's last
    /// used provider/model.
    pub provider_spec: Option<String>,
}

/// Result of a manual compaction (mirrors the `compacted` SSE event).
#[derive(Debug, Serialize, ToSchema)]
pub struct CompactResult {
    pub summary_message_id: String,
    /// Number of message rows the marker replaced in the LLM view (the rows
    /// remain in the DB, excluded from future requests).
    pub compacted_messages: u32,
    pub est_tokens_before: u64,
    pub est_tokens_after: u64,
    pub summary: String,
}

/// Manually compact a conversation: summarize the old history into a marker
/// row right away (synchronous; not an SSE stream). Old rows are kept — the
/// LLM view simply starts at the new marker.
#[utoipa::path(
    post,
    path = "/conversations/{id}/compact",
    params(
        ("id" = String, Path, description = "Conversation ID"),
        ("provider_spec" = Option<String>, Query, description = "provider_id:model_id; defaults to the conversation's last used provider/model"),
    ),
    responses(
        (status = 200, description = "Compaction done", body = CompactResult),
        (status = 400, description = "Nothing to compact, or no provider/model available"),
        (status = 404, description = "Conversation not found"),
        (status = 409, description = "An agent run or another compaction is already active")
    ),
    tag = "chat"
)]
pub async fn compact_conversation(
    State(state): State<AppState>,
    Path(id): Path<String>,
    axum::extract::Query(query): axum::extract::Query<CompactQuery>,
) -> ApiResult<Json<CompactResult>> {
    // The session entry doubles as the compaction mutex: an active run OR a
    // concurrent manual compact gets a 409. Removed on every exit path below
    // (a synchronous request, unlike the SSE run endpoints).
    insert_session_entry(&state, &id)?;
    let result = run_manual_compact(&state, &id, query.provider_spec.as_deref()).await;
    state.sessions.remove(&id);
    result
}

async fn run_manual_compact(
    state: &AppState,
    id: &str,
    provider_spec: Option<&str>,
) -> ApiResult<Json<CompactResult>> {
    conv_db::get_conversation(&state.pool, id)
        .await?
        .ok_or_else(|| AppError::NotFound("Conversation not found".into()))?;

    let spec = match provider_spec {
        Some(s) => s.to_string(),
        None => match msg_db::get_last_provider_spec(&state.pool, id).await? {
            Some((provider_id, model_id)) => format!("{provider_id}:{model_id}"),
            None => {
                return Err(AppError::BadRequest(
                    "Conversation has no recorded provider/model; pass provider_spec".into(),
                ));
            }
        },
    };
    let (chat_model, provider_id, model_id) = resolve_model(state, &spec).await?;
    let window = resolve_context_window(state, chat_model.as_ref(), &provider_id, &model_id).await;
    let tail_keep = window
        .map(compact::tail_keep_tokens)
        .unwrap_or(compact::DEFAULT_TAIL_KEEP_TOKENS);

    let view = msg_db::load_llm_view(&state.pool, id).await?;
    let Some(cut) = compact::find_cut_point(&view.messages, tail_keep) else {
        return Err(AppError::BadRequest(
            "Conversation is too short to compact".into(),
        ));
    };

    // Display-only figures: the anchor is used when present so the "before"
    // number matches what the provider actually charged, falling back to a
    // message-only estimate (no tool schemas at hand outside a run).
    let anchor = view.anchor.map(|a| (a.tokens, a.message_index));
    let est_before = compact::estimate_context(&view.messages, &[], anchor);

    let region = view.messages[..cut].to_vec();
    let summary = compact::generate_summary(chat_model.as_ref(), &region, window)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Compaction summary failed: {e}")))?;

    let marker = compact::build_marker_message(&summary);
    // est_after on the SAME anchor-based 口径 as est_before: the region's
    // estimate leaves, the marker's estimate enters. est_before already
    // counts the system prompt and tool schemas through the anchor (or the
    // full local estimate without one); a message-only estimate would
    // understate the "after" figure against that baseline.
    let est_after = est_before.saturating_sub(compact::estimate_messages(&region))
        + compact::estimate_message(&marker);
    let details = compact::marker_details(CompactionTrigger::Manual, cut, est_before, est_after);
    // The marker goes INTO the timeline at the cut — between the summarized
    // region and the tail — so the tail stays after it and survives the next
    // `load_llm_view` (appending at the end would hide the tail forever).
    let cut_ordering = view.orderings[cut];
    let stored = msg_db::insert_message_at_ordering(
        &state.pool,
        id,
        &marker,
        None,
        None,
        Some(&details),
        cut_ordering,
    )
    .await?;

    Ok(Json(CompactResult {
        summary_message_id: stored.id,
        compacted_messages: cut as u32,
        est_tokens_before: est_before,
        est_tokens_after: est_after,
        summary,
    }))
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

/// The original content a microcompact copy replaced. Scope-checked through
/// the copy's membership slot in THIS conversation (the original itself no
/// longer belongs to any timeline after the swap), so the endpoint neither
/// leaks cross-conversation rows nor 404s on the original's missing
/// membership.
#[utoipa::path(
    get,
    path = "/conversations/{id}/messages/{copy_id}/original",
    params(
        ("id" = String, Path, description = "Conversation ID"),
        ("copy_id" = String, Path, description = "Id of the cleared copy message (a stored-history row)"),
    ),
    responses(
        (status = 200, description = "The original content row the copy supersedes", body = msg_db::MessageContent),
        (status = 404, description = "Conversation not found, or the copy does not belong to it / supersedes nothing")
    ),
    tag = "chat"
)]
pub async fn get_original_message(
    State(state): State<AppState>,
    Path((id, copy_id)): Path<(String, String)>,
) -> ApiResult<Json<msg_db::MessageContent>> {
    conv_db::get_conversation(&state.pool, &id)
        .await?
        .ok_or_else(|| AppError::NotFound("Conversation not found".into()))?;

    let original = msg_db::get_superseded_message(&state.pool, &id, &copy_id)
        .await?
        .ok_or_else(|| {
            AppError::NotFound("Message is not a cleared copy of this conversation".into())
        })?;

    Ok(Json(original))
}
