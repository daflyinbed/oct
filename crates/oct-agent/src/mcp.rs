//! MCP integration layer: the connection manager plus the [`AgentTool`]
//! adapter that exposes one MCP server's tools to the agent loop.
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
//! [`McpTool`] then adapts one cached `tools/list` entry into the agent
//! loop's tool trait: the LLM sees the flat `mcp__{server}__{tool}` name
//! (deduplicated across servers by `oct_mcp::ToolNameMap`), while calls are
//! dispatched over the server's connection under the original tool name,
//! wrapped in that server's `tool_timeout_ms` budget and aborted as soon as
//! the run's cancellation token fires (the shared connection is never shut
//! down by an individual call).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dashmap::DashMap;
use serde_json::{Value, json};
use sqlx::SqlitePool;
use tracing::{info, warn};

use oct_mcp::ToolNameMap;
use oct_mcp::{McpCallResult, McpConnection, McpToolInfo, StdioServerConfig, connect_stdio};

use crate::db;
use crate::db::mcp::McpServer;
use crate::tools::truncate::MAX_TOOL_OUTPUT_BYTES;
use crate::tools::truncate::truncate_middle;
use crate::tools::{AgentTool, ToolContext, ToolOutput};

/// Budget for spawning one MCP server and finishing the initialize
/// handshake (and, separately, for the follow-up `tools/list`).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Per-call timeout for MCP tool calls when the server row carries no
/// `tool_timeout_ms`.
const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_secs(60);

/// Cap (in chars) on the LLM-visible description of an MCP tool. The
/// ellipsis is appended on top of the cap.
const MAX_DESCRIPTION_CHARS: usize = 1024;

/// Placeholder description when a server ships a tool without one.
const NO_DESCRIPTION: &str = "MCP tool (the server provided no description)";

/// Content fallback when a server returns neither text nor structured
/// content: an empty tool result is technically legal but unhelpful to the
/// model, so say so explicitly.
const NO_CONTENT: &str = "(MCP tool returned no content)";

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

/// What the manager tracks for one MCP server id.
struct ManagedServer {
    /// User-facing name from the DB row (UNIQUE); drives the exposed
    /// `mcp__{name}__{tool}` tool names.
    name: String,
    status: ServerStatus,
    /// Fingerprint of the config this entry was established under.
    /// `None` marks a test-injected entry (see
    /// [`McpManager::insert_connection_for_test`]): reconcile treats it as
    /// externally owned and never reconnects or removes it.
    fingerprint: Option<String>,
    /// Project scope this entry was established under (`None` = global,
    /// `Some(id)` = that project only). Copied from the row — and part of
    /// its fingerprint, so a scope edit refreshes it on the next reconcile —
    /// letting [`McpManager::tools_for_run`] filter without a DB read.
    scope: Option<String>,
    /// The live connection; `None` exactly when status is `Failed`.
    connection: Option<Arc<dyn McpConnection>>,
    /// Cached `tools/list` result (only meaningful while `Connected`).
    tools: Vec<McpToolInfo>,
    /// Per-call budget from the row's `tool_timeout_ms` (or the default).
    tool_timeout: Duration,
}

/// Owns the process-wide MCP connections. Shared through `AppState` as an
/// `Arc`; cheap to construct, so tests build their own per case.
#[derive(Default)]
pub struct McpManager {
    /// key = `mcp_servers.id`.
    servers: DashMap<String, ManagedServer>,
    /// Serializes reconciles: two concurrent chat turns must never both
    /// decide the same server needs connecting (the loser would overwrite —
    /// and leak — the winner's connection). Callers queue here; by the time
    /// the second one reconciles, the first one's entries are already
    /// `Connected` and it has nothing to do.
    reconcile_lock: tokio::sync::Mutex<()>,
}

impl McpManager {
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
    /// - any scope, a `Connected` entry under the current fingerprint →
    ///   untouched, even when the current project cannot use it (another
    ///   project's in-flight run may still be holding its connection).
    ///
    /// Entries (test-injected excepted) whose row disappeared from the
    /// enabled set — deleted or disabled, in any project — are disconnected
    /// and removed.
    pub async fn ensure_connections(&self, pool: &SqlitePool, project_id: Option<&str>) {
        let _guard = self.reconcile_lock.lock().await;

        let rows = match db::mcp::list_mcp_servers(pool).await {
            Ok(rows) => rows,
            Err(e) => {
                warn!("MCP reconcile skipped: cannot read mcp_servers: {e:#}");
                return;
            }
        };

        // ALL enabled rows, across every project: entries are process-wide,
        // so removal decisions must consider rows the current project cannot
        // see (a disabled scoped server in another project still has to go).
        let desired: HashMap<String, &McpServer> = rows
            .iter()
            .filter(|row| row.enabled)
            .map(|row| (row.id.clone(), row))
            .collect();

        // Entries whose row disappeared (deleted/disabled). Ref guards live
        // only inside the closure, so none is held across the awaits below.
        let stale: Vec<String> = self
            .servers
            .iter()
            .filter(|entry| {
                entry.value().fingerprint.is_some() && !desired.contains_key(entry.key())
            })
            .map(|entry| entry.key().clone())
            .collect();
        for id in stale {
            self.disconnect(&id).await;
        }

        // Stale-fingerprint entries of OUT-of-scope rows: remove only, no
        // reconnect (their project re-establishes them on its next turn).
        let mut out_of_scope_stale: Vec<String> = Vec::new();
        let to_connect: Vec<McpServer> = desired
            .into_iter()
            .filter(|(id, row)| {
                if scope_hits(row.project_id.as_deref(), project_id) {
                    if self.entry_needs_connect(id, row) {
                        self.disconnect_blocking(id);
                        true
                    } else {
                        false
                    }
                } else {
                    if self.entry_is_stale(id, row) {
                        out_of_scope_stale.push(id.clone());
                    }
                    false
                }
            })
            .map(|(_, row)| row.clone())
            .collect();
        for id in out_of_scope_stale {
            self.disconnect(&id).await;
        }

        if to_connect.is_empty() {
            return;
        }

        // Connect all pending servers concurrently; each outcome lands in
        // the map whatever it is (Failed is a state, not an error).
        let connects = to_connect
            .iter()
            .map(|row| connect_server(row))
            .collect::<Vec<_>>();
        let results = futures_util::future::join_all(connects).await;
        for (row, outcome) in to_connect.iter().zip(results) {
            let (status, connection, tools) = outcome;
            match &status {
                ServerStatus::Connected { tool_count } => info!(
                    server = %row.name,
                    tools = tool_count,
                    "MCP server connected"
                ),
                ServerStatus::Failed { error } => {
                    warn!(server = %row.name, %error, "MCP server connection failed")
                }
            }
            self.servers.insert(
                row.id.clone(),
                ManagedServer {
                    name: row.name.clone(),
                    status,
                    fingerprint: Some(fingerprint_of(row)),
                    scope: row.project_id.clone(),
                    connection,
                    tools,
                    tool_timeout: tool_timeout_of(row),
                },
            );
        }
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
                    || matches!(entry.value().status, ServerStatus::Failed { .. })
            }
        }
    }

    /// Whether the entry for `id` was established under a config that no
    /// longer matches the row (fingerprint mismatch) and therefore must not
    /// be kept as-is. Test-injected entries never count as stale.
    fn entry_is_stale(&self, id: &str, row: &McpServer) -> bool {
        let Some(entry) = self.servers.get(id) else {
            return false;
        };
        matches!(
            entry.value().fingerprint.as_deref(),
            Some(seen) if seen != fingerprint_of(row)
        )
    }

    /// Remove the entry for `id` and shut its connection down (if any).
    async fn disconnect(&self, id: &str) {
        if let Some((_, mut managed)) = self.servers.remove(id) {
            if let Some(connection) = managed.connection.take() {
                connection.shutdown().await;
            }
        }
    }

    /// `disconnect` for callers already inside sync code: shutdown is only
    /// awaited on the stale-removal path, while reconnect candidates had a
    /// `Failed` status (no connection) in just about every real case — but
    /// a fingerprint edit of a *connected* server lands here too, so the
    /// entry is removed first and its connection shut down detached, keeping
    /// the ordering (drop old, then connect new) without holding DashMap
    /// guards across an await.
    fn disconnect_blocking(&self, id: &str) {
        if let Some((_, managed)) = self.servers.remove(id) {
            if let Some(connection) = managed.connection {
                tokio::spawn(async move {
                    connection.shutdown().await;
                });
            }
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
                    && scope_hits(entry.value().scope.as_deref(), project_id)
            })
            .map(|entry| {
                let managed = entry.value();
                (
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
        connected.sort_by(|a, b| a.0.cmp(&b.0));

        let mut names = ToolNameMap::new();
        let mut tools = Vec::new();
        for (server, connection, infos, timeout) in connected {
            for info in infos {
                let exposed = names.insert(&server, &info.name);
                tools.push(Box::new(McpTool::new(
                    exposed,
                    info,
                    server.clone(),
                    connection.clone(),
                    timeout,
                )) as Box<dyn AgentTool>);
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
                status: entry.value().status.clone(),
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
            .map(|entry| entry.value().status.clone())
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

    /// Test seam (integration tests live outside the crate, hence `pub`):
    /// register a pre-established `connection` under `server_id` as a
    /// `Connected` server advertising `tools`, scoped to `scope` (`None` =
    /// global). The entry carries no config fingerprint, so
    /// [`Self::ensure_connections`] treats it as externally owned: it is
    /// never reconnected, and it survives reconciles even if the matching
    /// row is missing or disabled. Tool-call timeout is the default; build
    /// an [`McpTool`] directly to control it.
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
        self.servers.insert(
            server_id.to_string(),
            ManagedServer {
                name: name.to_string(),
                status: ServerStatus::Connected { tool_count },
                fingerprint: None,
                scope: scope.map(str::to_string),
                connection: Some(connection),
                tools,
                tool_timeout: DEFAULT_TOOL_TIMEOUT,
            },
        );
    }
}

/// Fingerprint of every launch- and scope-relevant field of a row. Any edit
/// that changes how the server is started (or addressed, once http lands)
/// must produce a different value, forcing a reconnect on the next
/// reconcile; `project_id` is included so a scope edit refreshes the
/// entry's scope view too. `updated_at` would be simpler but loses
/// precision on rapid edits and fires on irrelevant schema changes; the
/// semantic fields are exact.
fn fingerprint_of(server: &McpServer) -> String {
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
fn scope_hits(scope: Option<&str>, project_id: Option<&str>) -> bool {
    match scope {
        None => true,
        Some(scope) => Some(scope) == project_id,
    }
}

fn tool_timeout_of(server: &McpServer) -> Duration {
    server
        .tool_timeout_ms
        .filter(|ms| *ms > 0)
        .map(|ms| Duration::from_millis(ms as u64))
        .unwrap_or(DEFAULT_TOOL_TIMEOUT)
}

/// Connect one server row: spawn + handshake, then `tools/list`, each
/// bounded by [`CONNECT_TIMEOUT`]. Never returns an error — failures come
/// back as a `Failed` status with the error string for the API surface.
async fn connect_server(
    row: &McpServer,
) -> (
    ServerStatus,
    Option<Arc<dyn McpConnection>>,
    Vec<McpToolInfo>,
) {
    if row.transport != "stdio" {
        return failed(format!(
            "transport '{}' is not supported yet; only 'stdio' is available",
            row.transport
        ));
    }
    let Some(command) = row.command.as_deref() else {
        return failed("stdio transport requires a command".to_string());
    };
    let cfg = StdioServerConfig {
        command: command.to_string(),
        args: row.args.as_deref().cloned().unwrap_or_default(),
        env: row.env.as_deref().cloned().unwrap_or_default(),
    };
    let attempt = async {
        let connection = connect_stdio(&cfg, CONNECT_TIMEOUT).await?;
        // connect_stdio bounds only the handshake; bound tools/list too so a
        // server that lists forever cannot stall the chat turn.
        let tools = tokio::time::timeout(CONNECT_TIMEOUT, connection.list_tools())
            .await
            .map_err(|_| {
                anyhow::anyhow!("tools/list did not finish within {CONNECT_TIMEOUT:?}")
            })??;
        Ok::<_, anyhow::Error>((Arc::from(connection), tools))
    };
    match attempt.await {
        Ok((connection, tools)) => (
            ServerStatus::Connected {
                tool_count: tools.len(),
            },
            Some(connection),
            tools,
        ),
        Err(e) => failed(format!("{e:#}")),
    }
}

fn failed(
    error: String,
) -> (
    ServerStatus,
    Option<Arc<dyn McpConnection>>,
    Vec<McpToolInfo>,
) {
    (ServerStatus::Failed { error }, None, Vec::new())
}

/// `[mcp:{server}] {description}` for the LLM, capped at
/// [`MAX_DESCRIPTION_CHARS`] chars plus an ellipsis. A missing (or blank)
/// description gets a placeholder so the tool never advertises nothing.
fn tool_description(server: &str, description: Option<&str>) -> String {
    let body = description
        .filter(|d| !d.trim().is_empty())
        .unwrap_or(NO_DESCRIPTION);
    let full = format!("[mcp:{server}] {body}");
    if full.chars().count() <= MAX_DESCRIPTION_CHARS {
        full
    } else {
        let cut: String = full.chars().take(MAX_DESCRIPTION_CHARS).collect();
        format!("{cut}…")
    }
}

/// Model-visible content of one call result: the joined text blocks, or —
/// when the server sent no text — `structuredContent` rendered as pretty
/// JSON.
fn result_content(result: &McpCallResult) -> String {
    if !result.text.is_empty() {
        result.text.clone()
    } else if let Some(structured) = &result.structured_json {
        serde_json::to_string_pretty(structured).unwrap_or_else(|_| structured.to_string())
    } else {
        NO_CONTENT.to_string()
    }
}

/// One MCP tool behind the [`AgentTool`] trait. The LLM addresses it by the
/// deduplicated exposed name; `execute` dispatches over `connection` under
/// the original server-side name, bounded by `timeout`.
pub struct McpTool {
    exposed_name: String,
    original_name: String,
    server_name: String,
    connection: Arc<dyn McpConnection>,
    timeout: Duration,
    description: String,
    input_schema: Value,
}

impl McpTool {
    pub fn new(
        exposed_name: String,
        info: McpToolInfo,
        server_name: String,
        connection: Arc<dyn McpConnection>,
        timeout: Duration,
    ) -> Self {
        let description = tool_description(&server_name, info.description.as_deref());
        Self {
            exposed_name,
            original_name: info.name,
            server_name,
            connection,
            timeout,
            description,
            input_schema: info.input_schema,
        }
    }
}

#[async_trait]
impl AgentTool for McpTool {
    fn name(&self) -> &str {
        &self.exposed_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> serde_json::Value {
        self.input_schema.clone()
    }

    fn title(&self, _args: &serde_json::Value) -> String {
        format!("{} / {}", self.server_name, self.original_name)
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> anyhow::Result<ToolOutput> {
        // Never `Err`: a dead or slow server becomes an error ToolOutput so
        // the agent loop keeps running and the model can react to the text.
        // Timeout, cancellation and transport errors name the server and the
        // tool so the user (and the model) can locate the misbehaving party.
        let result = tokio::select! {
            result = self.connection.call_tool(&self.original_name, &args) => result,
            _ = tokio::time::sleep(self.timeout) => {
                return Ok(ToolOutput::error(format!(
                    "MCP tool error: tool `{}` on MCP server `{}` timed out after {:?}",
                    self.original_name, self.server_name, self.timeout
                )));
            }
            // The run was cancelled: bail out now instead of letting the call
            // run to its timeout. The shared connection is NOT shut down — it
            // is keyed by server id and outlives this run — and dropping the
            // request future simply makes rmcp discard the eventual response.
            // Sending a JSON-RPC cancelled notification is deliberately out
            // of scope here.
            _ = ctx.cancel_token().cancelled() => {
                return Ok(ToolOutput::error(format!(
                    "MCP tool error: tool `{}` on MCP server `{}` was cancelled by the user",
                    self.original_name, self.server_name
                )));
            }
        };
        let result = match result {
            Ok(result) => result,
            Err(e) => {
                return Ok(ToolOutput::error(format!(
                    "MCP tool error: calling `{}` on MCP server `{}` failed: {e:#}",
                    self.original_name, self.server_name
                )));
            }
        };

        // Reuse the shared output budget (head+tail truncation) so a server
        // dumping megabytes cannot flood the LLM context.
        let content = truncate_middle(&result_content(&result), MAX_TOOL_OUTPUT_BYTES);
        let details = json!({
            "server": self.server_name,
            "tool": self.original_name,
            "structured": result.structured_json,
        });
        if result.is_error {
            Ok(ToolOutput::error_with_details(content, details))
        } else {
            Ok(ToolOutput::success_with_details(content, details))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use serde_json::json;
    use tokio_util::sync::CancellationToken;

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
                fingerprint: Some("fp".to_string()),
                scope: None,
                connection: None,
                tools: vec![info("dead", None)],
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
        });
        let output = run(&tool).await;
        assert!(output.content.starts_with("[output truncated"));
        assert!(output.content.len() < MAX_TOOL_OUTPUT_BYTES * 2);
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
                fingerprint: Some("stale-fingerprint".to_string()),
                scope: None,
                connection: Some(FakeConn::ok("x")),
                tools: vec![info("old", None)],
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
                fingerprint: Some(fingerprint_of(&row)),
                scope: row.project_id.clone(),
                connection: Some(fake.clone()),
                tools: vec![info("echo", None)],
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
                fingerprint: Some(fingerprint_of(&row)),
                scope: Some(owner.id.clone()),
                connection: Some(fake),
                tools: vec![info("mine", None)],
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
                fingerprint: Some("stale-fingerprint".to_string()),
                scope: Some(owner.id.clone()),
                connection: Some(FakeConn::ok("x")),
                tools: vec![info("mine", None)],
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
                fingerprint: Some(fingerprint_of(&row)),
                scope: None,
                connection: Some(FakeConn::ok("x")),
                tools: vec![info("tool", None)],
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
