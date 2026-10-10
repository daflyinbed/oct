//! The process-wide MCP connection manager: registry, reconcile, and the
//! catalog-backed read paths (prompts/resources).
//!
//! [`McpManager`] owns the live connections (keyed by `mcp_servers.id`) and
//! reconciles them against the enabled DB rows on every chat turn
//! ([`McpManager::ensure_connections`]): rows gone from the DB (or disabled)
//! are disconnected, never-connected rows are connected concurrently, and a
//! config change (fingerprint mismatch) forces a reconnect. Everything is
//! best-effort — a server that cannot be spawned is recorded as `Failed`
//! with its error and never blocks the chat request that triggered it.
//!
//! Connections are process-wide and shared across projects: the connection
//! key is the server id, so the same connection serves any conversation of
//! its project. Scoping (`mcp_servers.project_id`: NULL = global, non-NULL =
//! that project only) affects exactly two things — which project's turn
//! triggers a (re)connect, and which project's run receives the tools
//! ([`McpManager::tools_for_run`] filters by the entry's scope).
//!
//! Reconcile concurrency: the decision phase runs under a short
//! [`decide_lock`](McpManager::decide_lock) with no network awaits, and
//! every connect/disconnect takes only ITS server's gate — one project's
//! slow server never queues another project's reconcile. Concurrent
//! reconciles that decide the same server needs connecting converge on the
//! gate: the first runs the handshake, the others re-check afterwards and
//! find nothing left to do. OAuth invalidation bumps the server's
//! [`epoch`](McpManager::epochs) instead of waiting out an in-flight
//! handshake; a connect commits only while its captured epoch still
//! matches, so a stale-credential result can never re-enter the map.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, Weak};
use std::time::Duration;

use dashmap::DashMap;
use serde_json::Value;
use sqlx::SqlitePool;
use tracing::{info, warn};

use oct_mcp::ToolNameMap;
use oct_mcp::{
    McpConnection, McpPromptInfo, McpPromptResult, McpResourceContent, McpResourceInfo, McpToolInfo,
};

use crate::db;
use crate::db::mcp::McpServer;
use crate::tools::AgentTool;

use super::catalog::spawn_tool_list_refresh_task;
use super::connect::{ServerCatalog, connect_server};
use super::tool::McpTool;
use super::{credential, health::AuthAwareConnection};

/// Per-call timeout for MCP tool calls when the server row carries no
/// `tool_timeout_ms`.
pub(crate) const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_secs(60);

/// Live state of one managed server, also surfaced to the API layer through
/// [`McpManager::status_snapshot`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerStatus {
    Connected { tool_count: usize },
    Failed { error: String },
}

/// One row of [`McpManager::status_snapshot`].
#[derive(Debug, Clone)]
pub struct ServerStatusEntry {
    pub server_id: String,
    pub name: String,
    pub status: ServerStatus,
}

/// Outcome of a catalog-backed server call ([`McpManager::render_prompt`] /
/// [`McpManager::read_resource`]).
///
/// `NotConnected` covers every "no live server to ask" shape (no entry,
/// `Failed` entry, or the requested name/URI is absent from the cached
/// catalog) and maps to 404 at the API layer. `Failed` carries the
/// server-side error string (transport error, server refusal, timeout) and
/// maps to 502 — the request really reached a live server and it errored.
#[derive(Debug, Clone, PartialEq)]
pub enum McpServerCall<T> {
    Ok(T),
    NotConnected,
    Failed(String),
}

/// Cloned-out snapshot of one connected entry for the render/read call paths
/// (no DashMap guard crosses an await).
struct ConnectedEntry {
    connection: Arc<dyn McpConnection>,
    prompts: Vec<McpPromptInfo>,
    resources: Vec<McpResourceInfo>,
    tool_timeout: Duration,
}

/// What the manager tracks for one MCP server id.
pub(crate) struct ManagedServer {
    /// User-facing name from the DB row (UNIQUE); drives the exposed
    /// `mcp__{name}__{tool}` tool names.
    pub(crate) name: String,
    pub(crate) status: ServerStatus,
    /// Identity of this entry incarnation, allocated from the manager's
    /// monotonic counter on every insert. The `tools/list_changed` refresh
    /// task spawned with an entry may only write while the live entry's
    /// generation still matches — removing or replacing the entry retires
    /// the old task on its next write instead of letting it corrupt a newer
    /// entry. `0` only ever appears in hand-built test literals, which no
    /// spawned task matches.
    pub(crate) generation: u64,
    /// Fingerprint of the config this entry was established under.
    /// `None` marks a test-injected entry (see
    /// [`McpManager::insert_connection_for_test`]): reconcile treats it as
    /// externally owned and never reconnects or removes it.
    pub(crate) fingerprint: Option<String>,
    /// Project scope this entry was established under (`None` = global,
    /// `Some(id)` = that project only). Copied from the row — and part of
    /// its fingerprint, so a scope edit refreshes it on the next reconcile —
    /// letting [`McpManager::tools_for_run`] filter without a DB read.
    pub(crate) scope: Option<String>,
    /// The live connection; `None` exactly when status is `Failed`.
    pub(crate) connection: Option<Arc<dyn McpConnection>>,
    /// Shared with the connection adapter; a 401 retires this incarnation.
    pub(crate) auth_rejected: Arc<AtomicBool>,
    /// Cached `tools/list` result (only meaningful while `Connected`),
    /// refreshed in place by the entry's `tools/list_changed` task.
    pub(crate) tools: Vec<McpToolInfo>,
    /// Cached `prompts/list` result. Unlike `tools`, fetched ONCE at connect
    /// time — no `prompts/list_changed` refresh task exists (the UI treats
    /// the prompt catalog as connect-time metadata; a config edit reconnects
    /// and re-fetches anyway).
    pub(crate) prompts: Vec<McpPromptInfo>,
    /// Cached `resources/list` result, same connect-time-once contract as
    /// `prompts`.
    pub(crate) resources: Vec<McpResourceInfo>,
    /// Per-call budget from the row's `tool_timeout_ms` (or the default).
    pub(crate) tool_timeout: Duration,
}

/// The connect pipeline's signature (owned args so the returned future is
/// `'static` and boxable).
pub(crate) type ConnectOutcome = (ServerStatus, Option<Arc<dyn McpConnection>>, ServerCatalog);
type Connector = dyn Fn(sqlx::SqlitePool, McpServer) -> futures_util::future::BoxFuture<'static, ConnectOutcome>
    + Send
    + Sync;

/// Owns the process-wide MCP connections. Shared through `AppState` as an
/// `Arc`; cheap to construct, so tests build their own per case.
pub struct McpManager {
    /// key = `mcp_servers.id`. Per-entry refresh tasks hold a Weak of this map.
    servers: Arc<DashMap<String, ManagedServer>>,
    /// Borrowed HTTP incarnations retired by automatic credential refresh.
    /// Weak references allow final-lease cleanup, while explicit logout or
    /// server edits can still shut down every incarnation immediately.
    retired: DashMap<String, Vec<Weak<dyn McpConnection>>>,
    /// Monotonic source of [`ManagedServer::generation`] values.
    generation: AtomicU64,
    /// Per-server connect gates. Serializes the connects/removals of ONE
    /// server id only — never held across a network await by more than one
    /// server, so independent servers (and independent projects' reconciles)
    /// progress in parallel.
    gates: DashMap<String, Arc<tokio::sync::Mutex<()>>>,
    /// Invalidation epochs, bumped by [`McpManager::invalidate`] (the
    /// non-blocking removal path). A connect captures the epoch when it
    /// starts its handshake and commits only while the epoch still matches:
    /// an OAuth invalidation that lands mid-handshake must not be overwritten
    /// by the stale-credential result arriving late.
    epochs: Arc<DashMap<String, u64>>,
    /// Serializes the DECISION phase of reconciles only (map reads and plan
    /// computation — no network awaits). Concurrent chat turns queue here
    /// briefly, then act per-server in parallel.
    decide_lock: tokio::sync::Mutex<()>,
    /// The connect pipeline under test replacement (see
    /// [`Self::set_connector_for_test`]); production runs
    /// [`super::connect::connect_server`].
    connector: std::sync::RwLock<Arc<Connector>>,
}

impl Default for McpManager {
    fn default() -> Self {
        Self {
            servers: Arc::new(DashMap::new()),
            retired: DashMap::new(),
            generation: AtomicU64::new(0),
            gates: DashMap::new(),
            epochs: Arc::new(DashMap::new()),
            decide_lock: tokio::sync::Mutex::new(()),
            connector: std::sync::RwLock::new(Arc::new(
                |pool: sqlx::SqlitePool, row: McpServer| {
                    Box::pin(async move { connect_server(&pool, &row).await })
                },
            )),
        }
    }
}

impl McpManager {
    /// Wait for shared reconciliation until THIS caller cancels. The owned
    /// reconciliation continues: aborting a waiter must neither tear down a
    /// sibling's handshake nor lose a rotated refresh token before storage.
    /// Returns false when cancelled, true when reconciliation has finished.
    pub async fn ensure_connections_cancelled(
        self: &Arc<Self>,
        pool: &SqlitePool,
        project_id: Option<&str>,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> bool {
        if cancel.is_cancelled() {
            return false;
        }
        let manager = self.clone();
        let pool = pool.clone();
        let project = project_id.map(str::to_owned);
        let reconciliation = tokio::spawn(async move {
            manager.ensure_connections(&pool, project.as_deref()).await;
        });
        tokio::select! {
            biased;
            _ = cancel.cancelled() => false,
            outcome = reconciliation => {
                if let Err(error) = outcome { warn!("MCP reconciliation task failed: {error}"); }
                true
            }
        }
    }

    /// Reconcile the live connections against the enabled `mcp_servers`
    /// rows, from the perspective of `project_id` (the project whose turn is
    /// about to run). Best-effort by contract: connection failures (and even
    /// a DB read failure) are recorded/logged, never propagated — a broken
    /// MCP server must not break the chat turn that triggered the reconcile.
    ///
    /// Entries are process-wide and shared across projects, so removals look
    /// at every enabled row regardless of scope; only (re)connect decisions
    /// are scoped. Semantics per enabled row ("in scope" = global row, or
    /// row scoped to `project_id`):
    ///
    /// - in scope, no entry / a `Failed` entry / an entry whose fingerprint
    ///   no longer matches the row (config edit) → (re)connect, concurrently
    ///   with the other pending servers;
    /// - out of scope with a stale-fingerprint entry → the entry is removed
    ///   (and its connection shut down) but NOT reconnected here — the
    ///   project that owns the row connects it again on its own next turn;
    /// - any scope, a healthy `Connected` entry with a current fingerprint
    ///   and credential → untouched, even when the current project cannot
    ///   use it (another project's run may still hold its connection).
    ///
    /// Entries (test-injected excepted) whose row disappeared from the
    /// enabled set — deleted or disabled, in any project — are disconnected
    /// and removed.
    pub async fn ensure_connections(&self, pool: &SqlitePool, project_id: Option<&str>) {
        let rows = match db::mcp::list_mcp_servers(pool).await {
            Ok(rows) => rows,
            Err(e) => {
                warn!("MCP reconcile skipped: cannot read mcp_servers: {e:#}");
                return;
            }
        };
        // Read outside decide_lock. Connected HTTP transports retain a
        // static bearer, so an expiry must trigger a fresh assembly too.
        let tokens = db::mcp_tokens::list_token_summaries(pool)
            .await
            .unwrap_or_else(|error| {
                warn!("MCP token freshness check skipped: {error:#}");
                HashMap::new()
            });

        // Phase 1 — decide under the short lock: read the map, plan removals
        // and connects. No network await happens here, so concurrent
        // reconciles queue for microseconds, not for each other's slowest
        // server handshake.
        let (gone, out_of_scope_stale, to_connect) = {
            let _decided = self.decide_lock.lock().await;

            // ALL enabled rows, across every project: entries are
            // process-wide, so removal decisions must consider rows the
            // current project cannot see (a disabled scoped server in
            // another project still has to go).
            let desired: HashMap<String, &McpServer> = rows
                .iter()
                .filter(|row| row.enabled)
                .map(|row| (row.id.clone(), row))
                .collect();

            // Entries whose row disappeared (deleted/disabled): remove
            // unconditionally (fingerprint irrelevant — the row is gone).
            let gone: Vec<String> = self
                .servers
                .iter()
                .filter(|entry| {
                    entry.value().fingerprint.is_some() && !desired.contains_key(entry.key())
                })
                .map(|entry| entry.key().clone())
                .collect();

            // Stale-fingerprint entries of OUT-of-scope rows: remove only
            // when the entry still shows the old fingerprint (a concurrent
            // in-scope reconnect may already have installed a fresh one),
            // and never reconnect here.
            let mut out_of_scope_stale: Vec<(String, String)> = Vec::new();
            let to_connect: Vec<McpServer> = desired
                .into_iter()
                .filter(|(id, row)| {
                    if scope_hits(row.project_id.as_deref(), project_id) {
                        self.entry_needs_connect(id, row)
                            || (row.transport == "http"
                                && self
                                    .servers
                                    .get(id)
                                    .is_some_and(|entry| entry.fingerprint.is_some())
                                && tokens.get(id).is_some_and(|token| {
                                    token.has_refresh_token
                                        && credential::token_expiring(token.expires_at)
                                }))
                    } else if let Some(seen) = self.stale_fingerprint(id, row) {
                        out_of_scope_stale.push((id.clone(), seen));
                        false
                    } else {
                        false
                    }
                })
                .map(|(_, row)| row.clone())
                .collect();
            (gone, out_of_scope_stale, to_connect)
        };

        // Phase 2 — act per server, outside the decide lock. Each action
        // takes only its own server's gate; different servers (and different
        // projects' reconciles) run in parallel.
        let mut actions: Vec<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>> =
            Vec::with_capacity(gone.len() + out_of_scope_stale.len() + to_connect.len());
        for id in &gone {
            actions.push(Box::pin(self.remove_entry(id, None)));
        }
        for (id, seen) in &out_of_scope_stale {
            actions.push(Box::pin(self.remove_entry(id, Some(seen.as_str()))));
        }
        for row in &to_connect {
            actions.push(Box::pin(self.connect_one(pool, row)));
        }
        futures_util::future::join_all(actions).await;
    }

    /// The server's gate, creating it on first use.
    fn gate_for(&self, id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.gates
            .entry(id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .value()
            .clone()
    }

    /// Next value for the entry generation counter (starts at 1; 0 stays
    /// reserved for hand-built test literals no spawned task can match).
    fn next_generation(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Whether the entry for `id` must be (re)connected for the current row.
    /// Only meaningful for rows IN the reconciling project's scope (an
    /// out-of-scope row is never connected by this project's turn). The
    /// DashMap guard is confined to this function (no await inside), so
    /// the caller may freely mutate the map afterwards.
    fn entry_needs_connect(&self, id: &str, row: &McpServer) -> bool {
        let Some(entry) = self.servers.get(id) else {
            return true;
        };
        match entry.value().fingerprint.as_deref() {
            // Test-injected: externally owned, reconcile leaves it alone.
            None => false,
            Some(seen) => {
                seen != fingerprint_of(row)
                    || entry
                        .value()
                        .connection
                        .as_ref()
                        .is_some_and(|c| c.is_closed())
                    || matches!(entry.value().status, ServerStatus::Failed { .. })
            }
        }
    }

    /// When the entry for `id` was established under a config that no longer
    /// matches the row, the OLD fingerprint (the removal condition for
    /// out-of-scope rows); `None` when the entry is current (or absent, or
    /// test-injected — those never count as stale).
    fn stale_fingerprint(&self, id: &str, row: &McpServer) -> Option<String> {
        let entry = self.servers.get(id)?;
        match entry.value().fingerprint.as_deref() {
            Some(seen) if seen != fingerprint_of(row) => Some(seen.to_string()),
            _ => None,
        }
    }

    /// Remove the entry under the server's gate (so it serializes with
    /// connects) and shut its connection down. `only_if_fingerprint`
    /// restricts the removal to entries still showing that (stale)
    /// fingerprint — while we waited for the gate, a concurrent reconnect
    /// may legitimately have installed a fresh entry that must survive.
    async fn remove_entry(&self, id: &str, only_if_fingerprint: Option<&str>) {
        let gate = self.gate_for(id);
        let _guard = gate.lock().await;
        if let Some(seen) = only_if_fingerprint
            && !self
                .servers
                .get(id)
                .is_some_and(|e| e.value().fingerprint.as_deref() == Some(seen))
        {
            return;
        }
        let mut connections = self.take_retired(id);
        if let Some((_, mut managed)) = self.servers.remove(id) {
            if let Some(connection) = managed.connection.take() {
                connections.push(connection);
            }
        }
        futures_util::future::join_all(connections.iter().map(|connection| connection.shutdown()))
            .await;
    }

    fn take_retired(&self, id: &str) -> Vec<Arc<dyn McpConnection>> {
        self.retired
            .remove(id)
            .map_or_else(Vec::new, |(_, retired)| {
                retired
                    .into_iter()
                    .filter_map(|connection| connection.upgrade())
                    .collect()
            })
    }

    /// Force the next reconcile to rebuild this server's connection from
    /// scratch: drop the entry (and shut its connection down) without
    /// touching the DB row. The OAuth paths call this after the stored
    /// credential changed (login stored a new token / logout cleared it), so
    /// a possibly-connected entry never keeps serving on a stale token until
    /// some unrelated fingerprint edit happens to reconnect it. Cheaper and
    /// more direct than folding token state into the fingerprint: the token
    /// row changes exactly at those two API calls, and both of them already
    /// know the server id.
    ///
    /// Does NOT wait out an in-flight handshake (the API call must not hang
    /// behind a 15s connect): instead it bumps the server's invalidation
    /// epoch, and the in-flight connect drops its result when the epochs no
    /// longer match (see [`Self::connect_one`]). A connect that starts
    /// AFTER the bump is unaffected — it reads the fresh credential anyway.
    pub async fn invalidate(&self, server_id: &str) {
        let connections = {
            // Share the short commit lock with connect_one: invalidation
            // cannot land between its epoch check and insertion.
            let _commit = self.decide_lock.lock().await;
            *self.epochs.entry(server_id.to_string()).or_default() += 1;
            let mut connections = self.take_retired(server_id);
            if let Some((_, managed)) = self.servers.remove(server_id)
                && let Some(connection) = managed.connection
            {
                connections.push(connection);
            }
            connections
        };
        futures_util::future::join_all(connections.iter().map(|connection| connection.shutdown()))
            .await;
    }

    /// Connect one server under its gate. Concurrent reconciles converge
    /// here: the first taker runs the handshake; later takers wait for the
    /// gate, re-check, and find nothing left to do (ONE handshake per
    /// server, shared outcome). The epoch captured before re-reading the
    /// DB row is the commit ticket — an invalidation that bumped it
    /// mid-handshake makes the (stale-credential) result be dropped instead
    /// of committed.
    async fn connect_one(&self, pool: &SqlitePool, row: &McpServer) {
        let gate = self.gate_for(&row.id);
        let _guard = gate.lock().await;
        // Capture before re-reading the row: a CRUD commit followed by
        // invalidate must retire this attempt even when it races that read.
        let epoch = self
            .epochs
            .get(&row.id)
            .map(|e| *e.value())
            .unwrap_or_default();
        match db::mcp::get_mcp_server(pool, &row.id).await {
            Ok(Some(current))
                if current.enabled && fingerprint_of(&current) == fingerprint_of(row) => {}
            Ok(_) => return, // The queued reconcile plan belongs to an old row.
            Err(error) => {
                warn!(server = %row.id, "MCP connect skipped: cannot recheck server row: {error:#}");
                return;
            }
        }
        // Another reconcile committed a fresh entry while we waited: done.
        if !self.entry_needs_connect(&row.id, row) {
            // Another waiter may already have refreshed while this one was
            // queued. Re-read after taking the gate to share that result.
            let expiring = row.transport == "http"
                && db::mcp_tokens::token_summary(pool, &row.id)
                    .await
                    .is_ok_and(|token| {
                        token.has_refresh_token && credential::token_expiring(token.expires_at)
                    });
            if !expiring {
                return;
            }
        }
        // Credential-only HTTP replacement releases the registry's lease;
        // existing runs may still own tools on that transport. Its adapter
        // closes it when the last lease drops. Config edits and stdio
        // reconnects still shut down immediately, without delaying handshake.
        let (mut rejected, connections) = {
            // Retirement and explicit invalidation share the commit lock;
            // a logout cannot miss a just-removed, still-borrowed transport.
            let _commit = self.decide_lock.lock().await;
            if self
                .epochs
                .get(&row.id)
                .is_some_and(|current| *current != epoch)
            {
                return;
            }
            let mut rejected = false;
            let mut connections = Vec::new();
            if let Some((_, managed)) = self.servers.remove(&row.id) {
                rejected = managed.auth_rejected.load(Ordering::Acquire);
                let retire_when_released = row.transport == "http"
                    && managed.fingerprint.as_deref() == Some(fingerprint_of(row).as_str());
                if retire_when_released {
                    if let Some(connection) = managed.connection {
                        let mut retired = self.retired.entry(row.id.clone()).or_default();
                        retired.retain(|connection| connection.strong_count() > 0);
                        retired.push(Arc::downgrade(&connection));
                    }
                } else {
                    connections.extend(self.take_retired(&row.id));
                    connections.extend(managed.connection);
                }
            }
            (rejected, connections)
        };
        for connection in connections {
            tokio::spawn(async move { connection.shutdown().await });
        }

        if rejected && row.transport == "http" {
            rejected = !credential::refresh_after_rejection(pool, row).await;
        }

        // connect_server owns the whole http-token path: fetch the stored
        // credential row, refresh it when near expiry, merge the bearer
        // header (token wins).
        let connector = self
            .connector
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let (status, connection, catalog) = connector(pool.clone(), row.clone()).await;
        // Preserve rejection across a failed reconnect. A transient AS
        // outage must not strand a revoked, future-dated token until expiry.
        let auth_rejected = Arc::new(AtomicBool::new(matches!(
            &status, ServerStatus::Failed { error }
                if rejected || credential::looks_like_auth_rejection(error)
        )));
        let connection = connection.map(|connection| {
            if row.transport == "http" {
                Arc::new(AuthAwareConnection::new(connection, auth_rejected.clone()))
                    as Arc<dyn McpConnection>
            } else {
                connection
            }
        });
        match &status {
            ServerStatus::Connected { tool_count } => info!(
                server = %row.name,
                tools = tool_count,
                prompts = catalog.prompts.len(),
                resources = catalog.resources.len(),
                "MCP server connected"
            ),
            ServerStatus::Failed { error } => {
                warn!(server = %row.name, %error, "MCP server connection failed")
            }
        }
        // Commit ticket: an invalidation bumped the epoch while the
        // handshake ran — drop the result (shut the connection down) instead
        // of letting a stale entry re-enter the map.
        let commit = self.decide_lock.lock().await;
        let still_valid = self
            .epochs
            .get(&row.id)
            .map(|e| *e.value() == epoch)
            .unwrap_or(true);
        if !still_valid {
            drop(commit);
            if let Some(connection) = connection {
                connection.shutdown().await;
            }
            return;
        }
        let generation = self.next_generation();
        // The refresh task needs the connection Arc too; clone before it
        // moves into the entry.
        let refresh_connection = connection.clone();
        self.servers.insert(
            row.id.clone(),
            ManagedServer {
                name: row.name.clone(),
                status,
                generation,
                fingerprint: Some(fingerprint_of(row)),
                scope: row.project_id.clone(),
                connection,
                auth_rejected,
                tools: catalog.tools,
                prompts: catalog.prompts,
                resources: catalog.resources,
                tool_timeout: tool_timeout_of(row),
            },
        );
        drop(commit);
        if let Some(connection) = refresh_connection {
            spawn_tool_list_refresh_task(
                self.servers.clone(),
                row.id.clone(),
                generation,
                connection,
            );
        }
    }

    /// Tools of every connected server IN SCOPE for `project_id`'s runs:
    /// global entries (scope `NULL`) serve every project, project entries
    /// only their own. The effective set is thus the two-level "global +
    /// own project" merge of the DB rows, applied to the live entries.
    /// Exposed names are deduplicated across servers through a fresh
    /// [`ToolNameMap`] (insertion order is deterministic: servers sorted by
    /// name, tools in server order), so the same set of connections always
    /// yields the same exposed names.
    pub fn tools_for_run(&self, project_id: Option<&str>) -> Vec<Box<dyn AgentTool>> {
        let mut connected: Vec<_> = self
            .servers
            .iter()
            .filter(|entry| {
                matches!(entry.value().status, ServerStatus::Connected { .. })
                    && !entry.value().auth_rejected.load(Ordering::Acquire)
                    && scope_hits(entry.value().scope.as_deref(), project_id)
            })
            .map(|entry| {
                let managed = entry.value();
                (
                    entry.key().clone(),
                    managed.name.clone(),
                    managed
                        .connection
                        .clone()
                        .expect("Connected entries always hold a connection"),
                    managed.tools.clone(),
                    managed.tool_timeout,
                )
            })
            .collect();
        connected.sort_by(|a, b| a.1.cmp(&b.1));

        let mut names = ToolNameMap::new();
        let mut tools = Vec::new();
        for (server_id, server, connection, infos, timeout) in connected {
            for info in infos {
                let exposed = names.insert(&server, &info.name);
                tools.push(Box::new(
                    McpTool::new(exposed, info, server.clone(), connection.clone(), timeout)
                        .with_server_id(server_id.clone()),
                ) as Box<dyn AgentTool>);
            }
        }
        tools
    }

    /// Live status of every managed server, sorted by (name, id) for stable
    /// output. Reads only the in-memory map — no DB access.
    pub fn status_snapshot(&self) -> Vec<ServerStatusEntry> {
        let mut out: Vec<_> = self
            .servers
            .iter()
            .map(|entry| ServerStatusEntry {
                server_id: entry.key().clone(),
                name: entry.value().name.clone(),
                status: visible_status(entry.value()),
            })
            .collect();
        out.sort_by(|a, b| {
            a.name
                .cmp(&b.name)
                .then_with(|| a.server_id.cmp(&b.server_id))
        });
        out
    }

    /// Live status of one server by id, if the manager holds an entry.
    pub fn status_of(&self, server_id: &str) -> Option<ServerStatus> {
        self.servers
            .get(server_id)
            .map(|entry| visible_status(entry.value()))
    }

    /// Cached `tools/list` of one server while `Connected`; empty when the
    /// server is missing, failed, or not yet connected. Read by the API layer
    /// to surface the loaded tool names/descriptions to the UI.
    pub fn tools_of(&self, server_id: &str) -> Vec<McpToolInfo> {
        self.servers
            .get(server_id)
            .map(|entry| entry.value().tools.clone())
            .unwrap_or_default()
    }

    /// Cached `prompts/list` of one server while `Connected`; same empty-on-
    /// miss contract as [`Self::tools_of`]. Empty also covers a server that
    /// offers no prompts capability (or whose connect-time fetch failed).
    pub fn prompts_of(&self, server_id: &str) -> Vec<McpPromptInfo> {
        self.servers
            .get(server_id)
            .map(|entry| entry.value().prompts.clone())
            .unwrap_or_default()
    }

    /// Cached `resources/list` of one server while `Connected`; same
    /// contract as [`Self::prompts_of`].
    pub fn resources_of(&self, server_id: &str) -> Vec<McpResourceInfo> {
        self.servers
            .get(server_id)
            .map(|entry| entry.value().resources.clone())
            .unwrap_or_default()
    }

    /// Render one prompt of one server (`prompts/get`). `arguments` must be
    /// a JSON object (or `null`). The call runs under the entry's per-call
    /// timeout budget — the same one tool calls use. See
    /// [`McpServerCall`] for the outcome mapping; `NotConnected` additionally
    /// fires when the cached prompt catalog is non-empty but does not list
    /// `name` (an empty catalog falls through to the wire, since it may be an
    /// empty-by-fetch-failure rather than a genuine "no prompts").
    pub async fn render_prompt(
        &self,
        server_id: &str,
        name: &str,
        arguments: &Value,
    ) -> McpServerCall<McpPromptResult> {
        let Some(entry) = self.connected_entry(server_id) else {
            return McpServerCall::NotConnected;
        };
        if !entry.prompts.is_empty() && !entry.prompts.iter().any(|p| p.name == name) {
            return McpServerCall::NotConnected;
        }
        match tokio::time::timeout(
            entry.tool_timeout,
            entry.connection.get_prompt(name, arguments),
        )
        .await
        {
            Ok(Ok(result)) => McpServerCall::Ok(result),
            Ok(Err(e)) => McpServerCall::Failed(format!("{e:#}")),
            Err(_) => McpServerCall::Failed(format!(
                "prompts/get for `{name}` timed out after {:?}",
                entry.tool_timeout
            )),
        }
    }

    /// Read one resource of one server by URI (`resources/read`); same
    /// outcome mapping as [`Self::render_prompt`] (an unknown URI is
    /// `NotConnected` when the cached resource catalog is non-empty).
    pub async fn read_resource(
        &self,
        server_id: &str,
        uri: &str,
    ) -> McpServerCall<McpResourceContent> {
        let Some(entry) = self.connected_entry(server_id) else {
            return McpServerCall::NotConnected;
        };
        if !entry.resources.is_empty() && !entry.resources.iter().any(|r| r.uri == uri) {
            return McpServerCall::NotConnected;
        }
        match tokio::time::timeout(entry.tool_timeout, entry.connection.read_resource(uri)).await {
            Ok(Ok(content)) => McpServerCall::Ok(content),
            Ok(Err(e)) => McpServerCall::Failed(format!("{e:#}")),
            Err(_) => McpServerCall::Failed(format!(
                "resources/read for `{uri}` timed out after {:?}",
                entry.tool_timeout
            )),
        }
    }

    /// Connection + cached catalogs + per-call timeout of one CONNECTED
    /// entry. Cloned out of the map so no DashMap guard is held across the
    /// awaits of [`Self::render_prompt`] / [`Self::read_resource`].
    fn connected_entry(&self, server_id: &str) -> Option<ConnectedEntry> {
        let entry = self.servers.get(server_id)?;
        let managed = entry.value();
        if !matches!(managed.status, ServerStatus::Connected { .. })
            || managed.auth_rejected.load(Ordering::Acquire)
        {
            return None;
        }
        Some(ConnectedEntry {
            connection: managed.connection.clone()?,
            prompts: managed.prompts.clone(),
            resources: managed.resources.clone(),
            tool_timeout: managed.tool_timeout,
        })
    }

    /// Test seam (integration tests live outside the crate, hence `pub`):
    /// register a pre-established `connection` under `server_id` as a
    /// `Connected` server advertising `tools`, scoped to `scope` (`None` =
    /// global). The entry carries no config fingerprint, so
    /// [`Self::ensure_connections`] treats it as externally owned: it is
    /// never reconnected, and it survives reconciles even if the matching
    /// row is missing or disabled. Tool-call timeout is the default; build
    /// an [`McpTool`] directly to control it. Prompts/resources catalogs stay
    /// empty (hand-build a `ManagedServer` literal to exercise them in-crate;
    /// out-of-crate tests get them through the real connect path). A
    /// `tools/list_changed` refresh task is spawned exactly like for
    /// production entries — fire the connection's feed to drive it.
    /// Test seam for the reconcile-concurrency tests: replace the connect
    /// pipeline with a scripted one (gates, counters, canned outcomes).
    #[doc(hidden)]
    pub fn set_connector_for_test(
        &self,
        connector: impl Fn(
            sqlx::SqlitePool,
            McpServer,
        ) -> futures_util::future::BoxFuture<'static, ConnectOutcome>
        + Send
        + Sync
        + 'static,
    ) {
        *self
            .connector
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Arc::new(connector);
    }

    /// Test seam for the refresh-task retirement tests: the reconcile
    /// removal path (entry gone + connection shut down), callable from
    /// outside the module.
    #[doc(hidden)]
    pub async fn remove_entry_for_test(&self, server_id: &str) {
        self.remove_entry(server_id, None).await;
    }

    #[doc(hidden)]
    pub fn insert_connection_for_test(
        &self,
        server_id: &str,
        name: &str,
        scope: Option<&str>,
        connection: Arc<dyn McpConnection>,
        tools: Vec<McpToolInfo>,
    ) {
        let tool_count = tools.len();
        let generation = self.next_generation();
        self.servers.insert(
            server_id.to_string(),
            ManagedServer {
                name: name.to_string(),
                status: ServerStatus::Connected { tool_count },
                generation,
                fingerprint: None,
                scope: scope.map(str::to_string),
                connection: Some(connection.clone()),
                auth_rejected: Default::default(),
                tools,
                prompts: Vec::new(),
                resources: Vec::new(),
                tool_timeout: DEFAULT_TOOL_TIMEOUT,
            },
        );
        spawn_tool_list_refresh_task(
            self.servers.clone(),
            server_id.to_string(),
            generation,
            connection,
        );
    }
}

fn visible_status(server: &ManagedServer) -> ServerStatus {
    if server.auth_rejected.load(Ordering::Acquire)
        && matches!(server.status, ServerStatus::Connected { .. })
    {
        ServerStatus::Failed {
            error: "MCP authorization rejected; the next assembly will refresh the credential"
                .into(),
        }
    } else {
        server.status.clone()
    }
}

/// Fingerprint of every launch- and scope-relevant field of a row. Any edit
/// that changes how the server is started or addressed must produce a
/// different value, forcing a reconnect on the next reconcile;
/// `project_id` is included so a scope edit refreshes the entry's scope view
/// too. `updated_at` would be simpler but loses precision on rapid edits and
/// fires on irrelevant schema changes; the semantic fields are exact.
pub(crate) fn fingerprint_of(server: &McpServer) -> String {
    format!(
        "{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
        server.name,
        server.transport,
        server.command,
        server.args,
        server.env,
        server.url,
        server.headers,
        server.tool_timeout_ms,
        server.project_id,
    )
}

/// Whether something established under `scope` (a DB row's `project_id`, or
/// a live entry's copy of it) serves `project_id`'s runs: `None` is global
/// and serves everyone; `Some(id)` serves only that project — and a
/// scope-less query (`project_id` `None`, no project context) sees only
/// global entries.
pub(crate) fn scope_hits(scope: Option<&str>, project_id: Option<&str>) -> bool {
    match scope {
        None => true,
        Some(scope) => Some(scope) == project_id,
    }
}

pub(crate) fn tool_timeout_of(server: &McpServer) -> Duration {
    server
        .tool_timeout_ms
        .filter(|ms| *ms > 0)
        .map(|ms| Duration::from_millis(ms as u64))
        .unwrap_or(DEFAULT_TOOL_TIMEOUT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;

    use async_trait::async_trait;
    use oct_mcp::{McpCallResult, ToolListChanged};
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    use crate::mcp::catalog::TOOL_LIST_REFRESH_DEBOUNCE;
    use crate::mcp::tool::MAX_DESCRIPTION_CHARS;
    use crate::mcp::tool::MAX_IMAGE_BASE64_CHARS;
    use crate::mcp::tool::MAX_RESULT_IMAGES;
    use crate::mcp::tool::McpTool;
    use crate::mcp::tool::NO_CONTENT;
    use crate::mcp::tool::NO_DESCRIPTION;
    use crate::mcp::tool::tool_description;
    use crate::tools::truncate::MAX_TOOL_OUTPUT_BYTES;
    use crate::tools::{ToolContext, ToolImage, ToolOutput};

    /// Minimal scripted connection for unit tests (the richer
    /// tests/support version is not visible from inside the crate).
    struct FakeConn {
        result: McpCallResult,
    }

    impl FakeConn {
        fn ok(text: &str) -> Arc<Self> {
            Arc::new(Self {
                result: McpCallResult {
                    text: text.to_string(),
                    structured_json: None,
                    is_error: false,
                    images: Vec::new(),
                },
            })
        }
    }

    #[async_trait]
    impl McpConnection for FakeConn {
        async fn list_tools(&self) -> anyhow::Result<Vec<McpToolInfo>> {
            Ok(Vec::new())
        }

        async fn call_tool(
            &self,
            _name: &str,
            _arguments: &Value,
        ) -> anyhow::Result<McpCallResult> {
            Ok(self.result.clone())
        }

        async fn shutdown(&self) {}
    }

    fn info(name: &str, description: Option<&str>) -> McpToolInfo {
        McpToolInfo {
            output_schema: None,
            name: name.to_string(),
            description: description.map(str::to_string),
            input_schema: json!({"type": "object", "properties": {}}),
        }
    }

    fn connected_manager() -> McpManager {
        let manager = McpManager::default();
        manager.insert_connection_for_test(
            "id-1",
            "one",
            None,
            FakeConn::ok("x"),
            vec![info("a", None)],
        );
        manager
    }

    // ------------------------------------------------------------------
    // tools_for_run: naming, dedup, cross-server exposure
    // ------------------------------------------------------------------

    #[test]
    fn tools_for_run_exposes_qualified_names() {
        let manager = connected_manager();
        let tools = manager.tools_for_run(None);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name(), "mcp__one__a");
        assert_eq!(
            tools[0].description(),
            format!("[mcp:one] {NO_DESCRIPTION}")
        );
        assert_eq!(tools[0].title(&json!({})), "one / a");
        assert_eq!(
            tools[0].input_schema(),
            json!({"type": "object", "properties": {}})
        );
    }

    #[test]
    fn tools_for_run_dedupes_cross_server_name_collisions() {
        // "a-b" and "a.b" sanitize to the same exposed name; sorted by name,
        // "a-b" is registered first and keeps the plain name while "a.b"
        // gets the hash-suffixed variant.
        let manager = McpManager::default();
        manager.insert_connection_for_test(
            "id-1",
            "a-b",
            None,
            FakeConn::ok("x"),
            vec![info("c", None)],
        );
        manager.insert_connection_for_test(
            "id-2",
            "a.b",
            None,
            FakeConn::ok("y"),
            vec![info("c", None)],
        );
        let tools = manager.tools_for_run(None);
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name(), "mcp__a-b__c");
        assert_ne!(tools[0].name(), tools[1].name());
        assert!(tools[1].name().starts_with("mcp__a-b__c"));
        // Both still dispatch to their own connection: titles tell them
        // apart by server name.
        let titles: Vec<String> = tools.iter().map(|t| t.title(&json!({}))).collect();
        assert!(titles.contains(&"a-b / c".to_string()));
        assert!(titles.contains(&"a.b / c".to_string()));
    }

    #[test]
    fn tools_for_run_skips_failed_servers() {
        let manager = connected_manager();
        manager.servers.insert(
            "id-2".to_string(),
            ManagedServer {
                name: "broken".to_string(),
                status: ServerStatus::Failed {
                    error: "spawn failed".to_string(),
                },
                generation: 0,
                fingerprint: Some("fp".to_string()),
                scope: None,
                connection: None,
                auth_rejected: Default::default(),
                tools: vec![info("dead", None)],
                prompts: Vec::new(),
                resources: Vec::new(),
                tool_timeout: DEFAULT_TOOL_TIMEOUT,
            },
        );
        let tools = manager.tools_for_run(None);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name(), "mcp__one__a");
    }

    #[test]
    fn tools_for_run_filters_entries_by_project_scope() {
        // Global + own project (two-level merge); a scope-less query (no
        // project context) only sees global entries.
        let manager = McpManager::default();
        manager.insert_connection_for_test(
            "id-g",
            "global",
            None,
            FakeConn::ok("x"),
            vec![info("shared", None)],
        );
        manager.insert_connection_for_test(
            "id-a",
            "alpha",
            Some("proj-a"),
            FakeConn::ok("x"),
            vec![info("only_a", None)],
        );
        manager.insert_connection_for_test(
            "id-b",
            "beta",
            Some("proj-b"),
            FakeConn::ok("x"),
            vec![info("only_b", None)],
        );

        let names = |project_id: Option<&str>| {
            manager
                .tools_for_run(project_id)
                .iter()
                .map(|t| t.name().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(None), vec!["mcp__global__shared"]);
        assert_eq!(
            names(Some("proj-a")),
            vec!["mcp__alpha__only_a", "mcp__global__shared"]
        );
        assert_eq!(
            names(Some("proj-b")),
            vec!["mcp__beta__only_b", "mcp__global__shared"]
        );
    }

    #[test]
    fn status_snapshot_lists_managed_servers_sorted() {
        let manager = connected_manager();
        manager.insert_connection_for_test("id-2", "two", None, FakeConn::ok("x"), vec![]);
        let snapshot = manager.status_snapshot();
        let names: Vec<&str> = snapshot.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["one", "two"]);
        assert_eq!(
            snapshot[0].status,
            ServerStatus::Connected { tool_count: 1 }
        );
        assert_eq!(
            snapshot[1].status,
            ServerStatus::Connected { tool_count: 0 }
        );
        assert_eq!(
            manager.status_of("id-1"),
            Some(ServerStatus::Connected { tool_count: 1 })
        );
        assert_eq!(manager.status_of("missing"), None);
    }

    // ------------------------------------------------------------------
    // McpTool.execute: result mapping, timeout, truncation
    // ------------------------------------------------------------------

    fn tool_with(result: McpCallResult) -> (Arc<TrackConn>, McpTool) {
        let conn = Arc::new(TrackConn {
            result: Mutex::new(CannedCall::Ok(result)),
        });
        let tool = McpTool::new(
            "mcp__srv__t".to_string(),
            info("t", Some("does things")),
            "srv".to_string(),
            conn.clone(),
            Duration::from_secs(5),
        );
        (conn, tool)
    }

    /// Canned `call_tool` reply (anyhow::Error is not Clone, so the error
    /// variant stores a message and re-wraps at call time).
    enum CannedCall {
        Ok(McpCallResult),
        Err(String),
    }

    struct TrackConn {
        result: Mutex<CannedCall>,
    }

    #[async_trait]
    impl McpConnection for TrackConn {
        async fn list_tools(&self) -> anyhow::Result<Vec<McpToolInfo>> {
            Ok(Vec::new())
        }

        async fn call_tool(&self, name: &str, _arguments: &Value) -> anyhow::Result<McpCallResult> {
            assert_eq!(name, "t", "calls must use the ORIGINAL tool name");
            match &*self.result.lock().unwrap() {
                CannedCall::Ok(result) => Ok(result.clone()),
                CannedCall::Err(message) => Err(anyhow::anyhow!("{message}")),
            }
        }

        async fn shutdown(&self) {}
    }

    async fn run(tool: &McpTool) -> ToolOutput {
        let ctx = ToolContext::new(
            "call-1",
            Arc::new(crate::agent::EventHub::new()),
            CancellationToken::new(),
        );
        tool.execute(json!({"a": 1}), &ctx).await.unwrap()
    }

    /// Like [`run`], but with an ALREADY-cancelled run token: the cancel arm
    /// of the execute select must win without any clock involved.
    async fn run_cancelled(tool: &McpTool) -> ToolOutput {
        let token = CancellationToken::new();
        token.cancel();
        let ctx = ToolContext::new("call-1", Arc::new(crate::agent::EventHub::new()), token);
        tool.execute(json!({"a": 1}), &ctx).await.unwrap()
    }

    #[tokio::test]
    async fn execute_maps_text_result_to_success_with_details() {
        let (_conn, tool) = tool_with(McpCallResult {
            text: "hello".to_string(),
            structured_json: Some(json!({"sum": 3})),
            is_error: false,
            images: Vec::new(),
        });
        let output = run(&tool).await;
        assert!(!output.is_error);
        assert_eq!(output.content, "hello");
        let details = output.details.unwrap();
        assert_eq!(details["server"], "srv");
        assert_eq!(details["tool"], "t");
        assert_eq!(details["structured"], json!({"sum": 3}));
    }

    #[tokio::test]
    async fn execute_renders_structured_only_results_as_pretty_json() {
        let (_conn, tool) = tool_with(McpCallResult {
            text: String::new(),
            structured_json: Some(json!({"rows": [], "count": 0})),
            is_error: false,
            images: Vec::new(),
        });
        let output = run(&tool).await;
        assert!(!output.is_error);
        assert!(
            output.content.contains("\"count\": 0"),
            "pretty JSON expected, got: {}",
            output.content
        );
    }

    #[tokio::test]
    async fn execute_maps_is_error_to_error_output() {
        let (_conn, tool) = tool_with(McpCallResult {
            text: "exploded".to_string(),
            structured_json: None,
            is_error: true,
            images: Vec::new(),
        });
        let output = run(&tool).await;
        assert!(output.is_error);
        assert_eq!(output.content, "exploded");
        assert!(output.details.is_some());
    }

    #[tokio::test]
    async fn execute_substitutes_placeholder_for_empty_results() {
        let (_conn, tool) = tool_with(McpCallResult {
            text: String::new(),
            structured_json: None,
            is_error: false,
            images: Vec::new(),
        });
        let output = run(&tool).await;
        assert!(!output.is_error);
        assert_eq!(output.content, NO_CONTENT);
    }

    #[tokio::test]
    async fn execute_truncates_huge_output_with_shared_budget() {
        let (_conn, tool) = tool_with(McpCallResult {
            text: "x".repeat(MAX_TOOL_OUTPUT_BYTES * 2),
            structured_json: None,
            is_error: false,
            images: Vec::new(),
        });
        let output = run(&tool).await;
        assert!(output.content.starts_with("[output truncated"));
        assert!(output.content.len() < MAX_TOOL_OUTPUT_BYTES * 2);
    }

    #[tokio::test]
    async fn execute_maps_result_images_into_the_output() {
        let (_conn, tool) = tool_with(McpCallResult {
            text: "chart rendered".to_string(),
            images: vec![
                oct_mcp::McpImage {
                    mime_type: "image/png".to_string(),
                    data: "aVBuZw==".to_string(),
                },
                oct_mcp::McpImage {
                    mime_type: "image/jpeg".to_string(),
                    data: "aGVpZw==".to_string(),
                },
            ],
            structured_json: None,
            is_error: false,
        });
        let output = run(&tool).await;
        assert!(!output.is_error);
        assert_eq!(output.content, "chart rendered");
        assert_eq!(
            output.images,
            vec![
                ToolImage {
                    mime_type: "image/png".to_string(),
                    data: "aVBuZw==".to_string(),
                },
                ToolImage {
                    mime_type: "image/jpeg".to_string(),
                    data: "aGVpZw==".to_string(),
                },
            ]
        );
    }

    #[tokio::test]
    async fn execute_image_only_result_keeps_placeholder_text_with_images() {
        let (_conn, tool) = tool_with(McpCallResult {
            text: String::new(),
            images: vec![oct_mcp::McpImage {
                mime_type: "image/png".to_string(),
                data: "c2hvdA==".to_string(),
            }],
            structured_json: None,
            is_error: false,
        });
        let output = run(&tool).await;
        assert_eq!(output.content, NO_CONTENT);
        assert_eq!(output.images.len(), 1);
    }

    #[tokio::test]
    async fn execute_drops_images_over_the_count_and_size_budget() {
        // Count budget: only the first MAX_RESULT_IMAGES survive, the rest
        // are counted in details.
        let many: Vec<_> = (0..(MAX_RESULT_IMAGES + 3))
            .map(|i| oct_mcp::McpImage {
                mime_type: "image/png".to_string(),
                data: format!("img{i}"),
            })
            .collect();
        let (_conn, tool) = tool_with(McpCallResult {
            text: "shots".to_string(),
            images: many,
            structured_json: None,
            is_error: false,
        });
        let output = run(&tool).await;
        assert_eq!(output.images.len(), MAX_RESULT_IMAGES);
        assert_eq!(output.images[0].data, "img0");
        assert_eq!(output.details.as_ref().unwrap()["images_dropped"], json!(3));

        // Size budget: one oversized image is dropped whole, the rest kept.
        let (_conn, tool) = tool_with(McpCallResult {
            text: "big".to_string(),
            images: vec![
                oct_mcp::McpImage {
                    mime_type: "image/png".to_string(),
                    data: "ok".to_string(),
                },
                oct_mcp::McpImage {
                    mime_type: "image/png".to_string(),
                    data: "x".repeat(MAX_IMAGE_BASE64_CHARS + 1),
                },
            ],
            structured_json: None,
            is_error: false,
        });
        let output = run(&tool).await;
        assert_eq!(output.images.len(), 1);
        assert_eq!(output.images[0].data, "ok");
        assert_eq!(output.details.as_ref().unwrap()["images_dropped"], json!(1));
    }

    /// A connection whose calls never resolve (exercises the timeout and
    /// cancellation paths).
    struct Hanging;

    #[async_trait]
    impl McpConnection for Hanging {
        async fn list_tools(&self) -> anyhow::Result<Vec<McpToolInfo>> {
            Ok(Vec::new())
        }
        async fn call_tool(&self, _name: &str, _args: &Value) -> anyhow::Result<McpCallResult> {
            futures_util::future::pending().await
        }
        async fn shutdown(&self) {}
    }

    #[tokio::test]
    async fn execute_timeout_maps_to_error_output_naming_server_and_tool() {
        // A connection whose calls never resolve: the pending future must
        // outrun the 20ms budget baked into the tool below.
        let tool = McpTool::new(
            "mcp__srv__slow".to_string(),
            info("slow", None),
            "srv".to_string(),
            Arc::new(Hanging),
            Duration::from_millis(20),
        );
        let output = run(&tool).await;
        assert!(output.is_error);
        let content = output.content;
        assert!(content.contains("MCP tool error"), "got: {content}");
        assert!(content.contains("timed out"), "got: {content}");
        assert!(content.contains("`slow`"), "must name the tool: {content}");
        assert!(content.contains("`srv`"), "must name the server: {content}");
    }

    #[tokio::test]
    async fn execute_cancelled_run_returns_error_output_without_timeout_wait() {
        // A hanging call plus an ALREADY-cancelled token: the cancel arm must
        // win immediately — no clock is involved, so the 60s budget below
        // proves nothing was waited for (a hang would trip the test timeout).
        let tool = McpTool::new(
            "mcp__srv__slow".to_string(),
            info("slow", None),
            "srv".to_string(),
            Arc::new(Hanging),
            Duration::from_secs(60),
        );
        let output = run_cancelled(&tool).await;
        assert!(output.is_error);
        let content = output.content;
        assert!(content.contains("MCP tool error"), "got: {content}");
        assert!(content.contains("cancelled"), "got: {content}");
        assert!(content.contains("`slow`"), "must name the tool: {content}");
        assert!(content.contains("`srv`"), "must name the server: {content}");
    }

    #[tokio::test]
    async fn execute_connection_error_maps_to_error_output() {
        let conn = Arc::new(TrackConn {
            result: Mutex::new(CannedCall::Err("peer went away".to_string())),
        });
        let tool = McpTool::new(
            "mcp__srv__t".to_string(),
            info("t", None),
            "srv".to_string(),
            conn,
            Duration::from_secs(5),
        );
        let output = run(&tool).await;
        assert!(output.is_error);
        assert!(output.content.contains("MCP tool error"));
        assert!(output.content.contains("peer went away"));
    }

    // ------------------------------------------------------------------
    // description building
    // ------------------------------------------------------------------

    #[test]
    fn description_prefixes_and_truncates() {
        assert_eq!(
            tool_description("gh", Some("create issues")),
            "[mcp:gh] create issues"
        );
        // Missing and blank descriptions get the placeholder.
        assert_eq!(
            tool_description("gh", None),
            format!("[mcp:gh] {NO_DESCRIPTION}")
        );
        assert_eq!(
            tool_description("gh", Some("   ")),
            format!("[mcp:gh] {NO_DESCRIPTION}")
        );

        let long = tool_description("srv", Some(&"x".repeat(5_000)));
        assert_eq!(long.chars().count(), MAX_DESCRIPTION_CHARS + 1);
        assert!(long.starts_with("[mcp:srv] "));
        assert!(long.ends_with('…'));
    }

    // ------------------------------------------------------------------
    // ensure_connections reconcile (real SQLite, missing-binary spawns)
    // ------------------------------------------------------------------

    async fn test_pool() -> sqlx::SqlitePool {
        let dir = tempfile::tempdir().expect("temp dir");
        let url = format!("sqlite://{}", dir.path().join("mcp_unit.db").display());
        let pool = crate::db::init_pool(&url).await.expect("init pool");
        // The pool (and its open connections) must outlive the TempDir
        // handle, or dropping the handle first deletes the file out from
        // under the test. Leaking the handle keeps the directory alive for
        // the rest of the test process.
        std::mem::forget(dir);
        pool
    }

    fn update_request(enabled: Option<bool>) -> db::mcp::UpdateMcpServerRequest {
        db::mcp::UpdateMcpServerRequest {
            name: None,
            transport: None,
            command: None,
            args: None,
            env: None,
            url: None,
            headers: None,
            enabled,
            tool_timeout_ms: None,
            project_id: None,
        }
    }

    async fn create_row(pool: &sqlx::SqlitePool, name: &str, command: &str) -> McpServer {
        create_scoped_row(pool, name, command, None).await
    }

    async fn create_scoped_row(
        pool: &sqlx::SqlitePool,
        name: &str,
        command: &str,
        project_id: Option<&str>,
    ) -> McpServer {
        db::mcp::create_mcp_server(
            pool,
            &db::mcp::CreateMcpServerRequest {
                name: name.to_string(),
                transport: "stdio".to_string(),
                command: Some(command.to_string()),
                args: None,
                env: None,
                url: None,
                headers: None,
                enabled: Some(true),
                tool_timeout_ms: Some(1_000),
                project_id: project_id.map(str::to_string),
            },
        )
        .await
        .expect("create mcp row")
    }

    /// FK target for scoped rows (mcp_servers.project_id REFERENCES
    /// projects(id), and the pool enforces foreign keys).
    async fn create_project_row(pool: &sqlx::SqlitePool, name: &str) -> db::projects::Project {
        db::projects::create_project(
            pool,
            &db::projects::CreateProjectRequest {
                name: name.to_string(),
                working_dir: "/tmp/oct-mcp-unit-project".to_string(),
                additional_dirs: None,
            },
        )
        .await
        .expect("create project row")
    }

    #[tokio::test]
    async fn reconcile_records_failed_status_for_unspawnable_servers() {
        let pool = test_pool().await;
        let row = create_row(&pool, "broken", "/nonexistent/oct-mcp-unit-server").await;
        let manager = McpManager::default();

        manager.ensure_connections(&pool, None).await;

        match manager.status_of(&row.id) {
            Some(ServerStatus::Failed { error }) => {
                assert!(
                    error.contains("/nonexistent/oct-mcp-unit-server"),
                    "error should name the command: {error}"
                );
            }
            other => panic!("expected Failed status, got {other:?}"),
        }
        // Failed servers contribute no tools.
        assert!(manager.tools_for_run(None).is_empty());

        // Deleting the row removes the entry on the next reconcile.
        db::mcp::delete_mcp_server(&pool, &row.id)
            .await
            .expect("delete row");
        manager.ensure_connections(&pool, None).await;
        assert_eq!(manager.status_of(&row.id), None);
    }

    #[tokio::test]
    async fn reconcile_removes_disabled_rows() {
        let pool = test_pool().await;
        let row = create_row(&pool, "off", "/nonexistent/oct-mcp-unit-server").await;
        let manager = McpManager::default();
        manager.ensure_connections(&pool, None).await;
        assert!(manager.status_of(&row.id).is_some());

        db::mcp::update_mcp_server(&pool, &row.id, &update_request(Some(false)))
            .await
            .expect("disable row");
        manager.ensure_connections(&pool, None).await;
        assert_eq!(manager.status_of(&row.id), None);
    }

    #[tokio::test]
    async fn reconcile_never_touches_test_injected_entries() {
        let pool = test_pool().await;
        // An injected entry whose id has NO row at all: reconcile must leave
        // it connected (externally owned).
        let manager = McpManager::default();
        let fake = FakeConn::ok("x");
        manager.insert_connection_for_test(
            "injected",
            "fake",
            None,
            fake.clone(),
            vec![info("echo", None)],
        );

        // Also create a real row so the reconcile has work to do.
        let _row = create_row(&pool, "broken", "/nonexistent/oct-mcp-unit-server").await;
        manager.ensure_connections(&pool, None).await;

        assert_eq!(
            manager.status_of("injected"),
            Some(ServerStatus::Connected { tool_count: 1 })
        );
        let tools = manager.tools_for_run(None);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name(), "mcp__fake__echo");
    }

    #[tokio::test]
    async fn reconcile_reconnects_when_fingerprint_changes() {
        let pool = test_pool().await;
        let row = create_row(&pool, "edit", "/nonexistent/oct-mcp-unit-server").await;
        let manager = McpManager::default();

        // A managed Connected entry under a stale fingerprint (only
        // reachable in-process; production entries with Some(fp) come from
        // ensure_connections itself).
        manager.servers.insert(
            row.id.clone(),
            ManagedServer {
                name: "edit".to_string(),
                status: ServerStatus::Connected { tool_count: 1 },
                generation: 0,
                fingerprint: Some("stale-fingerprint".to_string()),
                scope: None,
                connection: Some(FakeConn::ok("x")),
                auth_rejected: Default::default(),
                tools: vec![info("old", None)],
                prompts: Vec::new(),
                resources: Vec::new(),
                tool_timeout: DEFAULT_TOOL_TIMEOUT,
            },
        );

        manager.ensure_connections(&pool, None).await;

        // The stale entry was disconnected and a real reconnect attempted
        // (which fails against the missing binary): Connected never survives
        // a fingerprint mismatch.
        match manager.status_of(&row.id) {
            Some(ServerStatus::Failed { .. }) => {}
            other => panic!("expected a reconnect attempt (Failed), got {other:?}"),
        }
        assert!(manager.tools_for_run(None).is_empty());
    }

    #[tokio::test]
    async fn reconcile_keeps_connected_entries_with_matching_fingerprint() {
        let pool = test_pool().await;
        let row = create_row(&pool, "keep", "/nonexistent/oct-mcp-unit-server").await;
        let manager = McpManager::default();
        let fake = FakeConn::ok("kept");
        manager.servers.insert(
            row.id.clone(),
            ManagedServer {
                name: "keep".to_string(),
                status: ServerStatus::Connected { tool_count: 1 },
                generation: 0,
                fingerprint: Some(fingerprint_of(&row)),
                scope: row.project_id.clone(),
                connection: Some(fake.clone()),
                auth_rejected: Default::default(),
                tools: vec![info("echo", None)],
                prompts: Vec::new(),
                resources: Vec::new(),
                tool_timeout: tool_timeout_of(&row),
            },
        );

        manager.ensure_connections(&pool, None).await;

        // Untouched: the same connection instance survives (no spawn of the
        // missing binary was attempted, or the entry would be Failed).
        assert_eq!(
            manager.status_of(&row.id),
            Some(ServerStatus::Connected { tool_count: 1 })
        );
        let tools = manager.tools_for_run(None);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name(), "mcp__keep__echo");
    }

    // ------------------------------------------------------------------
    // project-scoped reconcile
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn reconcile_keeps_another_projects_connected_entry() {
        let pool = test_pool().await;
        let owner = create_project_row(&pool, "owner").await;
        let other = create_project_row(&pool, "other").await;
        let row = create_scoped_row(
            &pool,
            "scoped",
            "/nonexistent/oct-mcp-unit-server",
            Some(&owner.id),
        )
        .await;
        let manager = McpManager::default();
        let fake = FakeConn::ok("x");
        manager.servers.insert(
            row.id.clone(),
            ManagedServer {
                name: "scoped".to_string(),
                status: ServerStatus::Connected { tool_count: 1 },
                generation: 0,
                fingerprint: Some(fingerprint_of(&row)),
                scope: Some(owner.id.clone()),
                connection: Some(fake),
                auth_rejected: Default::default(),
                tools: vec![info("mine", None)],
                prompts: Vec::new(),
                resources: Vec::new(),
                tool_timeout: tool_timeout_of(&row),
            },
        );

        // Another project's turn reconciles: the entry is out of its scope
        // but its config is unchanged, so it must survive untouched (an
        // in-flight run of the owning project may still hold the Arc).
        manager.ensure_connections(&pool, Some(&other.id)).await;

        assert_eq!(
            manager.status_of(&row.id),
            Some(ServerStatus::Connected { tool_count: 1 })
        );
        let for_owner = manager.tools_for_run(Some(&owner.id));
        assert_eq!(for_owner.len(), 1);
        assert_eq!(for_owner[0].name(), "mcp__scoped__mine");
        assert!(manager.tools_for_run(Some(&other.id)).is_empty());
    }

    #[tokio::test]
    async fn reconcile_out_of_scope_fingerprint_change_only_removes_the_entry() {
        let pool = test_pool().await;
        let owner = create_project_row(&pool, "owner").await;
        let other = create_project_row(&pool, "other").await;
        let row = create_scoped_row(
            &pool,
            "scoped",
            "/nonexistent/oct-mcp-unit-server",
            Some(&owner.id),
        )
        .await;
        let manager = McpManager::default();
        manager.servers.insert(
            row.id.clone(),
            ManagedServer {
                name: "scoped".to_string(),
                status: ServerStatus::Connected { tool_count: 1 },
                generation: 0,
                fingerprint: Some("stale-fingerprint".to_string()),
                scope: Some(owner.id.clone()),
                connection: Some(FakeConn::ok("x")),
                auth_rejected: Default::default(),
                tools: vec![info("mine", None)],
                prompts: Vec::new(),
                resources: Vec::new(),
                tool_timeout: DEFAULT_TOOL_TIMEOUT,
            },
        );

        // The OTHER project's turn sees the stale entry: remove only, no
        // reconnect — status_of None proves no attempt happened (a reconnect
        // against the missing binary would have left a Failed entry).
        manager.ensure_connections(&pool, Some(&other.id)).await;
        assert_eq!(manager.status_of(&row.id), None);

        // The owning project's next turn is the one that reconnects.
        manager.ensure_connections(&pool, Some(&owner.id)).await;
        assert!(matches!(
            manager.status_of(&row.id),
            Some(ServerStatus::Failed { .. })
        ));
    }

    #[tokio::test]
    async fn reconcile_removes_deleted_scoped_rows_even_from_another_project() {
        let pool = test_pool().await;
        let owner = create_project_row(&pool, "owner").await;
        let other = create_project_row(&pool, "other").await;
        let row = create_scoped_row(
            &pool,
            "scoped",
            "/nonexistent/oct-mcp-unit-server",
            Some(&owner.id),
        )
        .await;
        let manager = McpManager::default();
        // Establish a (Failed) managed entry from the owning project's turn.
        manager.ensure_connections(&pool, Some(&owner.id)).await;
        assert!(manager.status_of(&row.id).is_some());

        // Deleting the row is scope-independent: any project's reconcile
        // removes the orphaned entry.
        db::mcp::delete_mcp_server(&pool, &row.id)
            .await
            .expect("delete row");
        manager.ensure_connections(&pool, Some(&other.id)).await;
        assert_eq!(manager.status_of(&row.id), None);
    }

    #[tokio::test]
    async fn reconcile_reconnects_on_scope_change_and_refreshes_toolset() {
        let pool = test_pool().await;
        let project = create_project_row(&pool, "owner").await;
        // Start global with a live matching entry: every project sees it.
        let row = create_row(&pool, "mover", "/nonexistent/oct-mcp-unit-server").await;
        let manager = McpManager::default();
        manager.servers.insert(
            row.id.clone(),
            ManagedServer {
                name: "mover".to_string(),
                status: ServerStatus::Connected { tool_count: 1 },
                generation: 0,
                fingerprint: Some(fingerprint_of(&row)),
                scope: None,
                connection: Some(FakeConn::ok("x")),
                auth_rejected: Default::default(),
                tools: vec![info("tool", None)],
                prompts: Vec::new(),
                resources: Vec::new(),
                tool_timeout: tool_timeout_of(&row),
            },
        );
        assert_eq!(manager.tools_for_run(Some(&project.id)).len(), 1);

        // Re-scope the row to the project: project_id is part of the
        // fingerprint, so the project's next reconcile drops the stale entry
        // and reconnects (Failed here — the binary does not exist).
        let scoped = db::mcp::UpdateMcpServerRequest {
            project_id: Some(project.id.clone()),
            ..update_request(None)
        };
        db::mcp::update_mcp_server(&pool, &row.id, &scoped)
            .await
            .expect("scope the row");
        manager.ensure_connections(&pool, Some(&project.id)).await;
        assert!(matches!(
            manager.status_of(&row.id),
            Some(ServerStatus::Failed { .. })
        ));
        assert!(manager.tools_for_run(Some(&project.id)).is_empty());
    }

    // ------------------------------------------------------------------
    // Reconcile concurrency: per-server gates + invalidation epochs
    // ------------------------------------------------------------------

    fn connected_outcome(tools: Vec<McpToolInfo>) -> ConnectOutcome {
        let tool_count = tools.len();
        (
            ServerStatus::Connected { tool_count },
            Some(FakeConn::ok("x")),
            ServerCatalog {
                tools,
                prompts: Vec::new(),
                resources: Vec::new(),
            },
        )
    }

    /// Regression (P2「MCP reconcile 按-server 合并」): with the old global
    /// reconcile lock, project B's ensure queued behind project A's slowest
    /// server's handshake. Now a blocked connect of one server only pins its
    /// own gate — an ensure touching other servers completes.
    #[tokio::test]
    async fn slow_server_does_not_queue_another_projects_ensure() {
        let pool = test_pool().await;
        let p1 = create_project_row(&pool, "p1-project").await;
        // slow: scoped to p1; fast: global (in scope for both projects).
        let slow = create_scoped_row(&pool, "slow", "cmd", Some(&p1.id)).await;
        let fast = create_row(&pool, "fast", "cmd").await;

        let manager = Arc::new(McpManager::default());
        let release = Arc::new(tokio::sync::Notify::new());
        {
            let release = release.clone();
            manager.set_connector_for_test(move |_pool, row| {
                let release = release.clone();
                Box::pin(async move {
                    if row.name == "slow" {
                        release.notified().await;
                    }
                    connected_outcome(Vec::new())
                })
            });
        }

        let slow_manager = manager.clone();
        let slow_pool = pool.clone();
        let blocked = tokio::spawn(async move {
            slow_manager
                .ensure_connections(&slow_pool, Some(&p1.id))
                .await
        });
        // Let p1's ensure enter the slow handshake (and connect `fast`).
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(matches!(
            manager.status_of(&fast.id),
            Some(ServerStatus::Connected { .. })
        ));

        // p2's ensure touches only `fast` (already connected, no work) and
        // must return promptly — it does NOT queue behind `slow`.
        let p2 = create_project_row(&pool, "p2-project").await;
        tokio::time::timeout(
            Duration::from_secs(2),
            manager.ensure_connections(&pool, Some(&p2.id)),
        )
        .await
        .expect("p2's ensure completes while p1's slow server is blocked");

        release.notify_waiters();
        blocked.await.expect("blocked ensure finishes");
        assert!(matches!(
            manager.status_of(&slow.id),
            Some(ServerStatus::Connected { .. })
        ));
    }

    /// Two concurrent reconciles that decide the same server needs
    /// connecting share ONE handshake: the second waits on the server's
    /// gate, re-checks, and finds the first's entry already in place.
    #[tokio::test]
    async fn concurrent_ensures_share_one_handshake_per_server() {
        let pool = test_pool().await;
        let row = create_row(&pool, "once", "cmd").await;

        let manager = Arc::new(McpManager::default());
        let release = Arc::new(tokio::sync::Notify::new());
        let handshakes = Arc::new(AtomicUsize::new(0));
        {
            let release = release.clone();
            let handshakes = handshakes.clone();
            manager.set_connector_for_test(move |_pool, _row| {
                let release = release.clone();
                let handshakes = handshakes.clone();
                Box::pin(async move {
                    handshakes.fetch_add(1, Ordering::SeqCst);
                    release.notified().await;
                    connected_outcome(Vec::new())
                })
            });
        }

        let first = tokio::spawn({
            let manager = manager.clone();
            let pool = pool.clone();
            async move { manager.ensure_connections(&pool, None).await }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        let second = tokio::spawn({
            let manager = manager.clone();
            let pool = pool.clone();
            async move { manager.ensure_connections(&pool, None).await }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        // Still exactly one in-flight handshake; the second ensure is parked
        // on the server's gate, not on a second connect.
        assert_eq!(handshakes.load(Ordering::SeqCst), 1);

        release.notify_waiters();
        first.await.expect("first ensure");
        second.await.expect("second ensure");
        assert_eq!(handshakes.load(Ordering::SeqCst), 1, "one handshake total");
        assert!(matches!(
            manager.status_of(&row.id),
            Some(ServerStatus::Connected { .. })
        ));
    }

    /// A plan queued before CRUD must recheck the row after taking its gate.
    #[tokio::test]
    async fn queued_connect_plan_cannot_recreate_an_updated_or_deleted_server() {
        use std::sync::atomic::AtomicUsize;
        for delete in [false, true] {
            let pool = test_pool().await;
            let row = create_row(&pool, "queued", "unused").await;
            let manager = Arc::new(McpManager::default());
            let calls = Arc::new(AtomicUsize::new(0));
            manager.set_connector_for_test({
                let calls = calls.clone();
                move |_, _| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Box::pin(async { connected_outcome(Vec::new()) })
                }
            });
            let gate = manager.gate_for(&row.id);
            let held = gate.lock().await;
            let queued = {
                let manager = manager.clone();
                let pool = pool.clone();
                let row = row.clone();
                tokio::spawn(async move { manager.connect_one(&pool, &row).await })
            };
            if delete {
                db::mcp::delete_mcp_server(&pool, &row.id).await.unwrap();
            } else {
                let mut update = update_request(None);
                update.command = Some("replacement".into());
                db::mcp::update_mcp_server(&pool, &row.id, &update)
                    .await
                    .unwrap();
            }
            manager.invalidate(&row.id).await;
            drop(held);
            queued.await.unwrap();
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            assert!(manager.status_of(&row.id).is_none());
        }
    }

    /// An OAuth invalidation that lands while a handshake is in flight must
    /// not be overwritten by the (stale-credential) result arriving late:
    /// the connect's epoch ticket no longer matches, so the entry never
    /// re-enters the map.
    #[tokio::test]
    async fn invalidate_during_inflight_connect_drops_the_late_result() {
        let pool = test_pool().await;
        let row = create_row(&pool, "stale-token", "cmd").await;

        let manager = Arc::new(McpManager::default());
        let release = Arc::new(tokio::sync::Notify::new());
        {
            let release = release.clone();
            manager.set_connector_for_test(move |_pool, _row| {
                let release = release.clone();
                Box::pin(async move {
                    release.notified().await;
                    connected_outcome(vec![info("a", None)])
                })
            });
        }

        let connecting = tokio::spawn({
            let manager = manager.clone();
            let pool = pool.clone();
            async move { manager.ensure_connections(&pool, None).await }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        // The (hypothetical) login/logout lands mid-handshake: bumps the
        // epoch and removes whatever entry exists — none yet — then returns
        // without waiting out the handshake.
        manager.invalidate(&row.id).await;
        release.notify_waiters();
        connecting.await.expect("ensure finishes");

        assert!(
            manager.status_of(&row.id).is_none(),
            "late connect result must not re-enter the map after invalidation"
        );
    }

    // ------------------------------------------------------------------
    // tools/list_changed refresh loop (debounce, generation guard)
    // ------------------------------------------------------------------

    /// Fake connection with a manual notification trigger, a mutable tool
    /// catalog and a `list_tools` call counter — everything the refresh
    /// loop's tests need to observe (the richer tests/support fake is not
    /// visible from inside the crate).
    struct NotifyFake {
        tools: Mutex<Vec<McpToolInfo>>,
        list_calls: AtomicUsize,
        notifier: oct_mcp::ToolListNotifier,
        watcher: ToolListChanged,
    }

    impl NotifyFake {
        fn new(tools: Vec<McpToolInfo>) -> Arc<Self> {
            let (notifier, watcher) = oct_mcp::tool_list_changed_channel();
            Arc::new(Self {
                tools: Mutex::new(tools),
                list_calls: AtomicUsize::new(0),
                notifier,
                watcher,
            })
        }

        fn set_tools(&self, tools: Vec<McpToolInfo>) {
            *self.tools.lock().unwrap() = tools;
        }

        fn list_call_count(&self) -> usize {
            self.list_calls.load(Ordering::SeqCst)
        }

        /// The server announces a catalog change (manual fire).
        fn notify(&self) {
            self.notifier.notify();
        }
    }

    #[async_trait]
    impl McpConnection for NotifyFake {
        async fn list_tools(&self) -> anyhow::Result<Vec<McpToolInfo>> {
            self.list_calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.tools.lock().unwrap().clone())
        }

        async fn call_tool(
            &self,
            _name: &str,
            _arguments: &Value,
        ) -> anyhow::Result<McpCallResult> {
            Ok(McpCallResult {
                text: String::new(),
                structured_json: None,
                is_error: false,
                images: Vec::new(),
            })
        }

        async fn shutdown(&self) {}

        fn tool_list_changed(&self) -> Option<ToolListChanged> {
            Some(self.watcher.clone())
        }
    }

    fn tool_names(tools: Vec<McpToolInfo>) -> Vec<String> {
        tools.into_iter().map(|t| t.name).collect()
    }

    #[tokio::test(start_paused = true)]
    async fn list_changed_notification_refreshes_the_entry_after_debounce() {
        let manager = McpManager::default();
        let fake = NotifyFake::new(vec![info("a", None)]);
        manager.insert_connection_for_test(
            "id-1",
            "one",
            None,
            fake.clone(),
            vec![info("a", None)],
        );
        assert_eq!(tool_names(manager.tools_of("id-1")), ["a".to_string()]);

        // The server gains a tool and announces the catalog change.
        fake.set_tools(vec![info("a", None), info("b", None)]);
        fake.notify();

        // Paused clock: time auto-advances only while every task is parked,
        // so sleeping past the debounce window guarantees the refresh (whose
        // deadline sits at the window's start) has completed — deterministic,
        // no wall-clock sleep, no polling.
        tokio::time::sleep(TOOL_LIST_REFRESH_DEBOUNCE * 2).await;

        assert_eq!(fake.list_call_count(), 1, "one notification = one re-list");
        assert_eq!(
            tool_names(manager.tools_of("id-1")),
            ["a".to_string(), "b".to_string()]
        );
        assert_eq!(
            manager.status_of("id-1"),
            Some(ServerStatus::Connected { tool_count: 2 })
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_burst_of_notifications_debounces_into_a_single_refresh() {
        let manager = McpManager::default();
        let fake = NotifyFake::new(vec![info("a", None)]);
        manager.insert_connection_for_test(
            "id-1",
            "one",
            None,
            fake.clone(),
            vec![info("a", None)],
        );

        // Three notifications land inside one debounce window: the burst
        // must collapse into ONE tools/list round trip.
        fake.set_tools(vec![info("a", None), info("b", None)]);
        fake.notify();
        fake.notify();
        fake.notify();
        tokio::time::sleep(TOOL_LIST_REFRESH_DEBOUNCE * 2).await;

        assert_eq!(
            fake.list_call_count(),
            1,
            "a burst inside the window is one refresh"
        );
        assert_eq!(
            tool_names(manager.tools_of("id-1")),
            ["a".to_string(), "b".to_string()]
        );

        // A LATER notification still starts a fresh cycle (no coalescing
        // across windows).
        fake.set_tools(vec![info("a", None), info("b", None), info("c", None)]);
        fake.notify();
        tokio::time::sleep(TOOL_LIST_REFRESH_DEBOUNCE * 2).await;

        assert_eq!(fake.list_call_count(), 2);
        assert_eq!(manager.tools_of("id-1").len(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn refresh_task_retires_when_its_entry_is_replaced() {
        let manager = McpManager::default();
        let stale = NotifyFake::new(vec![info("stale", None)]);
        manager.insert_connection_for_test(
            "id-1",
            "one",
            None,
            stale.clone(),
            vec![info("stale", None)],
        );

        // The entry is disconnected (reconcile's removal path) and the same
        // id re-established under a fresh generation with a DIFFERENT
        // connection — the shape of a fingerprint-change reconnect.
        manager.remove_entry_for_test("id-1").await;
        let fresh = NotifyFake::new(vec![info("fresh", None)]);
        manager.insert_connection_for_test(
            "id-1",
            "one",
            None,
            fresh.clone(),
            vec![info("fresh", None)],
        );

        // The OLD connection's server announces (its feed never closed —
        // the fake keeps it alive); the old task wakes, runs its debounce
        // and re-lists on its own connection...
        stale.set_tools(vec![info("stale-2", None)]);
        stale.notify();
        tokio::time::sleep(TOOL_LIST_REFRESH_DEBOUNCE * 2).await;

        // ...the refresh really ran, but the generation guard rejected the
        // write: the live entry still shows the fresh connection's catalog.
        assert_eq!(
            stale.list_call_count(),
            1,
            "the retired task did perform its (rejected) re-list"
        );
        assert_eq!(tool_names(manager.tools_of("id-1")), ["fresh".to_string()]);
        assert_eq!(
            fresh.list_call_count(),
            0,
            "the fresh connection never announced"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn failed_refresh_keeps_the_entry_and_its_cached_tools() {
        // A server announcing a change while its tools/list breaks (e.g.
        // mid-restart): the refresh fails, the entry keeps its cached
        // catalog and Connected status — reconcile stays the authority on
        // reconnects.
        let manager = McpManager::default();

        // A connection whose tools/list always fails but whose feed fires.
        struct FailingList(ToolListChanged);
        #[async_trait]
        impl McpConnection for FailingList {
            async fn list_tools(&self) -> anyhow::Result<Vec<McpToolInfo>> {
                Err(anyhow::anyhow!("server restarted mid-list"))
            }
            async fn call_tool(
                &self,
                _name: &str,
                _arguments: &Value,
            ) -> anyhow::Result<McpCallResult> {
                unreachable!("no tool calls in this test")
            }
            async fn shutdown(&self) {}
            fn tool_list_changed(&self) -> Option<ToolListChanged> {
                Some(self.0.clone())
            }
        }
        let (notifier, watcher) = oct_mcp::tool_list_changed_channel();
        manager.insert_connection_for_test(
            "id-2",
            "broken",
            None,
            Arc::new(FailingList(watcher)),
            vec![info("kept", None)],
        );

        notifier.notify();
        tokio::time::sleep(TOOL_LIST_REFRESH_DEBOUNCE * 2).await;

        assert_eq!(tool_names(manager.tools_of("id-2")), ["kept".to_string()]);
        assert_eq!(
            manager.status_of("id-2"),
            Some(ServerStatus::Connected { tool_count: 1 })
        );
    }

    // ------------------------------------------------------------------
    // prompts/resources: catalog accessors + render/read call paths
    // ------------------------------------------------------------------

    /// Canned prompt/resource scripted connection for the catalog paths.
    struct CatalogFake {
        prompts: Vec<McpPromptInfo>,
        resources: Vec<McpResourceInfo>,
        prompt_calls: Mutex<Vec<(String, Value)>>,
        prompt_error: bool,
        resource_calls: Mutex<Vec<String>>,
        resource_hang: bool,
    }

    impl CatalogFake {
        fn new(prompts: Vec<McpPromptInfo>, resources: Vec<McpResourceInfo>) -> Arc<Self> {
            Arc::new(Self {
                prompts,
                resources,
                prompt_calls: Mutex::new(Vec::new()),
                prompt_error: false,
                resource_calls: Mutex::new(Vec::new()),
                resource_hang: false,
            })
        }
    }

    #[async_trait]
    impl McpConnection for CatalogFake {
        async fn list_tools(&self) -> anyhow::Result<Vec<McpToolInfo>> {
            Ok(Vec::new())
        }
        async fn call_tool(
            &self,
            _name: &str,
            _arguments: &Value,
        ) -> anyhow::Result<McpCallResult> {
            unreachable!("no tool calls in the catalog tests")
        }
        async fn shutdown(&self) {}

        async fn list_prompts(&self) -> anyhow::Result<Vec<McpPromptInfo>> {
            Ok(self.prompts.clone())
        }
        async fn get_prompt(
            &self,
            name: &str,
            arguments: &Value,
        ) -> anyhow::Result<oct_mcp::McpPromptResult> {
            self.prompt_calls
                .lock()
                .unwrap()
                .push((name.to_string(), arguments.clone()));
            if self.prompt_error {
                return Err(anyhow::anyhow!("server exploded"));
            }
            Ok(oct_mcp::McpPromptResult {
                messages: vec![oct_mcp::McpPromptMessage {
                    role: "user".to_string(),
                    text: format!("{name}: {}", arguments["name"].as_str().unwrap_or("-")),
                }],
            })
        }
        async fn list_resources(&self) -> anyhow::Result<Vec<McpResourceInfo>> {
            Ok(self.resources.clone())
        }
        async fn read_resource(&self, uri: &str) -> anyhow::Result<oct_mcp::McpResourceContent> {
            self.resource_calls.lock().unwrap().push(uri.to_string());
            if self.resource_hang {
                futures_util::future::pending().await
            }
            match uri {
                "memo://notes" => Ok(oct_mcp::McpResourceContent {
                    mime_type: Some("text/markdown".to_string()),
                    text: Some("# Notes".to_string()),
                    blob_base64: None,
                }),
                _ => Ok(oct_mcp::McpResourceContent {
                    mime_type: Some("image/png".to_string()),
                    text: None,
                    blob_base64: Some("aVBuZw==".to_string()),
                }),
            }
        }
    }

    fn prompt_info(name: &str) -> McpPromptInfo {
        McpPromptInfo {
            name: name.to_string(),
            description: Some(format!("{name} prompt")),
            arguments: vec![oct_mcp::McpPromptArgument {
                name: "name".to_string(),
                description: None,
                required: true,
            }],
        }
    }

    fn resource_info(uri: &str) -> McpResourceInfo {
        McpResourceInfo {
            uri: uri.to_string(),
            name: uri.to_string(),
            description: None,
            mime_type: Some("text/markdown".to_string()),
        }
    }

    /// Insert a hand-built Connected entry carrying catalogs (the test seam
    /// keeps injected catalogs empty by design; these tests need them set).
    fn insert_catalog_entry(
        manager: &McpManager,
        id: &str,
        connection: Arc<dyn McpConnection>,
        prompts: Vec<McpPromptInfo>,
        resources: Vec<McpResourceInfo>,
    ) {
        manager.servers.insert(
            id.to_string(),
            ManagedServer {
                name: "srv".to_string(),
                status: ServerStatus::Connected { tool_count: 0 },
                generation: 0,
                fingerprint: None,
                scope: None,
                connection: Some(connection),
                auth_rejected: Default::default(),
                tools: Vec::new(),
                prompts,
                resources,
                tool_timeout: Duration::from_secs(5),
            },
        );
    }

    #[tokio::test]
    async fn catalog_accessors_return_cached_prompts_and_resources() {
        let manager = McpManager::default();
        let fake = CatalogFake::new(
            vec![prompt_info("greet")],
            vec![resource_info("memo://notes")],
        );
        insert_catalog_entry(
            &manager,
            "id-1",
            fake,
            vec![prompt_info("greet")],
            vec![resource_info("memo://notes")],
        );
        assert_eq!(manager.prompts_of("id-1").len(), 1);
        assert_eq!(manager.resources_of("id-1").len(), 1);
        // Missing entries report empty catalogs.
        assert!(manager.prompts_of("missing").is_empty());
        assert!(manager.resources_of("missing").is_empty());
    }

    #[tokio::test]
    async fn render_prompt_passes_arguments_and_maps_outcomes() {
        let manager = McpManager::default();
        let fake = CatalogFake::new(vec![prompt_info("greet")], Vec::new());
        insert_catalog_entry(
            &manager,
            "id-1",
            fake.clone(),
            vec![prompt_info("greet")],
            Vec::new(),
        );

        // Ok: arguments pass through verbatim.
        let outcome = manager
            .render_prompt("id-1", "greet", &json!({"name": "Ada"}))
            .await;
        match outcome {
            McpServerCall::Ok(result) => {
                assert_eq!(result.messages.len(), 1);
                assert_eq!(result.messages[0].text, "greet: Ada");
            }
            other => panic!("expected Ok, got {other:?}"),
        }
        assert_eq!(
            *fake.prompt_calls.lock().unwrap(),
            vec![("greet".to_string(), json!({"name": "Ada"}))]
        );

        // Unknown prompt against a NON-empty cached catalog: 404 shape
        // without hitting the wire.
        let outcome = manager.render_prompt("id-1", "nope", &Value::Null).await;
        assert_eq!(outcome, McpServerCall::NotConnected);
        assert_eq!(
            fake.prompt_calls.lock().unwrap().len(),
            1,
            "no extra wire call"
        );

        // Missing entry entirely.
        assert_eq!(
            manager
                .render_prompt("missing", "greet", &Value::Null)
                .await,
            McpServerCall::NotConnected
        );

        // Server-side failure maps to the 502 shape.
        let broken: Arc<dyn McpConnection> = Arc::new(CatalogFake {
            prompts: vec![prompt_info("greet")],
            resources: Vec::new(),
            prompt_calls: Mutex::new(Vec::new()),
            prompt_error: true,
            resource_calls: Mutex::new(Vec::new()),
            resource_hang: false,
        });
        insert_catalog_entry(
            &manager,
            "id-2",
            broken,
            vec![prompt_info("greet")],
            Vec::new(),
        );
        match manager.render_prompt("id-2", "greet", &Value::Null).await {
            McpServerCall::Failed(error) => assert!(error.contains("server exploded"), "{error}"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn render_prompt_with_empty_catalog_falls_through_to_the_wire() {
        // An empty catalog may be "fetch failed", so the wire decides.
        let manager = McpManager::default();
        let fake = CatalogFake::new(Vec::new(), Vec::new());
        insert_catalog_entry(&manager, "id-1", fake.clone(), Vec::new(), Vec::new());
        assert!(matches!(
            manager.render_prompt("id-1", "greet", &Value::Null).await,
            McpServerCall::Ok(_)
        ));
        assert_eq!(fake.prompt_calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn read_resource_maps_text_blob_and_not_found_shapes() {
        let manager = McpManager::default();
        let fake = CatalogFake::new(
            Vec::new(),
            vec![
                resource_info("memo://notes"),
                resource_info("file://logo.png"),
            ],
        );
        insert_catalog_entry(
            &manager,
            "id-1",
            fake,
            Vec::new(),
            vec![
                resource_info("memo://notes"),
                resource_info("file://logo.png"),
            ],
        );

        let text = manager.read_resource("id-1", "memo://notes").await;
        assert!(matches!(&text, McpServerCall::Ok(c) if c.text.as_deref() == Some("# Notes")));

        let blob = manager.read_resource("id-1", "file://logo.png").await;
        assert!(
            matches!(&blob, McpServerCall::Ok(c) if c.blob_base64.as_deref() == Some("aVBuZw=="))
        );

        // Unknown URI against a non-empty catalog: no wire call.
        assert_eq!(
            manager.read_resource("id-1", "nope://x").await,
            McpServerCall::NotConnected
        );
        assert_eq!(manager.resources_of("id-1").len(), 2);

        // Timeout maps to Failed (a 20ms budget against a hanging read).
        let hanging = CatalogFake {
            prompts: Vec::new(),
            resources: vec![resource_info("memo://notes")],
            prompt_calls: Mutex::new(Vec::new()),
            prompt_error: false,
            resource_calls: Mutex::new(Vec::new()),
            resource_hang: true,
        };
        manager.servers.insert(
            "id-2".to_string(),
            ManagedServer {
                name: "slow".to_string(),
                status: ServerStatus::Connected { tool_count: 0 },
                generation: 0,
                fingerprint: None,
                scope: None,
                connection: Some(Arc::new(hanging)),
                auth_rejected: Default::default(),
                tools: Vec::new(),
                prompts: Vec::new(),
                resources: vec![resource_info("memo://notes")],
                tool_timeout: Duration::from_millis(20),
            },
        );
        match manager.read_resource("id-2", "memo://notes").await {
            McpServerCall::Failed(error) => {
                assert!(error.contains("timed out"), "{error}")
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn tool_timeout_prefers_positive_row_override() {
        let mut row = create_row_sync("t", "c");
        assert_eq!(tool_timeout_of(&row), Duration::from_millis(1_234));
        // Missing and non-positive overrides fall back to the default.
        row.tool_timeout_ms = None;
        assert_eq!(tool_timeout_of(&row), DEFAULT_TOOL_TIMEOUT);
        row.tool_timeout_ms = Some(0);
        assert_eq!(tool_timeout_of(&row), DEFAULT_TOOL_TIMEOUT);
        row.tool_timeout_ms = Some(-5);
        assert_eq!(tool_timeout_of(&row), DEFAULT_TOOL_TIMEOUT);
    }

    fn create_row_sync(name: &str, command: &str) -> McpServer {
        McpServer {
            id: "id".to_string(),
            name: name.to_string(),
            transport: "stdio".to_string(),
            command: Some(command.to_string()),
            args: None,
            env: None,
            url: None,
            headers: None,
            enabled: true,
            tool_timeout_ms: Some(1_234),
            project_id: None,
            created_at: chrono::Utc::now().naive_utc(),
            updated_at: chrono::Utc::now().naive_utc(),
        }
    }
}
