use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::NaiveDateTime;
use serde::Serialize;
use utoipa::ToSchema;

use super::AppState;
use super::error::{AppError, ApiResult};
use crate::db::{models as model_db, providers as db};

#[derive(Debug, Serialize, ToSchema)]
pub struct ProviderResponse {
    pub id: String,
    pub name: String,
    pub adapter_type: String,
    pub base_url: Option<String>,
    pub api_key_set: bool,
    pub doc_url: Option<String>,
    pub source: String,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
    pub models: Vec<ModelSummary>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ModelSummary {
    pub model_id: String,
    pub name: String,
    pub family: Option<String>,
    pub reasoning: bool,
    pub tool_call: bool,
    pub attachment: bool,
    pub limit_context: Option<i64>,
    pub limit_output: Option<i64>,
    pub cost_input: Option<f64>,
    pub cost_output: Option<f64>,
    pub is_enabled: bool,
    pub source: String,
}

fn to_provider_response(p: &db::Provider, models: Vec<ModelSummary>) -> ProviderResponse {
    ProviderResponse {
        id: p.id.clone(),
        name: p.name.clone(),
        adapter_type: p.adapter_type.clone(),
        base_url: p.base_url.clone(),
        api_key_set: !p.api_key.is_empty(),
        doc_url: p.doc_url.clone(),
        source: p.source.clone(),
        created_at: p.created_at,
        updated_at: p.updated_at,
        models,
    }
}

fn model_to_summary(m: &model_db::Model) -> ModelSummary {
    ModelSummary {
        model_id: m.model_id.clone(),
        name: m.name.clone(),
        family: m.family.clone(),
        reasoning: m.reasoning,
        tool_call: m.tool_call,
        attachment: m.attachment,
        limit_context: m.limit_context,
        limit_output: m.limit_output,
        cost_input: m.cost_input,
        cost_output: m.cost_output,
        is_enabled: m.is_enabled,
        source: m.source.clone(),
    }
}

#[utoipa::path(
    get,
    path = "/providers",
    responses(
        (status = 200, description = "List of providers with their models", body = Vec<ProviderResponse>)
    ),
    tag = "providers"
)]
pub async fn list_providers(
    State(state): State<AppState>,
) -> ApiResult<Json<Vec<ProviderResponse>>> {
    let providers = db::list_providers(&state.pool).await?;
    let mut result = Vec::with_capacity(providers.len());

    for p in &providers {
        let models = model_db::list_models_by_provider(&state.pool, &p.id).await?;
        let summaries: Vec<ModelSummary> = models.iter().map(model_to_summary).collect();
        result.push(to_provider_response(p, summaries));
    }

    Ok(Json(result))
}

#[utoipa::path(
    post,
    path = "/providers",
    request_body = db::CreateProviderRequest,
    responses(
        (status = 201, description = "Created provider", body = ProviderResponse),
        (status = 409, description = "Provider ID already exists")
    ),
    tag = "providers"
)]
pub async fn create_provider(
    State(state): State<AppState>,
    Json(req): Json<db::CreateProviderRequest>,
) -> ApiResult<(StatusCode, Json<ProviderResponse>)> {
    if db::get_provider(&state.pool, &req.id).await?.is_some() {
        return Err(AppError::Conflict(format!(
            "Provider '{}' already exists",
            req.id
        )));
    }

    let provider = db::create_custom_provider(&state.pool, &req).await?;
    let resp = to_provider_response(&provider, vec![]);

    Ok((StatusCode::CREATED, Json(resp)))
}

#[utoipa::path(
    put,
    path = "/providers/{id}",
    params(("id" = String, Path, description = "Provider ID")),
    request_body = db::UpdateProviderRequest,
    responses(
        (status = 200, description = "Provider updated"),
        (status = 404, description = "Provider not found")
    ),
    tag = "providers"
)]
pub async fn update_provider(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<db::UpdateProviderRequest>,
) -> ApiResult<StatusCode> {
    let updated = db::update_provider(&state.pool, &id, &req).await?;
    if updated {
        Ok(StatusCode::OK)
    } else {
        Err(AppError::NotFound("Provider not found".into()))
    }
}

#[utoipa::path(
    delete,
    path = "/providers/{id}",
    params(("id" = String, Path, description = "Provider ID")),
    responses(
        (status = 204, description = "Provider deleted"),
        (status = 404, description = "Provider not found or is not deletable")
    ),
    tag = "providers"
)]
pub async fn delete_provider(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let deleted = db::delete_provider(&state.pool, &id).await?;
    if deleted {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound(
            "Provider not found or is not deletable".into(),
        ))
    }
}

#[utoipa::path(
    post,
    path = "/providers/{id}/models",
    params(("id" = String, Path, description = "Provider ID")),
    request_body = model_db::CreateModelRequest,
    responses(
        (status = 201, description = "Model created", body = ModelSummary),
        (status = 404, description = "Provider not found"),
        (status = 409, description = "Model already exists for this provider")
    ),
    tag = "providers"
)]
pub async fn create_model(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<model_db::CreateModelRequest>,
) -> ApiResult<(StatusCode, Json<ModelSummary>)> {
    if db::get_provider(&state.pool, &id).await?.is_none() {
        return Err(AppError::NotFound("Provider not found".into()));
    }

    if model_db::get_model(&state.pool, &id, &req.model_id)
        .await?
        .is_some()
    {
        return Err(AppError::Conflict(format!(
            "Model '{}' already exists for provider '{}'",
            req.model_id, id
        )));
    }

    let model = model_db::create_model(&state.pool, &id, &req).await?;
    Ok((StatusCode::CREATED, Json(model_to_summary(&model))))
}

#[utoipa::path(
    put,
    path = "/providers/{providerId}/models/{modelId}",
    params(
        ("providerId" = String, Path, description = "Provider ID"),
        ("modelId" = String, Path, description = "Model ID"),
    ),
    request_body = model_db::UpdateModelRequest,
    responses(
        (status = 200, description = "Model updated"),
        (status = 404, description = "Model not found")
    ),
    tag = "providers"
)]
pub async fn update_model(
    State(state): State<AppState>,
    Path((provider_id, model_id)): Path<(String, String)>,
    Json(req): Json<model_db::UpdateModelRequest>,
) -> ApiResult<StatusCode> {
    let updated = model_db::update_model(&state.pool, &provider_id, &model_id, &req).await?;
    if updated {
        Ok(StatusCode::OK)
    } else {
        Err(AppError::NotFound("Model not found".into()))
    }
}

#[utoipa::path(
    delete,
    path = "/providers/{providerId}/models/{modelId}",
    params(
        ("providerId" = String, Path, description = "Provider ID"),
        ("modelId" = String, Path, description = "Model ID"),
    ),
    responses(
        (status = 204, description = "Model deleted"),
        (status = 404, description = "Model not found or is not deletable")
    ),
    tag = "providers"
)]
pub async fn delete_model(
    State(state): State<AppState>,
    Path((provider_id, model_id)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    let deleted = model_db::delete_model(&state.pool, &provider_id, &model_id).await?;
    if deleted {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound(
            "Model not found or is not deletable".into(),
        ))
    }
}
