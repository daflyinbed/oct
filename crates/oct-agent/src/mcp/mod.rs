//! MCP application layer, split by responsibility:
//!
//! * [`manager`] — [`McpManager`]: the process-wide connection registry and
//!   the reconcile against the enabled `mcp_servers` rows. Per-server gates
//!   serialize connects (one shared handshake per server) without a global
//!   lock spanning network waits, and an invalidation epoch keeps a late
//!   connect result from re-entering the map after an OAuth invalidation.
//! * [`connect`] — the connect pipeline: transport choice (stdio child /
//!   Streamable HTTP), handshake, and the three connect-time catalog lists.
//! * [`credential`] — the OAuth credential service: stored-token load,
//!   near-expiry refresh (login and auto-refresh share it), and the
//!   Authorization header merge.
//! * [`catalog`] — the `tools/list_changed` debounce/refresh loop that keeps
//!   a connected entry's tool list fresh.
//! * [`tool`] — [`McpTool`]: the `AgentTool` adapter exposing one server's
//!   tools to the agent loop under the deduplicated
//!   `mcp__{server}__{tool}` name.
//!
//! The low-level OAuth protocol functions live in [`crate::oauth`]; nothing
//! here leaks rmcp types into `agent`/`tools` (the boundary crate `oct_mcp`
//! exposes the narrow [`oct_mcp::McpConnection`] trait).

pub mod catalog;
pub mod connect;
pub mod credential;
pub mod manager;
pub mod tool;

pub use manager::{McpManager, McpServerCall, ServerStatus, ServerStatusEntry};
pub use tool::McpTool;
