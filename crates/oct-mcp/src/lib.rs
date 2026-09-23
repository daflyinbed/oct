//! # oct-mcp
//!
//! Narrow client-side wrapper over the official MCP Rust SDK ([rmcp]),
//! covering the two transports oct uses: child-process stdio and Streamable
//! HTTP.
//!
//! This crate is an isolation layer: rmcp's types NEVER leak into the public
//! API (signatures speak only in `anyhow`, `serde_json`, std and this crate's
//! own types — e.g. the notification feed pair
//! [`ToolListChanged`]/[`ToolListNotifier`]), so the rest of the workspace can
//! consume MCP tools without taking a direct rmcp dependency. The HTTP
//! transports pull rmcp's reqwest 0.13, which is why the whole workspace pins
//! reqwest at that single version (see the manifest for the invariant).
//!
//! Two halves:
//!
//! - [`connection`]: start an MCP server (spawn a child process over stdio,
//!   or reach a remote endpoint over Streamable HTTP with custom headers),
//!   run the initialize handshake, then list/call tools through the
//!   [`McpConnection`] trait (and observe `tools/list_changed` notifications
//!   via [`McpConnection::tool_list_changed`]).
//! - [`naming`]: pure helpers that translate between server-side tool names
//!   and the flat `mcp__{server}__{tool}` names exposed to the LLM, plus
//!   input-schema normalization.
//!
//! [rmcp]: https://crates.io/crates/rmcp

mod connection;
mod naming;

pub use connection::HttpServerConfig;
pub use connection::McpCallResult;
pub use connection::McpConnection;
pub use connection::McpImage;
pub use connection::McpToolInfo;
pub use connection::StdioServerConfig;
pub use connection::ToolListChanged;
pub use connection::ToolListNotifier;
pub use connection::connect_http;
pub use connection::connect_stdio;
pub use connection::tool_list_changed_channel;
pub use naming::MAX_TOOL_NAME_LEN;
pub use naming::ToolNameMap;
pub use naming::normalize_input_schema;
pub use naming::qualified_tool_name;
