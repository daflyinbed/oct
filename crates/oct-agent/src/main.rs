mod agent;
mod api;
mod db;
mod seed;
mod tools;

use anyhow::Result;
use clap::Parser;
use std::sync::Arc;
use tower_http::cors::CorsLayer;
use tracing::info;

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

    let state = api::AppState {
        pool,
        registry,
        sessions: Arc::new(dashmap::DashMap::new()),
    };

    let app = api::build_router(state).layer(CorsLayer::permissive());

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}")).await?;
    info!("Server listening on http://0.0.0.0:{port}");
    info!("OpenAPI docs at http://0.0.0.0:{port}/scalar");

    axum::serve(listener, app).await?;

    Ok(())
}
