mod agent;
mod api;
mod db;
mod tools;

use anyhow::Result;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use tower_http::cors::CorsLayer;
use tracing::info;

use oct_llm_provider::providers::default_registry;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "oct_agent=info,tower_http=debug".into()),
        )
        .init();

    let database_url =
        std::env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite:oct-agent.db".to_string());
    let port: u16 = std::env::var("OCT_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3000);
    let provider_spec = std::env::var("OCT_PROVIDER")
        .unwrap_or_else(|_| "anthropic:claude-sonnet-4-20250514".to_string());
    let working_dir = std::env::var("OCT_WORKING_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    let pool = db::init_pool(&database_url).await?;
    let registry = Arc::new(default_registry());

    let state = api::AppState {
        pool,
        registry,
        provider_spec: Arc::new(RwLock::new(provider_spec)),
        working_dir,
    };

    let app = api::build_router(state).layer(CorsLayer::permissive());

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}")).await?;
    info!("Server listening on http://0.0.0.0:{port}");
    info!("OpenAPI docs at http://0.0.0.0:{port}/scalar");

    axum::serve(listener, app).await?;

    Ok(())
}
