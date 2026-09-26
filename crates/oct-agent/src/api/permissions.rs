//! Permission-rule REST surface (ADR 0004 §5): the settings page's CRUD
//! over the `permission_rules` table. Project scope
//! (`/projects/{projectId}/permission-rules`) and global scope
//! (`/permission-rules`) share the handlers' bodies; `allow_always`
//! approvals write rows of `source = "approval"` through the DB layer but
//! never through these endpoints.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::AppState;
use super::error::{ApiResult, AppError};
use crate::db::permissions::{self as rule_db, PermissionRule};
use crate::permissions::rules::Effect;

/// One persisted permission rule as the API serves it.
#[derive(Debug, Serialize, ToSchema)]
pub struct PermissionRuleDto {
    pub id: String,
    /// `None` = a global rule.
    pub project_id: Option<String>,
    /// Always `None` today (reserved, ADR 0004 §3).
    pub conversation_id: Option<String>,
    /// `allow` | `deny` | `ask`.
    pub effect: String,
    /// Exact tool name (e.g. `execute_command`) or `mcp__{server}__*`.
    pub tool: String,
    /// `None` = any invocation; command prefix for `execute_command`;
    /// absolute-path glob for the file tools.
    pub pattern: Option<String>,
    /// `user` (this API) or `approval` (an "always allow" answer).
    pub source: String,
    pub created_at: String,
    pub updated_at: String,
}

impl PermissionRuleDto {
    fn from_row(row: PermissionRule) -> Self {
        Self {
            id: row.id,
            project_id: row.project_id,
            conversation_id: row.conversation_id,
            effect: row.effect,
            tool: row.tool,
            pattern: row.pattern,
            source: row.source,
            created_at: row.created_at.format("%Y-%m-%dT%H:%M:%S").to_string(),
            updated_at: row.updated_at.format("%Y-%m-%dT%H:%M:%S").to_string(),
        }
    }
}

/// The rule-creation body (both scopes).
#[derive(Debug, Deserialize, ToSchema)]
pub struct CreatePermissionRuleRequest {
    /// `allow` | `deny` | `ask`.
    pub effect: String,
    /// Tool name; non-empty.
    pub tool: String,
    /// Optional pattern; semantics per tool class (the front end is
    /// responsible for offering sensible patterns).
    pub pattern: Option<String>,
}

/// Validate the creation body into the DB layer's string payload; effect
/// and tool are checked here so a bad row never reaches the CHECK
/// constraint (a clean 400, not a 500).
fn validated(
    req: &CreatePermissionRuleRequest,
) -> Result<(Effect, String, Option<String>), AppError> {
    let effect =
        Effect::from_str_opt(&req.effect).ok_or_else(|| {
            AppError::BadRequest(format!(
                "Invalid effect: '{}' (expected one of: allow, deny, ask)",
                req.effect
            ))
        })?;
    let tool = req.tool.trim();
    if tool.is_empty() {
        return Err(AppError::BadRequest("tool must not be empty".into()));
    }
    Ok((effect, tool.to_string(), req.pattern.clone()))
}

/// The rules visible to a project: its own rows plus the global ones.
#[utoipa::path(
    get,
    path = "/projects/{projectId}/permission-rules",
    params(("projectId" = String, Path, description = "Project ID")),
    responses(
        (status = 200, description = "Rules visible to the project (own + global)", body = Vec<PermissionRuleDto>),
        (status = 404, description = "Project not found")
    ),
    tag = "permission-rules"
)]
pub async fn list_project_permission_rules(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
) -> ApiResult<Json<Vec<PermissionRuleDto>>> {
    crate::db::projects::get_project(&state.pool, &project_id)
        .await?
        .ok_or_else(|| AppError::NotFound("Project not found".into()))?;

    let rules = rule_db::list_rules_for_project(&state.pool, &project_id).await?;
    Ok(Json(rules.into_iter().map(PermissionRuleDto::from_row).collect()))
}

/// Create a project-scoped rule. Manual CRUD writes the DB only — runs
/// already in flight keep their assembly-time rule set; the next run loads
/// the new row (§1.8).
#[utoipa::path(
    post,
    path = "/projects/{projectId}/permission-rules",
    params(("projectId" = String, Path, description = "Project ID")),
    request_body = CreatePermissionRuleRequest,
    responses(
        (status = 201, description = "Created rule", body = PermissionRuleDto),
        (status = 400, description = "Invalid effect or empty tool"),
        (status = 404, description = "Project not found")
    ),
    tag = "permission-rules"
)]
pub async fn create_project_permission_rule(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
    Json(req): Json<CreatePermissionRuleRequest>,
) -> ApiResult<(StatusCode, Json<PermissionRuleDto>)> {
    crate::db::projects::get_project(&state.pool, &project_id)
        .await?
        .ok_or_else(|| AppError::NotFound("Project not found".into()))?;
    let (effect, tool, pattern) = validated(&req)?;

    let rule = rule_db::create_rule(
        &state.pool,
        rule_db::NewPermissionRule {
            project_id: Some(&project_id),
            conversation_id: None,
            effect: effect.as_str(),
            tool: &tool,
            pattern: pattern.as_deref(),
            source: "user",
        },
    )
    .await?;
    Ok((StatusCode::CREATED, Json(PermissionRuleDto::from_row(rule))))
}

/// The global rules (every project's runs evaluate them).
#[utoipa::path(
    get,
    path = "/permission-rules",
    responses(
        (status = 200, description = "Global rules", body = Vec<PermissionRuleDto>)
    ),
    tag = "permission-rules"
)]
pub async fn list_global_permission_rules(
    State(state): State<AppState>,
) -> ApiResult<Json<Vec<PermissionRuleDto>>> {
    let rules = rule_db::list_global_rules(&state.pool).await?;
    Ok(Json(rules.into_iter().map(PermissionRuleDto::from_row).collect()))
}

/// Create a global rule (`project_id = NULL`).
#[utoipa::path(
    post,
    path = "/permission-rules",
    request_body = CreatePermissionRuleRequest,
    responses(
        (status = 201, description = "Created rule", body = PermissionRuleDto),
        (status = 400, description = "Invalid effect or empty tool")
    ),
    tag = "permission-rules"
)]
pub async fn create_global_permission_rule(
    State(state): State<AppState>,
    Json(req): Json<CreatePermissionRuleRequest>,
) -> ApiResult<(StatusCode, Json<PermissionRuleDto>)> {
    let (effect, tool, pattern) = validated(&req)?;

    let rule = rule_db::create_rule(
        &state.pool,
        rule_db::NewPermissionRule {
            project_id: None,
            conversation_id: None,
            effect: effect.as_str(),
            tool: &tool,
            pattern: pattern.as_deref(),
            source: "user",
        },
    )
    .await?;
    Ok((StatusCode::CREATED, Json(PermissionRuleDto::from_row(rule))))
}

/// Delete a rule (either scope — the id is global).
#[utoipa::path(
    delete,
    path = "/permission-rules/{id}",
    params(("id" = String, Path, description = "Rule ID")),
    responses(
        (status = 204, description = "Rule deleted"),
        (status = 404, description = "Rule not found")
    ),
    tag = "permission-rules"
)]
pub async fn delete_permission_rule(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let deleted = rule_db::delete_rule(&state.pool, &id).await?;
    if deleted {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound("Permission rule not found".into()))
    }
}
