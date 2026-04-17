pub mod chat;
pub mod config;
pub mod conversations;

use axum::Router;
use dashmap::DashMap;
use sqlx::SqlitePool;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use utoipa::OpenApi;
use utoipa_axum::router::OpenApiRouter;
use utoipa_scalar::{Scalar, Servable};

use crate::agent::RunHandle;
use oct_llm_provider::provider::ProviderRegistry;

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub registry: Arc<ProviderRegistry>,
    pub provider_spec: Arc<RwLock<String>>,
    pub working_dir: PathBuf,
    pub sessions: Arc<DashMap<String, RunHandle>>,
}

#[derive(OpenApi)]
#[openapi(
    info(title = "Oct Agent API", version = "0.1.0"),
    components(schemas(
        crate::agent::AgentEvent,
        crate::db::conversations::Conversation,
        crate::db::messages::StoredMessage,
        conversations::CreateConversationRequest,
        conversations::UpdateConversationTitleRequest,
        chat::SendMessageRequest,
        config::AgentConfigResponse,
        config::UpdateConfigRequest,
    ))
)]
pub struct ApiDoc;

/// Build the application router with all routes and OpenAPI doc.
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
        .routes(utoipa_axum::routes!(conversations::list_conversations))
        .routes(utoipa_axum::routes!(conversations::create_conversation))
        .routes(utoipa_axum::routes!(conversations::get_conversation))
        .routes(utoipa_axum::routes!(conversations::delete_conversation))
        .routes(utoipa_axum::routes!(
            conversations::update_conversation_title
        ))
        .routes(utoipa_axum::routes!(chat::send_message))
        .routes(utoipa_axum::routes!(chat::get_messages))
        .routes(utoipa_axum::routes!(config::get_config))
        .routes(utoipa_axum::routes!(config::update_config))
        .with_state(state)
}
