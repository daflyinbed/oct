pub mod chat;
pub mod conversations;
pub mod error;
pub mod projects;
pub mod providers;

use axum::Router;
use dashmap::DashMap;
use sqlx::PgPool;
use std::sync::Arc;
use utoipa::OpenApi;
use utoipa_axum::router::OpenApiRouter;
use utoipa_scalar::{Scalar, Servable};

use crate::agent::RunHandle;
use oct_llm_provider::provider::ProviderRegistry;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub registry: Arc<ProviderRegistry>,
    pub sessions: Arc<DashMap<String, RunHandle>>,
}

#[derive(OpenApi)]
#[openapi(
    info(title = "Oct Agent API", version = "0.1.0"),
    components(schemas(
        crate::agent::AgentEvent,
        crate::db::conversations::Conversation,
        crate::db::conversations::CreateConversationRequest,
        crate::db::conversations::UpdateConversationRequest,
        crate::db::messages::StoredMessage,
        crate::db::projects::Project,
        crate::db::projects::CreateProjectRequest,
        crate::db::projects::UpdateProjectRequest,
        crate::db::providers::Provider,
        crate::db::providers::CreateProviderRequest,
        crate::db::providers::UpdateProviderRequest,
        crate::db::models::Model,
        crate::db::models::CreateModelRequest,
        crate::db::models::UpdateModelRequest,
        chat::SendMessageRequest,
        providers::ProviderResponse,
        providers::ModelSummary,
    ))
)]
pub struct ApiDoc;

pub fn build_router(state: AppState) -> Router {
    let (router, api) = OpenApiRouter::with_openapi(ApiDoc::openapi())
        .nest("/api", api_routes(state.clone()))
        .split_for_parts();

    let api_clone = api.clone();
    router
        .merge(Scalar::with_url("/scalar", api))
        .route(
            "/api/openapi.json",
            axum::routing::get(move || async move { axum::Json(api_clone) }),
        )
        .with_state(state)
}

fn api_routes(state: AppState) -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        // Providers & Models
        .routes(utoipa_axum::routes!(providers::list_providers))
        .routes(utoipa_axum::routes!(providers::create_provider))
        .routes(utoipa_axum::routes!(providers::update_provider))
        .routes(utoipa_axum::routes!(providers::delete_provider))
        .routes(utoipa_axum::routes!(providers::create_model))
        .routes(utoipa_axum::routes!(providers::update_model))
        .routes(utoipa_axum::routes!(providers::delete_model))
        // Projects
        .routes(utoipa_axum::routes!(projects::list_projects))
        .routes(utoipa_axum::routes!(projects::create_project))
        .routes(utoipa_axum::routes!(projects::get_project))
        .routes(utoipa_axum::routes!(projects::update_project))
        .routes(utoipa_axum::routes!(projects::delete_project))
        // Conversations
        .routes(utoipa_axum::routes!(conversations::list_conversations))
        .routes(utoipa_axum::routes!(conversations::create_conversation))
        .routes(utoipa_axum::routes!(conversations::get_conversation))
        .routes(utoipa_axum::routes!(conversations::delete_conversation))
        .routes(utoipa_axum::routes!(conversations::update_conversation))
        // Chat
        .routes(utoipa_axum::routes!(chat::send_message))
        .routes(utoipa_axum::routes!(chat::get_messages))
        .with_state(state)
}
