//! Catalog maintenance for connected entries: the connect-time best-effort
//! prompts/resources fetch, and the `tools/list_changed` debounce/refresh
//! loop that keeps a connected entry's tool list fresh while the server
//! keeps running.
//!
//! A refresh task belongs to exactly one entry generation — remove or
//! replace the entry and the old task retires on its next write, so it can
//! never corrupt a newer entry.

use std::sync::{Arc, Weak};
use std::time::Duration;

use dashmap::DashMap;
use tracing::{info, warn};

use oct_mcp::{McpConnection, ToolListChanged};

use super::connect::CONNECT_TIMEOUT;
use super::manager::{ManagedServer, ServerStatus};

/// Debounce window a `tools/list_changed` refresh task holds open before
/// re-listing: a server often announces several changes in a burst (and
/// restarts announce on reconnect), which must collapse into ONE
/// `tools/list` round trip. Short on purpose — the catalog refresh is
/// best-effort latency, never a correctness window.
pub(crate) const TOOL_LIST_REFRESH_DEBOUNCE: Duration = Duration::from_millis(500);

/// Spawn the `tools/list_changed` refresh task for one freshly inserted
/// entry incarnation. Connections without a notification feed (default
/// trait impl, e.g. minimal test doubles) spawn nothing. The task weakly
/// references the map and connection so idle watching cannot retain retired
/// transports or the manager. It terminates when the feed closes (production
/// connections close it on shutdown), or at the first write that finds its
/// entry removed or replaced
/// (generation guard). See [`tool_list_refresh_loop`].
pub(crate) fn spawn_tool_list_refresh_task(
    servers: Arc<DashMap<String, ManagedServer>>,
    id: String,
    generation: u64,
    connection: Arc<dyn McpConnection>,
) {
    let Some(watcher) = connection.tool_list_changed() else {
        return;
    };
    tokio::spawn(tool_list_refresh_loop(
        watcher,
        Arc::downgrade(&servers),
        id,
        generation,
        Arc::downgrade(&connection),
    ));
}

/// Debounce + refresh loop behind every connected entry: on each
/// `tools/list_changed` notification, hold [`TOOL_LIST_REFRESH_DEBOUNCE`]
/// open (notifications landing inside collapse into this cycle), then
/// re-list under the connect-time budget and write the new catalog into the
/// entry — guarded by the generation the task was spawned with.
///
/// Failure policy: a failed or timed-out refresh is logged and the entry is
/// left exactly as it was (the server may be mid-restart; reconcile remains
/// the authority on reconnects). The loop never removes an entry.
async fn tool_list_refresh_loop(
    mut watcher: ToolListChanged,
    servers: Weak<DashMap<String, ManagedServer>>,
    id: String,
    generation: u64,
    connection: Weak<dyn McpConnection>,
) {
    loop {
        // Park until the server announces a catalog change (or the feed
        // closes for good — a production connection's feed closes once its
        // entry is disconnected and the service dropped).
        if !watcher.changed().await {
            return;
        }
        // Debounce: a FIXED window from the first notification; every
        // notification landing inside it is absorbed here (its `changed()`
        // resolution marks the revision seen) and served by the single
        // re-list below. A notification after the window naturally starts a
        // fresh cycle. `changed()` is cancel-safe in select.
        let deadline = tokio::time::Instant::now() + TOOL_LIST_REFRESH_DEBOUNCE;
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => break,
                alive = watcher.changed() => {
                    if !alive {
                        return;
                    }
                }
            }
        }

        // Same budget the connect-time listing uses, so a server that lists
        // forever cannot pin the task.
        let (Some(servers), Some(connection)) = (servers.upgrade(), connection.upgrade()) else {
            return;
        };
        let refreshed = tokio::time::timeout(CONNECT_TIMEOUT, connection.list_tools()).await;
        match refreshed {
            Ok(Ok(tools)) => {
                // Generation check and write happen under one DashMap shard
                // lock (the RefMut guard), so a concurrent remove/replace of
                // the entry cannot interleave between them.
                let Some(mut entry) = servers.get_mut(&id) else {
                    // Entry removed (disconnect/reconcile): retire.
                    return;
                };
                if entry.generation != generation {
                    // Entry replaced by a reconnect: retire, never write
                    // into the new incarnation.
                    return;
                }
                let tool_count = tools.len();
                entry.tools = tools;
                entry.status = ServerStatus::Connected { tool_count };
                info!(
                    server = %entry.name,
                    tools = tool_count,
                    "MCP tool catalog refreshed after tools/list_changed"
                );
            }
            Ok(Err(e)) => {
                warn!(server = %id, "MCP tools/list refresh failed: {e:#}");
            }
            Err(_) => {
                warn!(
                    server = %id,
                    "MCP tools/list refresh timed out after {CONNECT_TIMEOUT:?}"
                );
            }
        }
    }
}

/// Best-effort connect-time catalog fetch for prompts/resources: bounded by
/// [`CONNECT_TIMEOUT`], and any failure (unsupported capability, transport
/// error, timeout) is logged and turned into an EMPTY catalog — it must never
/// fail the connection itself.
pub(crate) async fn fetch_catalog<T>(
    what: &'static str,
    fetch: impl std::future::Future<Output = anyhow::Result<Vec<T>>>,
) -> Vec<T> {
    match tokio::time::timeout(CONNECT_TIMEOUT, fetch).await {
        Ok(Ok(entries)) => entries,
        Ok(Err(e)) => {
            // Most common shape: the server declares no such capability and
            // answers with a JSON-RPC error — debug, not warn, so logs stay
            // quiet for the tools-only majority.
            tracing::debug!("{what} failed at connect time (kept empty): {e:#}");
            Vec::new()
        }
        Err(_) => {
            warn!("{what} timed out after {CONNECT_TIMEOUT:?} (kept empty)");
            Vec::new()
        }
    }
}
