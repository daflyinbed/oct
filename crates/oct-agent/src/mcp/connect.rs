//! The connect pipeline for one MCP server row: transport selection (stdio
//! child process or Streamable HTTP), the initialize handshake, and the
//! three connect-time catalog lists.

use std::sync::Arc;
use std::time::Duration;

use sqlx::SqlitePool;

use oct_mcp::{
    HttpServerConfig, McpConnection, McpPromptInfo, McpResourceInfo, McpToolInfo,
    StdioServerConfig, connect_http, connect_stdio,
};

use crate::db::mcp::McpServer;

use super::catalog::fetch_catalog;
use super::credential;
use super::manager::ServerStatus;

/// Budget for spawning one MCP server and finishing the initialize
/// handshake (and, separately, for the follow-up `tools/list`).
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Connect-time capability catalogs of one server: the `tools/list` result
/// plus the best-effort `prompts/list` / `resources/list` results.
#[derive(Debug, Default)]
pub struct ServerCatalog {
    pub tools: Vec<McpToolInfo>,
    pub prompts: Vec<McpPromptInfo>,
    pub resources: Vec<McpResourceInfo>,
}

/// Connect one server row: start the transport (stdio child process or
/// Streamable HTTP) + handshake, then `tools/list`, each bounded by
/// [`CONNECT_TIMEOUT`]. Never returns an error — failures come back as a
/// `Failed` status with the error string for the API surface.
///
/// The http branch runs the OAuth credential path through
/// [`credential::authorize_headers`]: load the stored token row, refresh it
/// when near expiry, and merge `Authorization: Bearer <token>`.
pub(crate) async fn connect_server(
    pool: &SqlitePool,
    row: &McpServer,
) -> (ServerStatus, Option<Arc<dyn McpConnection>>, ServerCatalog) {
    // Resolve the transport-specific connection future; the handshake→list
    // pipeline below is shared. A row whose transport fields are incomplete
    // (the API validates this, but the DB can still be edited out-of-band)
    // fails fast with a clear error instead of a confusing spawn/connect one.
    let connect = match row.transport.as_str() {
        "stdio" => {
            let Some(command) = row.command.as_deref() else {
                return failed("stdio transport requires a command".to_string());
            };
            let cfg = StdioServerConfig {
                command: command.to_string(),
                args: row.args.as_deref().cloned().unwrap_or_default(),
                env: row.env.as_deref().cloned().unwrap_or_default(),
            };
            futures_util::future::Either::Left(async move {
                let connection = connect_stdio(&cfg, CONNECT_TIMEOUT).await?;
                Ok::<_, anyhow::Error>((connection, ()))
            })
        }
        "http" => {
            let Some(url) = row.url.as_deref().map(str::trim).filter(|u| !u.is_empty()) else {
                return failed("http transport requires a url".to_string());
            };
            let authorized = credential::authorize_headers(
                pool,
                &row.id,
                url,
                &row.name,
                row.headers.as_deref(),
            )
            .await;
            let had_token = authorized.had_token;
            let cfg = HttpServerConfig {
                url: url.to_string(),
                headers: authorized.headers,
            };
            futures_util::future::Either::Right(async move {
                let connection = match connect_http(&cfg, CONNECT_TIMEOUT).await {
                    Ok(connection) => connection,
                    Err(error) => {
                        // Tailor the failure when an OAuth token was in play
                        // and the rejection looks like an auth one: rmcp
                        // surfaces HTTP 401s in its error chains (status
                        // codes / WWW-Authenticate text), so a substring
                        // probe is the honest best-effort.
                        if had_token && credential::looks_like_auth_rejection(&format!("{error:#}"))
                        {
                            return Err(error.context(
                                "the stored OAuth token was rejected (401); it may be expired \
                                 or revoked — re-run OAuth login for this server",
                            ));
                        }
                        return Err(error);
                    }
                };
                Ok::<_, anyhow::Error>((connection, ()))
            })
        }
        other => {
            return failed(format!(
                "unsupported transport '{other}'; 'stdio' and 'http' are available"
            ));
        }
    };
    let attempt = async {
        let (connection, ()) = connect.await?;
        // connect_* bound only the handshake; bound tools/list too so a
        // server that lists forever cannot stall the chat turn.
        let tools = tokio::time::timeout(CONNECT_TIMEOUT, connection.list_tools())
            .await
            .map_err(|_| {
                anyhow::anyhow!("tools/list did not finish within {CONNECT_TIMEOUT:?}")
            })??;
        Ok::<_, anyhow::Error>((Arc::from(connection), tools))
    };
    match attempt.await {
        Ok((connection, tools)) => {
            let connection: Arc<dyn McpConnection> = connection;
            // Prompts/resources catalogs are read side-tracked: fetched on
            // the SAME connection right after tools, best-effort — a server
            // without the capability (or one that errors mid-fetch) connects
            // fine, its catalog is just empty. No list_changed refresh loop
            // exists for these; a reconnect re-fetches.
            let prompts = fetch_catalog("prompts/list", connection.list_prompts()).await;
            let resources = fetch_catalog("resources/list", connection.list_resources()).await;
            (
                ServerStatus::Connected {
                    tool_count: tools.len(),
                },
                Some(connection),
                ServerCatalog {
                    tools,
                    prompts,
                    resources,
                },
            )
        }
        Err(e) => failed(format!("{e:#}")),
    }
}

pub(crate) fn failed(
    error: String,
) -> (ServerStatus, Option<Arc<dyn McpConnection>>, ServerCatalog) {
    (
        ServerStatus::Failed { error },
        None,
        ServerCatalog::default(),
    )
}
