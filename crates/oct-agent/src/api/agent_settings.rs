//! Agent settings transport DTOs and three-state PATCH decoding.
use super::{
    AppState,
    error::{ApiResult, AppError},
};
use crate::agent_settings as domain;
use crate::application::settings::SettingsApplication;
use crate::tool_contract::ToolSelector;
use axum::{
    Json,
    extract::{Path, State},
};
use serde::{Deserialize, Deserializer, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum ModelChoice {
    Parent,
    Fixed {
        provider_id: String,
        model_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolChoice {
    Parent,
    Allowlist { items: Vec<ToolSelector> },
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentOverride {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<ToolChoice>,
}

fn patch_field<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(d).map(Some)
}

#[derive(Debug, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentPatch {
    #[serde(default, deserialize_with = "patch_field")]
    pub model: Option<Option<ModelChoice>>,
    #[serde(default, deserialize_with = "patch_field")]
    pub instructions: Option<Option<String>>,
    #[serde(default, deserialize_with = "patch_field")]
    pub tools: Option<Option<ToolChoice>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AgentSettings {
    pub model: ModelChoice,
    pub instructions: String,
    pub tools: ToolChoice,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SettingSources {
    pub model: String,
    pub instructions: String,
    pub tools: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct AgentSettingsView {
    pub key: String,
    pub name: String,
    pub description: String,
    pub delegatable: bool,
    pub tools_editable: bool,
    pub overrides: AgentOverride,
    pub effective: AgentSettings,
    pub global_effective: AgentSettings,
    pub sources: SettingSources,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SnapshotTool {
    pub selector: ToolSelector,
    pub exposed_name: String,
    /// Frozen presentation: the description rendered into the system prompt
    /// (`[mcp:{server}] ...` embeds the server name, so a rename would
    /// change the bytes). Empty on snapshots predating the field — restore
    /// then falls back to the live description.
    #[serde(default)]
    pub exposed_description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkerSnapshot {
    pub version: u32,
    pub agent_type: String,
    pub instructions: String,
    pub prompt_template_version: u32,
    pub tools: Vec<SnapshotTool>,
    pub sources: SettingSources,
    #[serde(default)]
    pub tool_mode: crate::tool_contract::McpToolMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_catalog_version: Option<u32>,
}

fn application(state: &AppState) -> SettingsApplication {
    SettingsApplication::new(state.pool.clone())
}

impl From<ModelChoice> for domain::ModelChoice {
    fn from(value: ModelChoice) -> Self {
        match value {
            ModelChoice::Parent => Self::Parent,
            ModelChoice::Fixed {
                provider_id,
                model_id,
            } => Self::Fixed {
                provider_id,
                model_id,
            },
        }
    }
}
impl From<domain::ModelChoice> for ModelChoice {
    fn from(value: domain::ModelChoice) -> Self {
        match value {
            domain::ModelChoice::Parent => Self::Parent,
            domain::ModelChoice::Fixed {
                provider_id,
                model_id,
            } => Self::Fixed {
                provider_id,
                model_id,
            },
        }
    }
}
impl From<ToolChoice> for domain::ToolChoice {
    fn from(value: ToolChoice) -> Self {
        match value {
            ToolChoice::Parent => Self::Parent,
            ToolChoice::Allowlist { items } => Self::Allowlist { items },
        }
    }
}
impl From<domain::ToolChoice> for ToolChoice {
    fn from(value: domain::ToolChoice) -> Self {
        match value {
            domain::ToolChoice::Parent => Self::Parent,
            domain::ToolChoice::Allowlist { items } => Self::Allowlist { items },
        }
    }
}
fn field_patch<T, U: From<T>>(value: Option<Option<T>>) -> domain::Patch<U> {
    match value {
        None => domain::Patch::Keep,
        Some(None) => domain::Patch::Inherit,
        Some(Some(value)) => domain::Patch::Set(value.into()),
    }
}
impl From<AgentPatch> for domain::SettingsPatch {
    fn from(patch: AgentPatch) -> Self {
        Self {
            model: field_patch(patch.model),
            instructions: field_patch(patch.instructions),
            tools: field_patch(patch.tools),
        }
    }
}
impl From<domain::AgentOverride> for AgentOverride {
    fn from(value: domain::AgentOverride) -> Self {
        Self {
            model: value.model.map(Into::into),
            instructions: value.instructions,
            tools: value.tools.map(Into::into),
        }
    }
}
impl From<domain::AgentSettings> for AgentSettings {
    fn from(value: domain::AgentSettings) -> Self {
        Self {
            model: value.model.into(),
            instructions: value.instructions,
            tools: value.tools.into(),
        }
    }
}
impl From<domain::SettingSources> for SettingSources {
    fn from(value: domain::SettingSources) -> Self {
        Self {
            model: value.model,
            instructions: value.instructions,
            tools: value.tools,
        }
    }
}
impl From<domain::ResolvedSettings> for AgentSettingsView {
    fn from(value: domain::ResolvedSettings) -> Self {
        let worker = value.key == domain::AgentKey::Worker;
        Self {
            key: value.key.as_str().into(),
            name: if worker {
                "Worker 子代理"
            } else {
                "Title 标题生成"
            }
            .into(),
            description: if worker {
                "委派任务的模型、角色指令与工具"
            } else {
                "自动生成标题的专用模型与命名偏好"
            }
            .into(),
            delegatable: worker,
            tools_editable: worker,
            overrides: value.overrides.into(),
            effective: value.effective.into(),
            global_effective: value.global_effective.into(),
            sources: value.sources.into(),
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AgentToolOption {
    pub selector: ToolSelector,
    pub label: String,
    pub group: String,
    pub available: bool,
}
impl From<domain::ToolOption> for AgentToolOption {
    fn from(value: domain::ToolOption) -> Self {
        Self {
            selector: value.selector,
            label: value.name,
            group: value.server_name.unwrap_or_else(|| "内置工具".into()),
            available: value.available,
        }
    }
}
#[utoipa::path(get, path = "/settings/agents", tag = "settings", responses((status = 200, body = Vec<AgentSettingsView>), (status = 400, body = super::error::ErrorBody), (status = 404, body = super::error::ErrorBody), (status = 500, body = super::error::ErrorBody)))]
pub async fn list_global(State(state): State<AppState>) -> ApiResult<Json<Vec<AgentSettingsView>>> {
    Ok(Json(
        application(&state)
            .list(None)
            .await?
            .into_iter()
            .map(Into::into)
            .collect(),
    ))
}

#[utoipa::path(get, path = "/settings/agents/{key}", tag = "settings", params(("key" = String, Path)), responses((status = 200, body = AgentSettingsView), (status = 400, body = super::error::ErrorBody), (status = 404, body = super::error::ErrorBody), (status = 500, body = super::error::ErrorBody)))]
pub async fn get_global(
    State(state): State<AppState>,
    Path(key): Path<String>,
) -> ApiResult<Json<AgentSettingsView>> {
    Ok(Json(application(&state).read(&key, None).await?.into()))
}

#[utoipa::path(patch, path = "/settings/agents/{key}", tag = "settings", params(("key" = String, Path)), request_body = AgentPatch, responses((status = 200, body = AgentSettingsView), (status = 400, body = super::error::ErrorBody), (status = 404, body = super::error::ErrorBody), (status = 500, body = super::error::ErrorBody)))]
pub async fn update_global(
    State(state): State<AppState>,
    Path(key): Path<String>,
    payload: Result<Json<AgentPatch>, axum::extract::rejection::JsonRejection>,
) -> ApiResult<Json<AgentSettingsView>> {
    let patch = payload
        .map_err(|error| AppError::BadRequest(error.body_text()))?
        .0;
    Ok(Json(
        application(&state)
            .update(&key, None, patch.into(), &state.mcp)
            .await?
            .into(),
    ))
}

#[utoipa::path(get, path = "/projects/{id}/settings/agents", tag = "settings", params(("id" = String, Path)), responses((status = 200, body = Vec<AgentSettingsView>), (status = 400, body = super::error::ErrorBody), (status = 404, body = super::error::ErrorBody), (status = 500, body = super::error::ErrorBody)))]
pub async fn list_project(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<AgentSettingsView>>> {
    Ok(Json(
        application(&state)
            .list(Some(&id))
            .await?
            .into_iter()
            .map(Into::into)
            .collect(),
    ))
}

#[utoipa::path(get, path = "/projects/{id}/settings/agents/{key}", tag = "settings", params(("id" = String, Path), ("key" = String, Path)), responses((status = 200, body = AgentSettingsView), (status = 400, body = super::error::ErrorBody), (status = 404, body = super::error::ErrorBody), (status = 500, body = super::error::ErrorBody)))]
pub async fn get_project(
    State(state): State<AppState>,
    Path((id, key)): Path<(String, String)>,
) -> ApiResult<Json<AgentSettingsView>> {
    Ok(Json(
        application(&state).read(&key, Some(&id)).await?.into(),
    ))
}

#[utoipa::path(patch, path = "/projects/{id}/settings/agents/{key}", tag = "settings", params(("id" = String, Path), ("key" = String, Path)), request_body = AgentPatch, responses((status = 200, body = AgentSettingsView), (status = 400, body = super::error::ErrorBody), (status = 404, body = super::error::ErrorBody), (status = 500, body = super::error::ErrorBody)))]
pub async fn update_project(
    State(state): State<AppState>,
    Path((id, key)): Path<(String, String)>,
    payload: Result<Json<AgentPatch>, axum::extract::rejection::JsonRejection>,
) -> ApiResult<Json<AgentSettingsView>> {
    let patch = payload
        .map_err(|error| AppError::BadRequest(error.body_text()))?
        .0;
    Ok(Json(
        application(&state)
            .update(&key, Some(&id), patch.into(), &state.mcp)
            .await?
            .into(),
    ))
}

#[utoipa::path(get, path = "/settings/agent-tools", tag = "settings", responses((status = 200, body = Vec<AgentToolOption>), (status = 400, body = super::error::ErrorBody), (status = 404, body = super::error::ErrorBody), (status = 500, body = super::error::ErrorBody)))]
pub async fn global_tools(State(state): State<AppState>) -> ApiResult<Json<Vec<AgentToolOption>>> {
    Ok(Json(
        application(&state)
            .catalog(None, &state.mcp)
            .await?
            .into_iter()
            .map(Into::into)
            .collect(),
    ))
}

#[utoipa::path(get, path = "/projects/{id}/settings/agent-tools", tag = "settings", params(("id" = String, Path)), responses((status = 200, body = Vec<AgentToolOption>), (status = 400, body = super::error::ErrorBody), (status = 404, body = super::error::ErrorBody), (status = 500, body = super::error::ErrorBody)))]
pub async fn project_tools(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<AgentToolOption>>> {
    Ok(Json(
        application(&state)
            .catalog(Some(&id), &state.mcp)
            .await?
            .into_iter()
            .map(Into::into)
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn http_patch_distinguishes_omission_null_and_explicit_values() {
        let decode = |value| -> domain::SettingsPatch {
            serde_json::from_value::<AgentPatch>(value).unwrap().into()
        };
        let omitted = decode(json!({}));
        assert_eq!(omitted.model, domain::Patch::Keep);
        assert_eq!(omitted.instructions, domain::Patch::Keep);
        assert_eq!(omitted.tools, domain::Patch::Keep);
        let reset = decode(json!({"model":null,"instructions":null,"tools":null}));
        assert_eq!(reset.model, domain::Patch::Inherit);
        assert_eq!(reset.instructions, domain::Patch::Inherit);
        assert_eq!(reset.tools, domain::Patch::Inherit);
        let explicit = decode(json!({
            "model":{"mode":"parent"},"instructions":"",
            "tools":{"mode":"allowlist","items":[]}
        }));
        assert_eq!(
            explicit.model,
            domain::Patch::Set(domain::ModelChoice::Parent)
        );
        assert_eq!(explicit.instructions, domain::Patch::Set(String::new()));
        assert_eq!(
            explicit.tools,
            domain::Patch::Set(domain::ToolChoice::Allowlist { items: vec![] })
        );
    }
}
