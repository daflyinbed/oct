pub mod agent_settings;
pub mod approvals;
pub mod background;
pub mod chat;
pub mod conversations;
pub mod dto;
pub mod error;
pub mod files;
pub mod git;
pub mod mcp;
pub mod permissions;
pub mod projects;
pub mod providers;
pub mod settings;
pub mod subagents;

use axum::Router;
use dashmap::DashMap;
use sqlx::SqlitePool;
use std::sync::Arc;
use utoipa::OpenApi;
use utoipa_axum::router::OpenApiRouter;
use utoipa_scalar::{Scalar, Servable};

use crate::agent::RunHandle;
use crate::mcp::McpManager;
use crate::oauth::OAuthFlowStore;
use crate::runtime::RunService;
use oct_llm_provider::provider::ProviderRegistry;

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub registry: Arc<ProviderRegistry>,
    pub sessions: Arc<DashMap<String, RunHandle>>,
    /// Application-layer run lifecycle: conversation
    /// occupancy, agent_runs persistence and loop supervision. Holds clones
    /// of the `Arc`s above — `sessions` in particular is the SAME instance,
    /// so the reconnect/status endpoints keep observing the same occupancy.
    pub runs: Arc<RunService>,
    /// Process-wide MCP connection manager: reconciled against the enabled
    /// `mcp_servers` rows on every chat turn, and read by the MCP list/get
    /// endpoints for live connection status.
    pub mcp: Arc<McpManager>,
    /// In-flight OAuth authorization flows (`state` → verifier + endpoints),
    /// bridging `oauth/start` and the callback. Memory-only by design; see
    /// `crate::oauth`.
    pub oauth_flows: Arc<OAuthFlowStore>,
    /// Live handle onto the LLM wire-log sink (the `settings` API flips
    /// it). Installed by `main` before the subscriber is assembled.
    pub wire_log: oct_llm_provider::wire::WireLogControl,
}

#[derive(OpenApi)]
#[openapi(
    info(title = "Oct Agent API", version = "0.1.0"),
    components(schemas(
        crate::agent::AgentEvent,
        crate::db::conversations::Conversation,
        crate::db::conversations::CreateConversationRequest,
        crate::db::conversations::UpdateConversationRequest,
        crate::db::history::ForkCut,
        crate::db::history::ForkSide,
        crate::db::history::ForkRequest,
        crate::db::history::ForkResponse,
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
        crate::db::mcp::CreateMcpServerRequest,
        crate::db::mcp::UpdateMcpServerRequest,
        mcp::McpServerResponse,
        mcp::McpToolSummary,
        mcp::McpPromptArgumentSummary,
        mcp::McpPromptSummary,
        mcp::McpResourceSummary,
        mcp::McpServerOauthSummary,
        mcp::OAuthStartResponse,
        settings::AppSettings,
        settings::UpdateAppSettings,
        settings::ProjectMcpSettings,
        settings::UpdateProjectMcpSettings,
        crate::codemode::NestedCall,
        mcp::RenderPromptRequest,
        mcp::RenderPromptResponse,
        mcp::PromptMessageResponse,
        mcp::ReadResourceRequest,
        mcp::ReadResourceResponse,
        chat::SendMessageRequest,
        chat::RunningStatus,
        chat::CompactQuery,
        chat::CompactResult,
        chat::InstructionFileDto,
        chat::InstructionsReloadResult,
        subagents::SubagentTaskDto,
        background::BackgroundTaskDto,
        background::TaskOutputQuery,
        background::TaskOutputChunkDto,
        approvals::PendingApprovalDto,
        approvals::DecideApprovalRequest,
        approvals::DecideApprovalResponse,
        permissions::PermissionRuleDto,
        permissions::CreatePermissionRuleRequest,
        error::ErrorBody,
        dto::MessageDto,
        dto::MessagePart,
        dto::MessageDetails,
        dto::KnownDetailsKind,
        dto::CompactionMetadata,
        dto::ToolResultImage,
        crate::agent::CompactionTrigger,
        crate::agent::MicrocompactTrigger,
        crate::agent::MicrocompactReplacement,
        crate::agent::SubagentRunStatus,
        crate::agent::ApprovalOutcome,
        files::FileContent,
        files::FileEntry,
        files::FileEntryKind,
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
        // MCP servers
        .routes(utoipa_axum::routes!(mcp::list_mcp_servers))
        .routes(utoipa_axum::routes!(mcp::ensure_project_mcp_servers))
        .routes(utoipa_axum::routes!(mcp::create_mcp_server))
        .routes(utoipa_axum::routes!(mcp::get_mcp_server))
        .routes(utoipa_axum::routes!(mcp::update_mcp_server))
        .routes(utoipa_axum::routes!(mcp::delete_mcp_server))
        // MCP prompts (slash menu) & resources (panel viewer)
        .routes(utoipa_axum::routes!(mcp::render_mcp_prompt))
        .routes(utoipa_axum::routes!(mcp::read_mcp_resource))
        // MCP OAuth (http transport): start / browser callback / logout
        .routes(utoipa_axum::routes!(mcp::start_mcp_oauth))
        .routes(utoipa_axum::routes!(mcp::mcp_oauth_callback))
        .routes(utoipa_axum::routes!(mcp::delete_mcp_oauth))
        // Projects
        .routes(utoipa_axum::routes!(projects::list_projects))
        .routes(utoipa_axum::routes!(projects::create_project))
        .routes(utoipa_axum::routes!(projects::get_project))
        .routes(utoipa_axum::routes!(projects::update_project))
        .routes(utoipa_axum::routes!(projects::delete_project))
        // Project files (directory tree + content preview)
        .routes(utoipa_axum::routes!(files::list_project_files))
        .routes(utoipa_axum::routes!(files::read_project_file))
        // Git Changes panel
        .routes(utoipa_axum::routes!(git::get_status))
        .routes(utoipa_axum::routes!(git::get_diff))
        .routes(utoipa_axum::routes!(git::get_history))
        .routes(utoipa_axum::routes!(git::get_stashes))
        .routes(utoipa_axum::routes!(git::run_action))
        // Conversations
        .routes(utoipa_axum::routes!(conversations::list_conversations))
        .routes(utoipa_axum::routes!(conversations::create_conversation))
        .routes(utoipa_axum::routes!(conversations::get_conversation))
        .routes(utoipa_axum::routes!(conversations::delete_conversation))
        .routes(utoipa_axum::routes!(conversations::update_conversation))
        .routes(utoipa_axum::routes!(conversations::fork_conversation))
        .routes(utoipa_axum::routes!(conversations::revert_conversation))
        // Tool approvals: pending list + decide
        .routes(utoipa_axum::routes!(approvals::list_pending_approvals))
        .routes(utoipa_axum::routes!(approvals::decide_approval))
        // Application settings (LLM wire log switch)
        .routes(utoipa_axum::routes!(settings::get_settings))
        .routes(utoipa_axum::routes!(settings::update_settings))
        .routes(utoipa_axum::routes!(settings::get_project_mcp_settings))
        .routes(utoipa_axum::routes!(settings::update_project_mcp_settings))
        .routes(utoipa_axum::routes!(agent_settings::list_global))
        .routes(utoipa_axum::routes!(agent_settings::get_global))
        .routes(utoipa_axum::routes!(agent_settings::update_global))
        .routes(utoipa_axum::routes!(agent_settings::list_project))
        .routes(utoipa_axum::routes!(agent_settings::get_project))
        .routes(utoipa_axum::routes!(agent_settings::update_project))
        .routes(utoipa_axum::routes!(agent_settings::global_tools))
        .routes(utoipa_axum::routes!(agent_settings::project_tools))
        // Permission rules: project scope + global scope + delete
        .routes(utoipa_axum::routes!(
            permissions::list_project_permission_rules
        ))
        .routes(utoipa_axum::routes!(
            permissions::create_project_permission_rule
        ))
        .routes(utoipa_axum::routes!(
            permissions::list_global_permission_rules
        ))
        .routes(utoipa_axum::routes!(
            permissions::create_global_permission_rule
        ))
        .routes(utoipa_axum::routes!(permissions::delete_permission_rule))
        // Chat
        .routes(utoipa_axum::routes!(chat::send_message))
        .routes(utoipa_axum::routes!(chat::get_messages))
        .routes(utoipa_axum::routes!(chat::get_original_message))
        .routes(utoipa_axum::routes!(chat::cancel_agent))
        .routes(utoipa_axum::routes!(chat::resume_turn))
        .routes(utoipa_axum::routes!(chat::compact_conversation))
        // Frozen prompt environment refresh (repo instructions + MCP set)
        .routes(utoipa_axum::routes!(chat::reload_instructions))
        // Run reconnection (frontend refresh while a run is in flight)
        .routes(utoipa_axum::routes!(chat::get_run_status))
        .routes(utoipa_axum::routes!(chat::stream_run_events))
        // Background bash tasks (list / output read / cancel)
        .routes(utoipa_axum::routes!(background::list_background_tasks))
        .routes(utoipa_axum::routes!(
            background::read_background_task_output
        ))
        .routes(utoipa_axum::routes!(background::cancel_background_task))
        // Subagents (task list / child history / child events / child cancel)
        .routes(utoipa_axum::routes!(subagents::list_subagents))
        .routes(utoipa_axum::routes!(subagents::get_subagent_messages))
        .routes(utoipa_axum::routes!(subagents::stream_subagent_events))
        .routes(utoipa_axum::routes!(subagents::cancel_subagent))
        .with_state(state)
}

mod sse;
