pub mod agent_settings;
pub mod attachments;
pub mod background;
pub mod conversations;
pub mod history;
pub mod mcp;
pub mod mcp_tokens;
pub mod messages;
pub mod models;
pub mod permissions;
pub mod projects;
pub mod providers;
pub mod runs;
pub mod settings;

use anyhow::{Context, Result};
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use std::{str::FromStr, time::Duration};

/// Resolve the database location for this process. Precedence:
///
/// 1. `DATABASE_URL` — explicit wins (dev exports, tests, deployments)
/// 2. `$XDG_DATA_HOME/oct/oct.db` (`~/.local/share/oct/oct.db` on Linux) —
///    the default never scatters files into whatever directory the binary
///    was launched from. This branch creates the data directory (0700) and
///    pre-creates the database file (0600): it stores provider API keys
///    and OAuth tokens.
///
/// The `exec/` and `llm-wire/` trees follow the db directory automatically
/// (see `runtime::background::exec_output_root` and `main.rs`).
pub fn resolve_database_url() -> Result<String> {
    if let Ok(url) = std::env::var("DATABASE_URL") {
        return Ok(url);
    }
    let dir = dirs::data_dir()
        .context("cannot resolve the default data directory; set DATABASE_URL to choose one")?
        .join("oct");
    ensure_private_dir(&dir)?;
    let db_path = dir.join("oct.db");
    ensure_private_file(&db_path)?;
    // sqlx parses sqlite URLs by trimming the scheme and splitting on '?'
    // — no Url::parse — so an absolute path pastes verbatim even when it
    // contains spaces or non-ASCII (a literal '?' in the path would still
    // be mistaken for the query separator).
    Ok(format!("sqlite://{}", db_path.display()))
}

#[cfg(unix)]
fn ensure_private_dir(dir: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(unix)]
fn ensure_private_file(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn ensure_private_dir(dir: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    Ok(())
}

#[cfg(not(unix))]
fn ensure_private_file(path: &std::path::Path) -> Result<()> {
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    Ok(())
}

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

/// Retain the owner and its result until a critical write commits. This is
/// for already-executed side effects/settlement, not arbitrary reads or user
/// requests. Closing the pool is the explicit shutdown escape hatch.
pub async fn persist_critical<T, F, Fut>(pool: &SqlitePool, mut write: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let mut delay = Duration::from_millis(50);
    loop {
        match write().await {
            Ok(value) => return Ok(value),
            Err(error) if pool.is_closed() => return Err(error),
            Err(error) => {
                tracing::error!(%error, "critical persistence pending; retaining execution ownership")
            }
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_secs(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn xdg_default_creates_private_tree() {
        let dir = std::env::temp_dir().join(format!("oct-db-paths-{}", uuid::Uuid::new_v4()));
        ensure_private_dir(&dir).unwrap();
        let db_path = dir.join("oct.db");
        std::fs::write(&db_path, "existing data").unwrap();
        ensure_private_file(&db_path).unwrap();

        use std::os::unix::fs::PermissionsExt;
        let dir_mode = std::fs::metadata(&dir).unwrap().permissions().mode();
        let file_mode = std::fs::metadata(&db_path).unwrap().permissions().mode();
        assert_eq!(dir_mode & 0o777, 0o700);
        assert_eq!(file_mode & 0o777, 0o600);
        // Opening for permission hardening must never clobber the database.
        assert_eq!(std::fs::read(&db_path).unwrap(), b"existing data");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
