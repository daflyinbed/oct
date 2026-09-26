pub mod background;
pub mod conversations;
pub mod mcp;
pub mod mcp_tokens;
pub mod messages;
pub mod models;
pub mod permissions;
pub mod projects;
pub mod providers;
pub mod runs;

use anyhow::Result;
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use std::{str::FromStr, time::Duration};

pub async fn init_pool(database_url: &str) -> Result<SqlitePool> {
    let options = SqliteConnectOptions::from_str(database_url)?
        .create_if_missing(true)
        .foreign_keys(true)
        .journal_mode(SqliteJournalMode::Wal)
        .busy_timeout(Duration::from_secs(5));

    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await?;

    sqlx::migrate!("../../migrations").run(&pool).await?;

    Ok(pool)
}

/// Whether a database error is SQLite BUSY (code 5). A deferred transaction
/// that reads and then writes (e.g. `SELECT MAX(ordering)` followed by an
/// UPDATE) can hit BUSY as an immediate snapshot-upgrade conflict when
/// another writer committed since the read began — a case `busy_timeout`
/// does NOT retry. Root and subagent loops now write to one database
/// concurrently, so callers with that read-then-write shape retry on it
/// (see `messages::insert_message`).
pub fn is_busy(e: &sqlx::Error) -> bool {
    e.as_database_error()
        .and_then(|db| db.code())
        .is_some_and(|code| matches!(code.as_ref(), "5" | "517"))
}
