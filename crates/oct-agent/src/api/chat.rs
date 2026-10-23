use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::sse::{KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use utoipa::ToSchema;

use super::AppState;
use super::dto;
use super::error::{ApiResult, AppError};
use super::sse::sse_event;
use crate::application::conversation::{CompactCommand, ResumeCommand, SendCommand};
use crate::context;
use crate::db::{conversations as conv_db, messages as msg_db};

fn event_response(events: crate::events::SubscribeStream) -> Response {
    Sse::new(events.map(|event| Ok::<_, Infallible>(sse_event(&event))))
        .keep_alive(KeepAlive::default())
        .into_response()
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReasoningRequestDto {
    /// 档位字符串（low/medium/high/...，按模型 reasoning_options.values 校验）。
    Effort { effort: String },
    /// toggle 型模型：开启思考。
    Enabled,
    /// 显式思考 token 预算（budget_tokens 型模型）。
    BudgetTokens { budget: u32 },
}

impl ReasoningRequestDto {
    fn to_core(&self) -> oct_llm_provider::core::ReasoningRequest {
        match self.clone() {
            ReasoningRequestDto::Effort { effort } => {
                oct_llm_provider::core::ReasoningRequest::Effort { effort }
            }
            ReasoningRequestDto::Enabled => oct_llm_provider::core::ReasoningRequest::Enabled,
            ReasoningRequestDto::BudgetTokens { budget } => {
                oct_llm_provider::core::ReasoningRequest::BudgetTokens { budget }
            }
        }
    }
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct SendMessageRequest {
    pub content: String,
    pub provider_spec: String,
    pub expected_revision: Option<i64>,
    /// Opaque client receipt used to reconcile a send whose response was lost.
    pub client_request_id: Option<String>,
    /// 本次回合的思考档位。校验通过后随 user 消息 details 持久化（resume
    /// 恢复用），并在 agent loop 的每次模型请求上生效。
    pub reasoning: Option<ReasoningRequestDto>,
    /// 随消息发送的附件：先经 `POST …/attachments` 上传得到的 staged id。
    /// 图片以 `oct-attachment://` 引用进 ImageUrl part（请求装配时才水合），
    /// 文本文件直接内联成 Text part。缺省/空 = 无附件。
    pub attachment_ids: Option<Vec<String>>,
}

/// Whether an agent run is currently active for a conversation.
#[derive(Debug, Serialize, ToSchema)]
pub struct RunningStatus {
    pub running: bool,
    pub resumable: bool,
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
    let command = SendCommand {
        conversation_id: id,
        content: req.content,
        provider_spec: req.provider_spec,
        expected_revision: req.expected_revision,
        client_request_id: req.client_request_id,
        reasoning: req.reasoning.map(|value| value.to_core()),
        attachment_ids: req.attachment_ids.unwrap_or_default(),
    };
    let subscription = state.conversations.send(command).await?;
    Ok(event_response(subscription.events))
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
    let subscription = state
        .conversations
        .resume(ResumeCommand {
            conversation_id: id,
        })
        .await?;
    Ok(event_response(subscription.events))
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
    let outcome = state
        .conversations
        .compact(CompactCommand {
            conversation_id: id,
            provider_spec: query.provider_spec,
        })
        .await?;
    Ok(Json(CompactResult {
        summary_message_id: outcome.summary_message_id,
        compacted_messages: outcome.compacted_messages,
        est_tokens_before: outcome.est_tokens_before,
        est_tokens_after: outcome.est_tokens_after,
        summary: outcome.summary,
    }))
}

/// One entry of the `/reload` manifest: a loaded instruction file with its
/// budgeted size.
#[derive(Debug, Serialize, ToSchema)]
pub struct InstructionFileDto {
    /// Absolute (canonical) path of the loaded file.
    pub path: String,
    /// Content length in bytes, AFTER budgeting.
    pub bytes: u64,
    /// Whether the per-file or total budget cut the content.
    pub truncated: bool,
}

/// Result of an instructions reload: the freshly frozen environment's
/// manifest.
#[derive(Debug, Serialize, ToSchema)]
pub struct InstructionsReloadResult {
    /// The instruction files now in effect (empty when the workspace has
    /// none).
    pub repo_files: Vec<InstructionFileDto>,
    /// Exposed names of the MCP tools now in effect for this conversation.
    pub mcp_tools: Vec<String>,
}

/// Re-read the conversation's repository instruction files and re-snapshot
/// the live MCP tool set, REPLACING the frozen prompt environment: the next
/// run assembles from the fresh entry (a run in flight is unaffected — its
/// prompt is already built). This is the explicit refresh point for edits
/// that would otherwise wait for a compaction boundary.
#[utoipa::path(
    post,
    path = "/conversations/{id}/instructions/reload",
    params(("id" = String, Path, description = "Conversation ID")),
    responses(
        (status = 200, description = "Environment re-frozen; manifest of what is now in effect", body = InstructionsReloadResult),
        (status = 404, description = "Conversation not found"),
        (status = 409, description = "Managed subagent conversation (reload through its parent)")
    ),
    tag = "chat"
)]
pub async fn reload_instructions(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<InstructionsReloadResult>> {
    let env = state.conversations.reload_instructions(&id).await?;
    Ok(Json(InstructionsReloadResult {
        repo_files: env
            .repo
            .files
            .iter()
            .map(|f| InstructionFileDto {
                path: f.path.clone(),
                bytes: f.bytes as u64,
                truncated: f.truncated,
            })
            .collect(),
        mcp_tools: env
            .mcp_tools
            .iter()
            .map(|t| t.exposed_name.clone())
            .collect(),
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
        resumable: state.conversations.can_resume(&id).await?,
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
        (status = 200, description = "SSE stream: the execution log including ready run_meta metadata, followed by live events"),
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
    let hub = {
        let Some(handle) = state.sessions.get(&id) else {
            return Err(AppError::NotFound(
                "No agent running for this conversation".into(),
            ));
        };
        handle.hub.clone()
    };

    // Ready metadata is part of this same hub's log, including subscriptions
    // opened while resume is still preparing its durable history.
    let stream = hub.subscribe().map(|e| Ok::<_, Infallible>(sse_event(&e)));

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
) -> ApiResult<Response> {
    let mut tx = state.pool.begin().await.map_err(anyhow::Error::from)?;
    let snapshot = sqlx::query!(
        "SELECT context_revision, history_epoch, forked_at_revision, origin_kind FROM conversations WHERE id = $1",
        id
    )
    .fetch_optional(&mut *tx)
    .await
    .map_err(anyhow::Error::from)?
    .ok_or_else(|| AppError::NotFound("Conversation not found".into()))?;
    let messages = msg_db::list_messages_in_tx(&mut tx, &id).await?;
    let estimate = match msg_db::view_from_snapshot(&mut tx, &messages).await {
        Ok(view) => {
            let entries = view
                .messages
                .into_iter()
                .zip(view.orderings)
                .map(|(message, ordering)| context::ContextEntry { message, ordering })
                .collect::<Vec<_>>();
            Some(context::estimate_view(
                &entries,
                None,
                &[],
                view.anchor.as_ref(),
            ))
        }
        Err(_) => None, // Preserve tolerant display of legacy/corrupt rows.
    };
    tx.commit().await.map_err(anyhow::Error::from)?;
    let tool_cuts = super::history::tool_cuts(&messages);
    let data = messages
        .iter()
        .map(|row| {
            let mut dto = dto::MessageDto::from_stored(row);
            dto.fork_cut = dto.fork_cut.or_else(|| tool_cuts.get(&row.id).cloned());
            dto.inherited = Some(
                snapshot.origin_kind == "revert_backup"
                    || snapshot
                        .forked_at_revision
                        .is_some_and(|revision| row.appended_at_revision <= revision),
            );
            dto
        })
        .collect::<Vec<_>>();
    let mut response = Json(data).into_response();
    response.headers_mut().insert(
        "X-Context-Revision",
        snapshot
            .context_revision
            .to_string()
            .parse()
            .expect("decimal header"),
    );
    response.headers_mut().insert(
        "X-History-Epoch",
        snapshot
            .history_epoch
            .to_string()
            .parse()
            .expect("decimal header"),
    );
    if let Some(tokens) = estimate {
        response.headers_mut().insert(
            "X-Context-Tokens",
            tokens.to_string().parse().expect("decimal header"),
        );
    }
    Ok(response)
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
