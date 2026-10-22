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

use super::AppState;
use super::dto;
use super::error::{ApiResult, AppError};
use super::sse::sse_event;
use crate::agent::compact;
use crate::agent::title;
use crate::agent::{AgentEvent, CompactionTrigger};
use crate::context::{self, LlmView};
use crate::db::{conversations as conv_db, messages as msg_db, projects as project_db};
use crate::runtime::RootRunRequest;
use crate::subagent::{RecoveryJob, orchestrate};

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

/// Thin delegates over the RunService's resolvers (moved there so subagents
/// and the recovery orchestrator resolve models exactly like a chat turn).
async fn resolve_model(
    state: &AppState,
    provider_spec: &str,
) -> Result<(Box<dyn oct_llm_provider::model::ChatModel>, String, String), AppError> {
    state.runs.resolve_model(provider_spec).await
}

async fn resolve_context_window(
    state: &AppState,
    model: &dyn oct_llm_provider::model::ChatModel,
    provider_id: &str,
    model_id: &str,
) -> Option<u32> {
    state
        .runs
        .resolve_context_window(model, provider_id, model_id)
        .await
}

/// 409 when the conversation is a managed subagent session: send / resume /
/// compact must go through the manager (or the parent), never start a run
/// behind its back.
fn ensure_not_managed(conversation: &conv_db::Conversation) -> Result<(), AppError> {
    if conversation.kind == "subagent" {
        return Err(AppError::ManagedConversation(
            "Conversation is a managed subagent session; drive it from the parent conversation"
                .into(),
        ));
    }
    Ok(())
}

// Send preparation rejects dangling managed delegations before repairing
// ordinary tool calls; the application preparation module owns that rule.
/// 校验请求携带的思考档位与 models 表的记录一致——fail fast（400），
/// 避免档位落到 wire 层才被 provider 拒绝。DB 缺少目录元数据时仍校验
/// adapter 支持范围与预算上限，但不猜测 effort 的合法值。
async fn validate_reasoning(
    state: &AppState,
    provider_id: &str,
    model_id: &str,
    reasoning: &ReasoningRequestDto,
) -> Result<(), AppError> {
    if !matches!(reasoning, ReasoningRequestDto::Effort { .. }) {
        let provider = crate::db::providers::get_provider(&state.pool, provider_id)
            .await
            .map_err(AppError::from)?;
        if provider.is_none_or(|p| p.adapter_type != "anthropic") {
            return Err(AppError::BadRequest(
                "此 provider 的适配器仅支持 effort 档位，暂不支持思考开关或 token 预算".into(),
            ));
        }
    }
    if let ReasoningRequestDto::BudgetTokens { budget } = reasoning
        && *budget < 1024
    {
        return Err(AppError::BadRequest(
            "Anthropic 思考预算必须至少为 1024".into(),
        ));
    }
    let row = crate::db::models::get_model(&state.pool, provider_id, model_id)
        .await
        .map_err(AppError::from)?;
    if let ReasoningRequestDto::BudgetTokens { budget } = reasoning {
        let limit = row
            .as_ref()
            .and_then(|row| row.limit_output)
            .filter(|limit| *limit > 0)
            .unwrap_or(8192);
        if i64::from(*budget) >= limit {
            return Err(AppError::BadRequest(
                "Anthropic 思考预算必须小于模型的输出上限".into(),
            ));
        }
    }
    let Some(row) = row else {
        return Ok(());
    };
    if !row.reasoning {
        return Err(AppError::BadRequest(format!(
            "模型 {provider_id}:{model_id} 不支持思考（reasoning），请移除思考档位"
        )));
    }
    if let Some(raw) = &row.reasoning_options
        && let Err(message) =
            crate::reasoning::ReasoningOptions::parse(raw).validate(&reasoning.to_core())
    {
        return Err(AppError::BadRequest(message));
    }
    Ok(())
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

    // Managed subagent sessions are driven by their parent's `task` calls —
    // never by a direct send.
    ensure_not_managed(&conversation)?;
    conversation.ensure_writable()?;

    if let Some(expected) = req.expected_revision {
        let revision = sqlx::query_scalar!(
            "SELECT context_revision FROM conversations WHERE id = $1",
            id
        )
        .fetch_one(&state.pool)
        .await
        .map_err(anyhow::Error::from)?;
        if revision != expected {
            return Err(AppError::StaleRevision(
                "会话历史已变化，请刷新后重新发送。".into(),
            ));
        }
    }

    let project = project_db::get_project(&state.pool, &conversation.project_id)
        .await
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::NotFound("Project not found".into()))?;

    let (chat_model, provider_id, model_id) = resolve_model(&state, &req.provider_spec).await?;
    if let Some(reasoning) = &req.reasoning {
        validate_reasoning(&state, &provider_id, &model_id, reasoning).await?;
    }
    let context_window =
        resolve_context_window(&state, chat_model.as_ref(), &provider_id, &model_id).await;

    // A turn with unfinished (crashed) subagent tasks must be recovered, not
    // appended to: the repair below would otherwise turn the pending task
    // calls into error placeholders.
    crate::runtime::preparation::ensure_no_pending_recovery(&state.pool, &id).await?;

    // Idle for the microcompact trigger: captured BEFORE the repair and the
    // new user message below — anything later would always look "just
    // active". Clock skew (negative delta) simply means "not idle".
    let idle = conversation_idle(&state, &id).await;

    // Poison guard: a previous run interrupted mid-tool (e.g. backend crash)
    // leaves assistant ToolCalls without tool messages. Shared preparation
    // repairs them BEFORE background notifications or the new user message
    // are appended, keeping the tool batch consecutive for the provider.
    crate::runtime::preparation::prepare_history(&state.runs, &id).await?;

    // One application operation owns the publication lease, frozen snapshots
    // and atomic ticket/message/ownership commit. Filesystem IO finishes before
    // the SQLite write transaction, and every failed claim remains retryable.
    let prepared = state
        .runs
        .attachments
        .prepare_message(
            &id,
            &req.content,
            req.attachment_ids.as_deref().unwrap_or_default(),
            context_window,
        )
        .await
        .map_err(super::attachments::service_error)?;
    // Receipt and reasoning are durable request metadata, independent of the
    // typed user body/attachments assembled by the attachment service.
    let mut details = serde_json::json!({});
    if let Some(request_id) = &req.client_request_id {
        details["client_request_id"] = serde_json::json!(request_id);
    }
    if let Some(reasoning) = &req.reasoning {
        details["reasoning"] = serde_json::to_value(reasoning).map_err(anyhow::Error::from)?;
    }
    prepared
        .commit(&provider_id, &model_id, details)
        .await
        .map_err(super::attachments::service_error)?;

    // Runtime captures the durable replay base after preparation and before
    // publishing this execution's content events.

    let view = msg_db::load_llm_view(&state.pool, &id)
        .await
        .map_err(AppError::from)?;

    // One-shot title generation: an independent background task firing
    // alongside the run (never inside it). Guards: the conversation still
    // carries its default title, and the user message just persisted is its
    // first — a failed attempt is not retried on later turns. Forks start
    // with a default「（分支）」placeholder over copied history, so for them
    // the one-shot fires on the first message sent INSIDE the fork (the row
    // count past the fork point) — that is the moment the branch actually
    // diverges and a regenerated title can say something the source's
    // cannot. Spawned only after every fallible step has succeeded, so an
    // aborted send never burns the one-shot on a message that was never
    // stored. The weak hub reference keeps a slow title call from delaying
    // SSE EOF past the run's end.
    let fork_first_own_message = conversation.origin_kind == "fork"
        && matches!(
            msg_db::count_user_messages_after(
                &state.pool,
                &id,
                conversation.forked_at_revision.unwrap_or(0),
            )
            .await,
            Ok(1),
        );
    if conversation.title_source == "default"
        && (matches!(msg_db::count_user_messages(&state.pool, &id).await, Ok(1))
            || fork_first_own_message)
    {
        let hub = state
            .sessions
            .get(&id)
            .map(|entry| Arc::downgrade(&entry.value().hub));
        if let Some(hub) = hub {
            title::spawn(title::TitleJob {
                runs: state.runs.clone(),
                project_id: project.id.clone(),
                parent_provider_id: provider_id.clone(),
                parent_model_id: model_id.clone(),
                pool: state.pool.clone(),
                conversation_id: id.clone(),
                first_user_content: req.content.clone(),
                hub,
            });
        }
    }

    // Bring the MCP connections in sync with the enabled mcp_servers rows
    // before the run's toolset is assembled, from this project's
    // perspective (best-effort: a server that fails to spawn is recorded as
    // Failed and simply contributes no tools).
    let handle = state
        .sessions
        .get(&id)
        .map(|entry| entry.value().clone())
        .expect("the occupancy guard guarantees the session entry");
    if !state
        .mcp
        .ensure_connections_cancelled(&state.pool, Some(&project.id), &handle.cancel)
        .await
    {
        // The user message is durable and remains resumable. Publish on the
        // existing hub too, so reconnect subscribers observe cancellation
        // before this preparation guard releases occupancy.
        handle.hub.publish(AgentEvent::Cancelled);
        return Ok(Sse::new(
            handle
                .hub
                .subscribe()
                .map(|event| Ok::<_, Infallible>(sse_event(&event))),
        )
        .keep_alive(KeepAlive::default())
        .into_response());
    }

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
            reasoning: req.reasoning.as_ref().map(|dto| dto.to_core()),
            // Chat conversations get the delegation tool (their workers then
            // never do — depth <= 1, enforced again at registration).
            attach_task_tool: conversation.kind == "chat",
        })
        .await?;
    Ok(
        Sse::new(started.events.map(|e| Ok::<_, Infallible>(sse_event(&e))))
            .keep_alive(KeepAlive::default())
            .into_response(),
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
    // Same occupancy guard as send_message: 409 on a live run, release on
    // every error exit below via drop. The token is the orchestrator's:
    // children restarted during recovery derive child tokens from it, and
    // the parent run started at the end reuses the same session handle — so
    // the cancel endpoint covers both phases.
    let cancel = tokio_util::sync::CancellationToken::new();
    let guard = state.runs.occupy_with(&id, cancel.clone())?;

    let conversation = conv_db::get_conversation(&state.pool, &id)
        .await
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::NotFound("Conversation not found".into()))?;

    // Managed subagent sessions recover through their parent conversation.
    ensure_not_managed(&conversation)?;
    conversation.ensure_writable()?;

    let project = project_db::get_project(&state.pool, &conversation.project_id)
        .await
        .map_err(AppError::from)?
        .ok_or_else(|| AppError::NotFound("Project not found".into()))?;

    // Read-only resumability probe — EXTENDED beyond dangling calls
    // (恢复资格看 run 状态): a dangling ToolCall OR a trailing unanswered user message
    // both mean the last turn never completed, and so does a `lost` latest
    // run whose last row is a tool result (every tool result landed, but the
    // next model request never completed). Nothing is persisted yet — an
    // empty/answered conversation must 409 regardless of provider state.
    let dangling = msg_db::find_dangling_tool_calls(&state.pool, &id)
        .await
        .map_err(AppError::from)?;
    if !crate::runtime::preparation::can_resume(&state.pool, &id).await? {
        return Err(AppError::NothingToResume("Nothing to resume".into()));
    }

    // The resumed turn reuses the provider/model the conversation last ran
    // with; there is no request body to carry a fresh spec. Resolved before
    // ANY write (also before the orchestrator spawns): persisting
    // placeholder rows and then failing to resolve would leave the
    // conversation permanently unresumable.
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

    // Restore the interrupted turn's reasoning level from the trailing user
    // message's details — the resumed request must not silently drop thinking
    // mid-conversation. Malformed stored values degrade to None (no level).
    let reasoning = msg_db::get_last_user_details(&state.pool, &id)
        .await
        .map_err(AppError::from)?
        .and_then(|details| details.get("reasoning").cloned())
        .and_then(|value| serde_json::from_value(value).ok());

    // Idle captured before any repair writes below (see send_message).
    let idle = conversation_idle(&state, &id).await;

    // Subscribe to the (already occupied) conversation's hub and return the
    // SSE response IMMEDIATELY: keep-alive covers the recovery wait, and
    // children publish `subagent_updated` onto this same hub so the UI is
    // live. The orchestrator does every write (repair, backfill, child
    // restarts), flushes background notifications after all tool results,
    // and finally starts the parent loop over the guard it holds.
    let handle = state
        .sessions
        .get(&id)
        .map(|entry| entry.value().clone())
        .expect("the occupancy guard guarantees the session entry");
    let hub = handle.hub.clone();
    let stream = handle
        .hub
        .subscribe()
        .map(|e| Ok::<_, Infallible>(sse_event(&e)));

    tokio::spawn(orchestrate(RecoveryJob {
        runs: state.runs.clone(),
        hub,
        cancel,
        guard,
        conversation_id: id,
        project,
        model: chat_model,
        provider_id,
        model_id,
        context_window,
        idle,
        reasoning,
        dangling,
    }));

    Ok(Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response())
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

    let conversation = conv_db::get_conversation(&state.pool, &id)
        .await?
        .ok_or_else(|| AppError::NotFound("Conversation not found".into()))?;
    // Managed subagent sessions compact with their parent's history — the
    // wait for their pending tasks would be lost otherwise.
    ensure_not_managed(&conversation)?;
    conversation.ensure_writable()?;
    // A root conversation with crashed subagent tasks must recover first:
    // compacting would summarize past the very calls whose results are still
    // owed.
    crate::runtime::preparation::ensure_no_pending_recovery(&state.pool, &id).await?;

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
    // cancellation token — a synchronous request. Tagged with the
    // conversation so the manual path's wire lines land in the right
    // per-conversation file (the run path inherits them from the supervisor).
    // The prompt-env handle flags the frozen instruction block dirty on
    // success — the next run re-reads the files.
    let compaction = oct_llm_provider::wire::with_tags(
        oct_llm_provider::wire::Tags {
            conversation_id: Some(id.to_string()),
            ..Default::default()
        },
        compact::run_compaction(
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
                prompt_env_refresh: Some(state.runs.prompt_env_refresh(id)),
            },
        ),
    );
    let outcome = match compaction.await {
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
    let conversation = conv_db::get_conversation(&state.pool, &id)
        .await?
        .ok_or_else(|| AppError::NotFound("Conversation not found".into()))?;
    // Managed subagent sessions share their ROOT conversation's entry —
    // reload through the parent (or the root id itself).
    ensure_not_managed(&conversation)?;
    // Read-only backups (revert) never run and must not mint a frozen entry.
    conversation.ensure_writable()?;
    let project = project_db::get_project(&state.pool, &conversation.project_id)
        .await?
        .ok_or_else(|| AppError::NotFound("Project not found".into()))?;

    // Same reconcile a chat turn performs, so newly-enabled servers join
    // the fresh identity snapshot (best-effort; failed servers contribute
    // no tools, as ever).
    state
        .mcp
        .ensure_connections(&state.pool, Some(&project.id))
        .await;

    let env = state.runs.reload_prompt_env(&id, &project).await;
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
        resumable: crate::runtime::preparation::can_resume(&state.pool, &id).await?,
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
    let tool_cuts = crate::db::history::tool_cuts(&messages);
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
