use anyhow::Result;
use clap::Parser;
use std::sync::Arc;
use tower_http::cors::CorsLayer;
use tracing::info;

use oct_agent::{api, db, seed};
use oct_llm_provider::providers::default_registry;

#[derive(Parser)]
#[command(name = "oct-agent", version, about = "Oct Agent CLI")]
enum Cli {
    /// Start the API server
    Serve {
        /// Port to listen on
        #[arg(long, env = "OCT_PORT", default_value = "3000")]
        port: u16,
    },
    /// Seed providers and models from models.dev
    Seed,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "oct_agent=info,tower_http=debug".into()),
        )
        .init();

    let cli = Cli::parse();
    let database_url =
        std::env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite://oct.db".to_string());

    match cli {
        Cli::Serve { port } => run_server(&database_url, port).await,
        Cli::Seed => seed::run(&database_url).await,
    }
}

async fn run_server(database_url: &str, port: u16) -> Result<()> {
    let pool = db::init_pool(database_url).await?;
    let registry = Arc::new(default_registry());
    let sessions: Arc<dashmap::DashMap<String, oct_agent::agent::RunHandle>> =
        Arc::new(dashmap::DashMap::new());
    let mcp = Arc::new(oct_agent::mcp::McpManager::default());

    // Crash recovery (ADR 0002 §6.2): agent_runs rows a previous process
    // left `running` become `lost` before this process accepts traffic.
    oct_agent::runtime::bootstrap(&pool).await;

    // Two-phase wire: the RunService owns the SubagentManager and the
    // background task manager; each manager reaches back into the service
    // through a OnceLock (set immediately below) — no reference cycle the
    // process needs to break.
    let subagents = Arc::new(oct_agent::subagent::SubagentManager::default());
    let exec_dir = std::env::var("OCT_EXEC_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            oct_agent::runtime::background::exec_output_root(&database_url)
        });
    let background = Arc::new(oct_agent::runtime::background::BackgroundTaskManager::new(
        pool.clone(),
        exec_dir,
    ));
    let runs = Arc::new(oct_agent::runtime::RunService {
        pool: pool.clone(),
        registry: registry.clone(),
        mcp: mcp.clone(),
        sessions: sessions.clone(),
        file_locks: dashmap::DashMap::new(),
        subagents: subagents.clone(),
        background: background.clone(),
        approvals: Arc::new(oct_agent::permissions::registry::ApprovalRegistry::new()),
        gates: Arc::new(dashmap::DashMap::new()),
    });
    subagents.set_service(runs.clone());
    background.set_service(runs.clone());

    let state = api::AppState {
        runs,
        pool,
        registry,
        sessions,
        mcp,
        oauth_flows: Arc::new(oct_agent::oauth::OAuthFlowStore::default()),
    };

    let app = api::build_router(state).layer(CorsLayer::permissive());

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}")).await?;
    info!("Server listening on http://0.0.0.0:{port}");
    info!("OpenAPI docs at http://0.0.0.0:{port}/scalar");

    axum::serve(listener, app).await?;

    Ok(())
}
