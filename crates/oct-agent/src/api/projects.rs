use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;

use super::AppState;
use super::error::{AppError, ApiResult};
use crate::db::projects as db;

#[utoipa::path(
    get,
    path = "/projects",
    responses(
        (status = 200, description = "List of projects", body = Vec<db::Project>)
    ),
    tag = "projects"
)]
pub async fn list_projects(
    State(state): State<AppState>,
) -> ApiResult<Json<Vec<db::Project>>> {
    let projects = db::list_projects(&state.pool).await?;
    Ok(Json(projects))
}

#[utoipa::path(
    post,
    path = "/projects",
    request_body = db::CreateProjectRequest,
    responses(
        (status = 201, description = "Created project", body = db::Project)
    ),
    tag = "projects"
)]
pub async fn create_project(
    State(state): State<AppState>,
    Json(req): Json<db::CreateProjectRequest>,
) -> ApiResult<(StatusCode, Json<db::Project>)> {
    let project = db::create_project(&state.pool, &req).await?;
    Ok((StatusCode::CREATED, Json(project)))
}

#[utoipa::path(
    get,
    path = "/projects/{id}",
    params(("id" = String, Path, description = "Project ID")),
    responses(
        (status = 200, description = "Project found", body = db::Project),
        (status = 404, description = "Project not found")
    ),
    tag = "projects"
)]
pub async fn get_project(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<db::Project>> {
    let project = db::get_project(&state.pool, &id)
        .await?
        .ok_or_else(|| AppError::NotFound("Project not found".into()))?;
    Ok(Json(project))
}

#[utoipa::path(
    patch,
    path = "/projects/{id}",
    params(("id" = String, Path, description = "Project ID")),
    request_body = db::UpdateProjectRequest,
    responses(
        (status = 200, description = "Project updated"),
        (status = 404, description = "Project not found")
    ),
    tag = "projects"
)]
pub async fn update_project(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<db::UpdateProjectRequest>,
) -> ApiResult<StatusCode> {
    let updated = db::update_project(&state.pool, &id, &req).await?;
    if updated {
        Ok(StatusCode::OK)
    } else {
        Err(AppError::NotFound("Project not found".into()))
    }
}

#[utoipa::path(
    delete,
    path = "/projects/{id}",
    params(("id" = String, Path, description = "Project ID")),
    responses(
        (status = 204, description = "Project deleted"),
        (status = 404, description = "Project not found")
    ),
    tag = "projects"
)]
pub async fn delete_project(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let deleted = db::delete_project(&state.pool, &id).await?;
    if deleted {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound("Project not found".into()))
    }
}
