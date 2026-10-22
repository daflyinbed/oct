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
    let cli = Cli::parse();
    let database_url = oct_agent::db::resolve_database_url()?;

    // The pool comes up before the tracing subscriber: the wire-log sink's
    // initial on/off state is read from the `settings` table, so both must
    // exist by the time the subscriber (and its wire layer) is assembled.
    let pool = db::init_pool(&database_url).await?;

    // LLM wire log: one JSONL file per conversation under the db
    // directory's `llm-wire/`. `OCT_LLM_WIRE_DIR` relocates it;
    // `OCT_LLM_WIRE_LOG=off` pins it off for the whole process (stronger
    // than the settings toggle, which then reports `false` until restart).
    let db_dir = oct_agent::runtime::background::exec_output_root(&database_url)
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default();
    let wire_dir = std::env::var("OCT_LLM_WIRE_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| db_dir.join("llm-wire"));
    let stored = oct_agent::db::settings::get(&pool, oct_agent::db::settings::KEY_LLM_WIRE_LOG)
        .await
        .unwrap_or(None);
    let hard_off = std::env::var("OCT_LLM_WIRE_LOG")
        .is_ok_and(|value| value.trim().eq_ignore_ascii_case("off"));
    let wire_log = oct_llm_provider::wire::init(
        wire_dir,
        !hard_off && oct_agent::db::settings::llm_wire_log_enabled(stored.as_deref()),
    );
    if hard_off {
        wire_log.hard_disable();
    }

    // Layered subscriber: the human-facing stderr layer keeps its old
    // defaults (the wire target is not in them, so it stays quiet unless
    // RUST_LOG asks for it); the wire layer routes its own target into
    // the per-conversation files, gated by the runtime toggle.
    use tracing_subscriber::prelude::*;
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer().with_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "oct_agent=info,tower_http=debug".into()),
            ),
        )
        .with(oct_llm_provider::wire::WireLogLayer)
        .init();

    // The pool comes up before the subscriber, so the resolved location is
    // only logged here — the one line that answers "where did my data go".
    info!(%database_url, "database location");

    match cli {
        Cli::Serve { port } => run_server(pool, db_dir, wire_log, port).await,
        Cli::Seed => seed::run(&database_url).await,
    }
}

async fn run_server(
    pool: sqlx::SqlitePool,
    db_dir: std::path::PathBuf,
    wire_log: oct_llm_provider::wire::WireLogControl,
    port: u16,
) -> Result<()> {
    let registry = Arc::new(default_registry());
    let sessions: Arc<dashmap::DashMap<String, oct_agent::agent::RunHandle>> =
        Arc::new(dashmap::DashMap::new());
    let mcp = Arc::new(oct_agent::mcp::McpManager::default());

    // Crash recovery: agent_runs rows a previous process
    // left `running` become `lost` before this process accepts traffic.
    oct_agent::runtime::bootstrap(&pool).await?;

    // Two-phase wire: the RunService owns the SubagentManager and the
    // background task manager; each manager reaches back into the service
    // through a OnceLock (set immediately below) — no reference cycle the
    // process needs to break.
    let subagents = Arc::new(oct_agent::subagent::SubagentManager::default());
    let exec_dir = std::env::var("OCT_EXEC_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| db_dir.join("exec"));
    let background = Arc::new(oct_agent::runtime::background::BackgroundTaskManager::new(
        pool.clone(),
        exec_dir,
    ));
    // Chat attachments: bytes under `{db_dir}/attachments/`, staged rows in
    // the `attachments` table. `OCT_ATTACHMENT_DIR` relocates the tree; the
    // startup sweep reclaims staged-but-never-sent uploads (TTL) and files
    // no live message references anymore.
    let attachments_dir = std::env::var("OCT_ATTACHMENT_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| db_dir.join("attachments"));
    let attachments = Arc::new(oct_agent::attachments::AttachmentService::new(
        attachments_dir,
        pool.clone(),
    ));
    attachments.ensure_root().await?;
    attachments.sweep().await;
    let runs = Arc::new(oct_agent::runtime::RunService {
        lifecycle: Default::default(),
        pool: pool.clone(),
        registry: registry.clone(),
        mcp: mcp.clone(),
        sessions: sessions.clone(),
        file_locks: Arc::new(dashmap::DashMap::new()),
        subagents: subagents.clone(),
        background: background.clone(),
        attachments: attachments.clone(),
        approvals: Arc::new(oct_agent::permissions::registry::ApprovalRegistry::new()),
        gates: Arc::new(dashmap::DashMap::new()),
        prompt_envs: Arc::new(dashmap::DashMap::new()),
        prompt_env_locks: Arc::new(dashmap::DashMap::new()),
    });
    subagents.set_service(runs.clone());
    background.set_service(runs.clone());
    background.resume_notifications();

    let state = api::AppState {
        runs,
        pool,
        registry,
        sessions,
        mcp,
        oauth_flows: Arc::new(oct_agent::oauth::OAuthFlowStore::default()),
        wire_log,
        terminals: Arc::new(oct_agent::terminal::TerminalManager::default()),
    };

    let app = api::build_router(state).layer(CorsLayer::permissive());

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}")).await?;
    info!("Server listening on http://0.0.0.0:{port}");
    info!("OpenAPI docs at http://0.0.0.0:{port}/scalar");
    if !api::embed::frontend_available() {
        tracing::warn!(
            "frontend bundle not embedded — build it with `pnpm --filter frontend build` \
             (release binaries embed packages/app/dist at compile time)"
        );
    }

    axum::serve(listener, app).await?;

    Ok(())
}
