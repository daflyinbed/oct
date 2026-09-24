use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::sse::{KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::sync::Arc;
use utoipa::ToSchema;

use oct_llm_provider::core::{Message, Role};
use tracing::debug;

use super::AppState;
use super::dto;
use super::error::{ApiResult, AppError};
use crate::agent::compact;
use crate::agent::title;
use crate::agent::{AgentEvent, CompactionTrigger};
use crate::context::{self, LlmView};
use crate::db::{
    conversations as conv_db, messages as msg_db, models as model_db, projects as project_db,
    providers as provider_db,
};
use crate::runtime::{RootRunRequest, sse_event};

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
    // The occupancy guard takes the session entry up front (concurrent
    // starts get their 409 here) and releases it on drop — every `?` below
    // frees the conversation, no manual cleanup needed.
    let guard = state.runs.occupy(&id)?;

    let conversation = conv_db::get_conversation(&state.pool, &id)
        .await
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::NotFound("Conversation not found".into()))?;

    let project = project_db::get_project(&state.pool, &conversation.project_id)
        .await
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::NotFound("Project not found".into()))?;

    let (chat_model, provider_id, model_id) = resolve_model(&state, &req.provider_spec).await?;
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
    msg_db::repair_dangling_tool_calls(&state.pool, &id)
        .await
        .map_err(AppError::from)?;

    let user_msg = Message::text(Role::User, &req.content);
    let stored_user = msg_db::insert_message(
        &state.pool,
        &id,
        &user_msg,
        Some(&provider_id),
        Some(&model_id),
        None,
    )
    .await
    .map_err(AppError::from)?;

    // Record the run boundary on the (already live) session entry: the user
    // message row is the divide between DB history and this run's event log.
    // GET /events reports it as run_meta so a reconnecting frontend truncates
    // its history exactly here before replaying the run's events.
    state.runs.set_start_message_id(&id, &stored_user.id);

    let view = msg_db::load_llm_view(&state.pool, &id)
        .await
        .map_err(AppError::from)?;

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

    // Shared tail of the chat endpoints: hand the occupancy guard to the run
    // service — it is CONSUMED by the supervised run, so occupancy lasts
    // until the supervisor settles the terminal state (which keeps GET /run
    // reporting `running` and GET /events able to replay until then) — and
    // return the SSE stream of the run's events.
    let started = state
        .runs
        .start_root_run(RootRunRequest {
            guard,
            conversation_id: id,
            project,
            model: chat_model,
            provider_id,
            model_id,
            view,
            context_window,
            idle,
        })
        .await?;
    Ok(started.response)
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
    // Same occupancy guard as send_message: 409 on a live run, release on
    // every error exit below via drop.
    let guard = state.runs.occupy(&id)?;

    let conversation = conv_db::get_conversation(&state.pool, &id)
        .await
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::NotFound("Conversation not found".into()))?;

    let project = project_db::get_project(&state.pool, &conversation.project_id)
        .await
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::NotFound("Project not found".into()))?;

    // Read-only resumability probe: a dangling ToolCall OR a trailing
    // unanswered user message both mean the last turn never completed.
    // Nothing is persisted yet — an empty/answered conversation must 409
    // regardless of provider state.
    let dangling = msg_db::find_dangling_tool_calls(&state.pool, &id)
        .await
        .map_err(AppError::from)?;
    let last_is_user = msg_db::last_message_is_user(&state.pool, &id)
        .await
        .map_err(AppError::from)?;
    if dangling.is_empty() && !last_is_user {
        return Err(AppError::NothingToResume("Nothing to resume".into()));
    }

    // The resumed turn reuses the provider/model the conversation last ran
    // with; there is no request body to carry a fresh spec. Resolved before
    // the repair below: persisting placeholder rows and then failing to
    // resolve would leave the conversation permanently unresumable (nothing
    // dangling anymore, last message now a tool row → "Nothing to resume").
    let provider_spec = match msg_db::get_last_provider_spec(&state.pool, &id).await? {
        Some((provider_id, model_id)) => format!("{provider_id}:{model_id}"),
        None => {
            return Err(AppError::BadRequest(
                "Conversation has no recorded provider/model to resume with".into(),
            ));
        }
    };

    let (chat_model, provider_id, model_id) = resolve_model(&state, &provider_spec).await?;
    let context_window =
        resolve_context_window(&state, chat_model.as_ref(), &provider_id, &model_id).await;

    // Idle captured before the repair writes below (see send_message).
    let idle = conversation_idle(&state, &id).await;

    // Only now persist the repair (idempotent): placeholder ToolResults for
    // the dangling calls found above.
    msg_db::repair_dangling_tool_calls(&state.pool, &id)
        .await
        .map_err(AppError::from)?;

    // No new user message: the loop continues from the existing history.
    let view = msg_db::load_llm_view(&state.pool, &id)
        .await
        .map_err(AppError::from)?;

    // Same MCP reconcile as send_message, again scoped to the project: the
    // resumed turn's toolset must reflect the current enabled servers.
    state
        .mcp
        .ensure_connections(&state.pool, Some(&project.id))
        .await;

    // Same shared tail as send_message: the guard moves into the supervised
    // run (see the comment there).
    let started = state
        .runs
        .start_root_run(RootRunRequest {
            guard,
            conversation_id: id,
            project,
            model: chat_model,
            provider_id,
            model_id,
            view,
            context_window,
            idle,
        })
        .await?;
    Ok(started.response)
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
    // concurrent manual compact gets a 409. The occupancy guard releases the
    // entry on drop at every exit path below (a synchronous request, unlike
    // the SSE run endpoints).
    let _guard = state.runs.occupy(&id)?;
    run_manual_compact(&state, &id, query.provider_spec.as_deref()).await
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

    let view = msg_db::load_llm_view(&state.pool, id).await?;
    let LlmView {
        messages,
        orderings,
        anchor,
    } = view;
    let entries: Vec<context::ContextEntry> = messages
        .into_iter()
        .zip(orderings)
        .map(|(message, ordering)| context::ContextEntry { message, ordering })
        .collect();

    // Display-only figures: the anchor is used when present so the "before"
    // number matches what the provider actually charged, falling back to a
    // message-only estimate (no tool schemas at hand outside a run — the
    // est_after 口径 is documented on `compact::CompactRequest`).
    let est_before = context::estimate_view(&entries, None, &[], anchor.as_ref());

    // The shared compaction transaction (same cut/tail/summary/persist rules
    // as the in-run auto trigger); no hub (no SSE stream here) and no
    // cancellation token — a synchronous request.
    let outcome = match compact::run_compaction(
        chat_model.as_ref(),
        &state.pool,
        id,
        compact::CompactRequest {
            entries: &entries,
            est_before,
            system_prompt: None,
            tool_specs: &[],
            window,
            trigger: CompactionTrigger::Manual,
            hub: None,
            cancel: None,
        },
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(compact::CompactFailure::Nothing) => {
            return Err(AppError::BadRequest(
                "Conversation is too short to compact".into(),
            ));
        }
        Err(compact::CompactFailure::Summary(e)) => {
            return Err(AppError::Internal(anyhow::anyhow!(
                "Compaction summary failed: {e}"
            )));
        }
        Err(compact::CompactFailure::Persist(e)) => {
            return Err(AppError::Internal(anyhow::anyhow!(
                "Compaction failed to persist the marker row: {e}"
            )));
        }
        Err(compact::CompactFailure::Cancelled) => {
            unreachable!("manual compaction passes no cancellation token")
        }
    };

    Ok(Json(CompactResult {
        summary_message_id: outcome.summary_message_id,
        compacted_messages: outcome.cut as u32,
        est_tokens_before: outcome.est_before,
        est_tokens_after: outcome.est_after,
        summary: outcome.summary,
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
        (status = 200, description = "List of messages (typed parts/details; raw parts_json/details_json never leave the backend)", body = Vec<dto::MessageDto>),
        (status = 404, description = "Conversation not found")
    ),
    tag = "chat"
)]
pub async fn get_messages(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<dto::MessageDto>>> {
    conv_db::get_conversation(&state.pool, &id)
        .await?
        .ok_or_else(|| AppError::NotFound("Conversation not found".into()))?;

    let messages = msg_db::list_messages(&state.pool, &id).await?;

    Ok(Json(messages.iter().map(dto::MessageDto::from_stored).collect()))
}

/// The original content a microcompact copy replaced. Scope-checked through
/// the copy's membership slot in THIS conversation (the original itself no
/// longer belongs to any timeline after the swap), so the endpoint neither
/// leaks cross-conversation rows nor 404s on the original's missing
/// membership. Same typed contract as the history endpoint.
#[utoipa::path(
    get,
    path = "/conversations/{id}/messages/{copy_id}/original",
    params(
        ("id" = String, Path, description = "Conversation ID"),
        ("copy_id" = String, Path, description = "Id of the cleared copy message (a stored-history row)"),
    ),
    responses(
        (status = 200, description = "The original content row the copy supersedes", body = dto::MessageDto),
        (status = 404, description = "Conversation not found, or the copy does not belong to it / supersedes nothing")
    ),
    tag = "chat"
)]
pub async fn get_original_message(
    State(state): State<AppState>,
    Path((id, copy_id)): Path<(String, String)>,
) -> ApiResult<Json<dto::MessageDto>> {
    conv_db::get_conversation(&state.pool, &id)
        .await?
        .ok_or_else(|| AppError::NotFound("Conversation not found".into()))?;

    let original = msg_db::get_superseded_message(&state.pool, &id, &copy_id)
        .await?
        .ok_or_else(|| {
            AppError::NotFound("Message is not a cleared copy of this conversation".into())
        })?;

    Ok(Json(dto::MessageDto::from_content(&original)))
}
