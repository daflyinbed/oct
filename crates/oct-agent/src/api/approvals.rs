//! Approval REST surface: the pending list a refreshed
//! frontend rebuilds its approval cards from, and the decide endpoint that
//! answers a parked tool call. Approvals are memory-only by design — the
//! durable traces are the rules an `allow_always` persists and the
//! ToolResult the gate produced.

use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::AppState;
use super::error::{ApiResult, AppError};
use crate::db::{conversations as conv_db, permissions as rule_db};
use crate::permissions::registry::{ApprovalDecision, PendingApprovalMeta};
use crate::permissions::rules::{Effect, Rule};

/// One pending approval as the API serves it — the same fields the
/// `approval_requested` event carried, plus `created_at`; the list a page
/// refresh recovers its open approval cards from.
#[derive(Debug, Serialize, ToSchema)]
pub struct PendingApprovalDto {
    pub approval_id: String,
    pub agent_id: Option<String>,
    pub run_id: String,
    /// The tool call id the approval parks.
    pub call_id: String,
    pub tool: String,
    /// Action class label (`read`/`write`/`edit`/`command`/`mcp`).
    pub kind: String,
    /// One-line summary (resolved path / command / tool name).
    pub title: String,
    /// The RESOLVED path, file actions only.
    pub path: Option<String>,
    /// The raw command, execute actions only.
    pub command: Option<String>,
    /// Workspace containment verdict, file actions only.
    pub outside: Option<bool>,
    /// A conservative "always allow" pattern the user can edit before
    /// submitting with `allow_always`.
    pub rule_suggestion: Option<String>,
    pub created_at: String,
}

impl PendingApprovalDto {
    fn from_meta(meta: PendingApprovalMeta) -> Self {
        Self {
            agent_id: meta
                .parent_conversation_id
                .as_ref()
                .map(|_| meta.conversation_id.clone()),
            approval_id: meta.id,
            run_id: meta.run_id,
            call_id: meta.call_id,
            tool: meta.tool,
            kind: meta.kind,
            title: meta.title,
            path: meta.path,
            command: meta.command,
            outside: meta.outside,
            rule_suggestion: meta.rule_suggestion,
            created_at: meta.created_at.format("%Y-%m-%dT%H:%M:%S").to_string(),
        }
    }
}

/// The decide endpoint's body.
#[derive(Debug, Deserialize, ToSchema)]
pub struct DecideApprovalRequest {
    /// `allow` | `allow_always` | `deny`.
    pub decision: String,
    /// Overrides the request's `rule_suggestion` for the rule an
    /// `allow_always` persists (the user-edited pattern).
    pub pattern: Option<String>,
}

/// The decide endpoint's answer.
#[derive(Debug, Serialize, ToSchema)]
pub struct DecideApprovalResponse {
    pub resolved: bool,
}

/// Parse the decision string (a hand-rolled match keeps a bad value a 400
/// with this API's error body instead of a bare serde rejection).
fn parse_decision(value: &str) -> Result<ApprovalDecision, AppError> {
    match value {
        "allow" => Ok(ApprovalDecision::Allow),
        "allow_always" => Ok(ApprovalDecision::AllowAlways),
        "deny" => Ok(ApprovalDecision::Deny),
        other => Err(AppError::BadRequest(format!(
            "Invalid decision: '{other}' (expected one of: allow, allow_always, deny)"
        ))),
    }
}

/// The pending approvals of a conversation — the truth source a refreshed
/// UI rebuilds its open approval cards from (the event log replays, but
/// only this list tells which approvals are still undecided).
#[utoipa::path(
    get,
    path = "/conversations/{id}/approvals",
    params(("id" = String, Path, description = "Conversation ID")),
    responses(
        (status = 200, description = "Pending approvals", body = Vec<PendingApprovalDto>),
        (status = 404, description = "Conversation not found")
    ),
    tag = "approvals"
)]
pub async fn list_pending_approvals(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<PendingApprovalDto>>> {
    conv_db::get_conversation(&state.pool, &id)
        .await?
        .ok_or_else(|| AppError::NotFound("Conversation not found".into()))?;

    let pending = state.runs.approvals.pending_for_conversation(&id);
    Ok(Json(
        pending
            .into_iter()
            .map(PendingApprovalDto::from_meta)
            .collect(),
    ))
}

/// Answer a pending approval: `allow` proceeds the parked call,
/// `deny` refuses it, and `allow_always` persists an allow rule (source
/// `approval`, project scope), injects it into the conversation's LIVE
/// gates (so the same run stops asking) and then proceeds. The
/// resolution event lands on the conversation's hub for live viewers and
/// the replay log.
#[utoipa::path(
    post,
    path = "/conversations/{id}/approvals/{approval_id}",
    params(
        ("id" = String, Path, description = "Conversation ID"),
        ("approval_id" = String, Path, description = "Approval ID (from the approval_requested event or the pending list)"),
    ),
    request_body = DecideApprovalRequest,
    responses(
        (status = 200, description = "Decision delivered to the waiting run", body = DecideApprovalResponse),
        (status = 400, description = "Unknown decision value"),
        (status = 404, description = "Conversation not found, or the approval is unknown / already resolved")
    ),
    tag = "approvals"
)]
pub async fn decide_approval(
    State(state): State<AppState>,
    Path((id, approval_id)): Path<(String, String)>,
    Json(req): Json<DecideApprovalRequest>,
) -> ApiResult<Json<DecideApprovalResponse>> {
    let conversation = conv_db::get_conversation(&state.pool, &id)
        .await?
        .ok_or_else(|| AppError::NotFound("Conversation not found".into()))?;
    let decision = parse_decision(&req.decision)?;
    let lock = state
        .runs
        .approvals
        .decision_lock(&approval_id)
        .ok_or_else(|| AppError::NotFound("Approval is unknown or already resolved".into()))?;
    let _decision_guard = lock.lock().await;

    // Scope check through the conversation's own pending list: an approval
    // of another conversation (or an unknown/already-decided id) is the
    // same indistinguishable 404 (the idempotent decide).
    let meta = state
        .runs
        .approvals
        .pending_for_conversation(&id)
        .into_iter()
        .find(|meta| meta.id == approval_id)
        .ok_or_else(|| AppError::NotFound("Approval is unknown or already resolved".into()))?;

    // `allow_always`: persist first, then inject into every live
    // gate of the conversation, then resolve. MCP tools take a NULL
    // pattern (a patterned MCP rule can never match); an action with
    // no usable pattern (none submitted, none suggested) degrades to a
    // one-shot allow.
    let mut rule_id = None;
    if decision == ApprovalDecision::AllowAlways {
        let pattern = if meta.kind == "mcp" {
            None
        } else {
            req.pattern.as_deref().or(meta.rule_suggestion.as_deref())
        };
        if meta.kind == "mcp" || pattern.is_some() {
            let rule = rule_db::create_rule(
                &state.pool,
                rule_db::NewPermissionRule {
                    project_id: Some(&conversation.project_id),
                    conversation_id: None,
                    effect: "allow",
                    tool: &meta.tool,
                    pattern,
                    source: "approval",
                },
            )
            .await?;
            rule_id = Some(rule.id);
            for gate in state.runs.active_gates(
                meta.parent_conversation_id
                    .as_deref()
                    .unwrap_or(&meta.conversation_id),
            ) {
                gate.add_rule(Rule {
                    effect: Effect::Allow,
                    tool: meta.tool.clone(),
                    pattern: pattern.map(str::to_string),
                });
            }
        }
    }

    // Deliver the answer; a failure means the waiter is already gone (the
    // run was cancelled and its cleanup raced us) — still a 404, the
    // approval is settled either way.
    state
        .runs
        .approvals
        .resolve_with_rule(&approval_id, decision, rule_id)
        .map_err(|_| AppError::NotFound("Approval is unknown or already resolved".into()))?;

    Ok(Json(DecideApprovalResponse { resolved: true }))
}
