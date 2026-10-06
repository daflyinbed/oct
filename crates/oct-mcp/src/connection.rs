//! MCP client connections over stdio (child process) and Streamable HTTP.
//!
//! This module wraps rmcp's client stack behind the narrow [`McpConnection`]
//! trait. rmcp types (`RunningService`, `Peer`, model structs) appear ONLY in
//! private items — never in a public signature — so consumers of this crate
//! can treat MCP as "start a server (spawn it, or point at a URL), list
//! tools, call tools, watch for `tools/list_changed`, list/render prompts,
//! list/read resources, shut down" without a direct rmcp dependency.
//!
//! Both transports enter through the same [`connect_transport`] seam, so the
//! handshake, request encoding and notification wiring are identical; only
//! the byte pipe differs (newline-delimited stdio vs HTTP POST + optional
//! SSE).

use std::collections::HashMap;
use std::future::Future;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use async_trait::async_trait;
use http::HeaderName;
use http::HeaderValue;
use rmcp::ClientHandler;
use rmcp::model::CallToolRequestParams;
use rmcp::model::CallToolResult;
use rmcp::model::ClientCapabilities;
use rmcp::model::ClientConfig;
use rmcp::model::ContentBlock;
use rmcp::model::GetPromptRequestParams;
use rmcp::model::GetPromptResult;
use rmcp::model::Implementation;
use rmcp::model::PaginatedRequestParams;
use rmcp::model::Prompt;
use rmcp::model::ReadResourceRequestParams;
use rmcp::model::ReadResourceResult;
use rmcp::model::Resource;
use rmcp::model::ResourceContents;
use rmcp::model::Role;
use rmcp::model::Tool;
use rmcp::service::MaybeSendFuture;
use rmcp::service::NotificationContext;
use rmcp::service::Peer;
use rmcp::service::RoleClient;
use rmcp::service::RunningService;
use rmcp::service::serve_client;
use rmcp::transport::IntoTransport;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::child_process::TokioChildProcess;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde_json::Map;
use serde_json::Value;
use tokio::sync::Mutex;
use tokio::sync::watch;

use crate::naming::normalize_input_schema;

/// How to start one MCP server as a child process talking MCP over stdio.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StdioServerConfig {
    /// Executable to run (resolved through PATH like any other command).
    pub command: String,
    /// Command-line arguments passed to the executable.
    pub args: Vec<String>,
    /// Environment overrides applied ON TOP of the inherited environment:
    /// the child starts with the current process's env and these key/value
    /// pairs replace or add entries.
    pub env: HashMap<String, String>,
}

/// How to reach one MCP server speaking Streamable HTTP.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HttpServerConfig {
    /// Endpoint URL, e.g. `https://example.com/mcp`.
    pub url: String,
    /// Extra headers attached to EVERY request on this connection — the
    /// bearer seam: store `{"Authorization": "Bearer <token>"}` here (the
    /// value passes through verbatim, prefix included). rmcp reserves only
    /// `accept`/`mcp-session-id`/`last-event-id` for itself; `Authorization`
    /// and anything else goes onto the wire untouched.
    pub headers: HashMap<String, String>,
}

/// One tool advertised by an MCP server, already shaped for LLM consumption.
#[derive(Debug, Clone, PartialEq)]
pub struct McpToolInfo {
    /// Server-side original tool name (the name `call_tool` expects).
    pub name: String,
    pub description: Option<String>,
    /// The server's `inputSchema` with defaults filled in by
    /// [`normalize_input_schema`] (missing `type`/`properties` are
    /// defaulted; nothing the server sent is rewritten).
    pub input_schema: Value,
    /// Optional schema for structuredContent; kept separate from inputSchema.
    pub output_schema: Option<Value>,
}

/// One image content block from a tool result, ready for LLM pass-through:
/// base64-encoded bytes plus MIME type, verbatim from the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpImage {
    /// MIME type of the image, e.g. `image/png`.
    pub mime_type: String,
    /// Base64-encoded image data as the server sent it.
    pub data: String,
}

/// The outcome of one tool call, flattened from MCP's content-block model.
#[derive(Debug, Clone, PartialEq)]
pub struct McpCallResult {
    /// All text content blocks joined with `"\n"`, in server order.
    /// Non-image non-text blocks (audio, resources) are skipped — oct has no
    /// way to feed them to a tool result today.
    pub text: String,
    /// Image content blocks in server order, base64 data verbatim. Empty for
    /// text-only results.
    pub images: Vec<McpImage>,
    /// The server's `structuredContent` verbatim, when present.
    pub structured_json: Option<Value>,
    /// The server's `isError` flag (tool-level failure), defaulting to
    /// `false` when absent.
    pub is_error: bool,
}

/// One argument a prompt accepts, from the server's `prompts/list` catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpPromptArgument {
    pub name: String,
    pub description: Option<String>,
    /// The server's `required` flag, defaulted to `false` when absent.
    pub required: bool,
}

/// One prompt advertised by an MCP server (`prompts/list` entry).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpPromptInfo {
    /// Server-side original prompt name (the name `get_prompt` expects).
    pub name: String,
    pub description: Option<String>,
    /// Declared arguments in server order; empty for argument-less prompts.
    pub arguments: Vec<McpPromptArgument>,
}

/// One rendered prompt message (`prompts/get` entry). MCP prompt content is
/// flattened to text: non-text content blocks become placeholder notes (see
/// [`McpConnection::get_prompt`] for the limitation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpPromptMessage {
    /// `"user"` or `"assistant"`, lowercased from the wire.
    pub role: String,
    /// Text content of the message (placeholder text for non-text blocks).
    pub text: String,
}

/// The outcome of one `prompts/get` render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpPromptResult {
    /// Rendered messages in server order.
    pub messages: Vec<McpPromptMessage>,
}

/// One resource advertised by an MCP server (`resources/list` entry). oct
/// treats resources as read-only catalog data for the UI panel — they never
/// enter the chat/model context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpResourceInfo {
    pub uri: String,
    pub name: String,
    pub description: Option<String>,
    pub mime_type: Option<String>,
}

/// The content of one `resources/read`: the server returns text OR base64
/// blob contents; per the narrow "view in panel" contract, all text contents
/// are joined with `"\n"` and the first blob is taken verbatim (extras
/// dropped — a multi-blob read is vanishingly rare and the panel only
/// displays one view).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpResourceContent {
    pub mime_type: Option<String>,
    pub text: Option<String>,
    pub blob_base64: Option<String>,
}

/// Live feed of one server's `notifications/tools/list_changed`, exposed by
/// [`McpConnection::tool_list_changed`].
///
/// Backed by a `tokio` watch channel kept private so no foreign types leak
/// into this crate's signatures. The watched value is a monotonic revision
/// counter: every notification received on the connection increments it, so
/// consumers can either coalesce bursts ("something changed") or count them
/// ([`ToolListChanged::current`]).
///
/// Clone-cheap: every clone tracks its own "last seen" revision, exactly like
/// the underlying watch receiver.
#[derive(Debug, Clone)]
pub struct ToolListChanged {
    rx: watch::Receiver<u64>,
}

impl ToolListChanged {
    /// Current revision. Starts at 0 on connect; every received
    /// `tools/list_changed` notification increments it. Mainly useful for
    /// assertions and logging.
    pub fn current(&self) -> u64 {
        *self.rx.borrow()
    }

    /// Resolves once at least one notification arrived since the last
    /// observation on THIS handle, marking the current revision seen.
    /// Cancel-safe (usable as a `tokio::select!` branch). Returns `false`
    /// when the feed is closed for good — the connection was shut down, no
    /// change will ever fire again — so `while watcher.changed().await` is
    /// the natural consumer loop.
    pub async fn changed(&mut self) -> bool {
        self.rx.changed().await.is_ok()
    }
}

/// Manual trigger side of a [`ToolListChanged`] feed. Held by the rmcp client
/// handler inside this crate, and constructible via
/// [`tool_list_changed_channel`] by test doubles simulating a live server
/// that announces catalog changes.
#[derive(Debug, Clone)]
pub struct ToolListNotifier {
    tx: watch::Sender<u64>,
}

impl ToolListNotifier {
    /// Announce one `tools/list_changed` event. Cheap no-op (no error, no
    /// allocation) when nobody observes the feed.
    pub fn notify(&self) {
        // Always increments the revision: the counter is the payload, so an
        // equal-value write would silently drop an event for any consumer
        // counting revisions.
        self.tx.send_modify(|revision| *revision += 1);
    }
}

/// Create a connected [`ToolListNotifier`]/[`ToolListChanged`] pair — the
/// test-double counterpart of the feed [`connect_stdio`] wires up internally.
pub fn tool_list_changed_channel() -> (ToolListNotifier, ToolListChanged) {
    let (tx, rx) = watch::channel(0);
    (ToolListNotifier { tx }, ToolListChanged { rx })
}

/// A live connection to one MCP server. All methods take `&self` so a single
/// connection can be shared across the agent loop; drop alone does NOT
/// guarantee a graceful server shutdown — call [`McpConnection::shutdown`].
#[async_trait]
pub trait McpConnection: Send + Sync {
    /// Transport liveness; tool-level errors do not close a connection.
    fn is_closed(&self) -> bool {
        false
    }

    /// List every tool the server exposes, following cursor pagination to
    /// exhaustion.
    async fn list_tools(&self) -> Result<Vec<McpToolInfo>>;

    /// Call one tool by its server-side original name. `arguments` must be a
    /// JSON object (or `null` for "no arguments"); it is passed through
    /// verbatim. A tool-level failure (`isError`) is returned as
    /// [`McpCallResult::is_error`] = `true`, not as an `Err`.
    async fn call_tool(&self, name: &str, arguments: &Value) -> Result<McpCallResult>;

    /// Gracefully terminate the connection: cancel the service loop and let
    /// the stdio transport close the child's stdin, wait briefly for exit,
    /// then kill. Idempotent.
    async fn shutdown(&self);

    /// Live `tools/list_changed` feed of this connection, when the transport
    /// surfaces server-initiated notifications. The default `None` means the
    /// connection never announces catalog changes — callers fall back to
    /// whatever they cached at connect time. Each call returns an independent
    /// clone observing the same revisions.
    fn tool_list_changed(&self) -> Option<ToolListChanged> {
        None
    }

    /// List every prompt the server exposes (`prompts/list`), following
    /// cursor pagination to exhaustion. The default implementation reports
    /// "unsupported" so test doubles only override what their scenario
    /// needs; a server that offers no prompts capability answers the wire
    /// request with a JSON-RPC error, which surfaces as `Err` here.
    async fn list_prompts(&self) -> Result<Vec<McpPromptInfo>> {
        Err(anyhow!(UNSUPPORTED_PROMPTS))
    }

    /// Render one prompt by its server-side original name (`prompts/get`).
    /// `arguments` must be a JSON object of string values (or `null`); it is
    /// passed through verbatim. Only text content blocks survive — image,
    /// audio and resource blocks become placeholder notes, because oct
    /// renders prompts into the chat input as plain text.
    async fn get_prompt(&self, name: &str, arguments: &Value) -> Result<McpPromptResult> {
        let _ = (name, arguments);
        Err(anyhow!(UNSUPPORTED_PROMPTS))
    }

    /// List every resource the server exposes (`resources/list`), following
    /// cursor pagination to exhaustion. Same default-unsupported contract as
    /// [`McpConnection::list_prompts`].
    async fn list_resources(&self) -> Result<Vec<McpResourceInfo>> {
        Err(anyhow!(UNSUPPORTED_RESOURCES))
    }

    /// Read one resource by URI (`resources/read`). Text contents come back
    /// joined in [`McpResourceContent::text`]; a blob read in
    /// [`McpResourceContent::blob_base64`] verbatim base64.
    async fn read_resource(&self, uri: &str) -> Result<McpResourceContent> {
        let _ = uri;
        Err(anyhow!(UNSUPPORTED_RESOURCES))
    }
}

/// Error text for the prompts capability's default (unsupported) trait impls.
const UNSUPPORTED_PROMPTS: &str = "this connection does not support MCP prompts";

/// Error text for the resources capability's default (unsupported) trait impls.
const UNSUPPORTED_RESOURCES: &str = "this connection does not support MCP resources";

/// Spawn an MCP server as a child process and run the MCP initialize
/// handshake over stdio.
///
/// The child inherits the current process environment with `cfg.env`
/// applied on top. `timeout` bounds ONLY the spawn + handshake phase; once
/// [`connect_stdio`] returns, requests ([`McpConnection::list_tools`] /
/// [`McpConnection::call_tool`]) are unbounded — wrap them in the caller's
/// own cancellation if a server can hang.
pub async fn connect_stdio(
    cfg: &StdioServerConfig,
    timeout: Duration,
) -> Result<Box<dyn McpConnection>> {
    let mut command = tokio::process::Command::new(&cfg.command);
    command.args(&cfg.args).envs(&cfg.env);
    // Linux: arm PR_SET_PDEATHSIG so the kernel SIGTERMs the server child if
    // the backend process dies outright (SIGKILL / core dump) — drop-based
    // shutdown never runs in that scenario and the stdio server would be
    // orphaned. Best-effort: PDEATHSIG is cleared across fork(2) (hence armed
    // in pre_exec, i.e. in the forked child before exec) and silently dropped
    // for setuid/setgid binaries; it also targets the parent *thread*, so a
    // runtime worker thread exiting can fire it early. macOS and other
    // Unixes have no equivalent — deliberate no-op.
    #[cfg(all(unix, target_os = "linux"))]
    // SAFETY: the closure runs in the forked child before exec and only
    // performs async-signal-safe calls (`prctl`/`getppid` are raw syscall
    // wrappers; the ppid==1 exit path aborts before anything else runs).
    unsafe {
        command.pre_exec(set_pdeathsig);
    }
    tracing::info!(command = %cfg.command, "spawning MCP server over stdio");
    let transport = TokioChildProcess::new(command)
        .with_context(|| format!("failed to spawn MCP server process `{}`", cfg.command))?;
    connect_transport(transport, timeout).await
}

/// Connect to an MCP server over Streamable HTTP and run the MCP initialize
/// handshake.
///
/// `cfg.headers` rides on every request (see [`HttpServerConfig::headers`]
/// for the bearer contract). `timeout` bounds ONLY the connect + handshake
/// phase, exactly like [`connect_stdio`]; once this returns, requests are
/// unbounded — wrap them in the caller's own cancellation if a server can
/// hang.
pub async fn connect_http(
    cfg: &HttpServerConfig,
    timeout: Duration,
) -> Result<Box<dyn McpConnection>> {
    // Header strings are validated up front (instead of failing per-request
    // deep inside rmcp) so a config typo surfaces at connect time with the
    // offending name in the error chain.
    let mut custom_headers = HashMap::with_capacity(cfg.headers.len());
    for (name, value) in &cfg.headers {
        let name = HeaderName::from_bytes(name.as_bytes())
            .with_context(|| format!("invalid MCP HTTP header name `{name}`"))?;
        let value = HeaderValue::from_str(value)
            .with_context(|| format!("invalid MCP HTTP header value for `{name}`"))?;
        custom_headers.insert(name, value);
    }
    tracing::info!(url = %cfg.url, "connecting to MCP server over streamable http");
    let config = StreamableHttpClientTransportConfig::with_uri(cfg.url.clone())
        .custom_headers(custom_headers);
    // from_config builds rmcp's stock reqwest client (redirects disabled, so
    // caller-supplied headers can never be replayed to a redirect target) and
    // spawns the transport worker; connect_transport then drives the SAME
    // initialize handshake + NotifyHandler wiring as the stdio path.
    let transport = StreamableHttpClientTransport::from_config(config);
    connect_transport(transport, timeout).await
}

/// pre_exec hook: arm PR_SET_PDEATHSIG in the forked server child, before
/// exec. Runs between fork and exec, so only async-signal-safe calls are
/// allowed — `prctl` and `getppid` are raw syscall wrappers and qualify.
/// SIGTERM (not SIGKILL) so well-behaved servers can trap it and shut down
/// gracefully. See the call site for the full best-effort caveats.
#[cfg(all(unix, target_os = "linux"))]
fn set_pdeathsig() -> std::io::Result<()> {
    unsafe {
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
        // Race guard: if the parent died between fork and prctl, the signal
        // will never fire (PDEATHSIG latches onto the parent as of prctl
        // time). A reparented child (ppid == 1) means that already happened —
        // exit now instead of running orphaned.
        if libc::getppid() == 1 {
            std::process::exit(1);
        }
    }
    Ok(())
}

/// rmcp client handler feeding a [`ToolListChanged`] watch channel with
/// `notifications/tools/list_changed`. `get_info` returns the same identity
/// `ClientConfig` the connection always declared — the handler ONLY adds
/// notification routing; every other notification and server request keeps
/// rmcp's no-op default (oct still consumes nothing beyond tools).
struct NotifyHandler {
    config: ClientConfig,
    tool_list_changed: ToolListNotifier,
}

impl ClientHandler for NotifyHandler {
    fn on_tool_list_changed(
        &self,
        _context: NotificationContext<RoleClient>,
    ) -> impl Future<Output = ()> + MaybeSendFuture + '_ {
        self.tool_list_changed.notify();
        std::future::ready(())
    }

    fn get_info(&self) -> ClientConfig {
        self.config.clone()
    }
}

/// Core of [`connect_stdio`] and [`connect_http`], generic over the transport
/// so the tests can drive the SAME production path (initialize handshake,
/// request encoding, result mapping) over an in-memory duplex pipe instead of
/// a child process or network.
///
/// The client identifies itself as "oct" (version of this crate) and declares
/// NO capabilities beyond the base protocol: oct only ever consumes tools,
/// prompts and resources (read side). A server that nevertheless sends
/// sampling/roots/elicitation requests gets them refused by rmcp's default
/// no-op handler — except `tools/list_changed`, which [`NotifyHandler`]
/// forwards to the [`ToolListChanged`] feed carried by the returned
/// connection.
async fn connect_transport<T, E, A>(
    transport: T,
    timeout: Duration,
) -> Result<Box<dyn McpConnection>>
where
    T: IntoTransport<RoleClient, E, A>,
    E: std::error::Error + Send + Sync + 'static,
{
    let (notifier, tool_list_changed) = tool_list_changed_channel();
    let handler = NotifyHandler {
        config: ClientConfig::new(
            ClientCapabilities::default(),
            Implementation::new("oct", env!("CARGO_PKG_VERSION")),
        ),
        tool_list_changed: notifier,
    };

    // Dropping the serve_client future mid-handshake (timeout path) drops the
    // transport with it — the spawned stdio child dies, the HTTP worker task
    // is cancelled — so nothing is left behind.
    let running = tokio::time::timeout(timeout, serve_client(handler, transport))
        .await
        .map_err(|_| {
            anyhow!("MCP server did not finish the initialize handshake within {timeout:?}")
        })
        .context("failed to connect to MCP server")?
        .context("MCP initialize handshake failed")?;

    tracing::debug!("MCP connection established");
    Ok(Box::new(RmcpConnection {
        peer: running.peer().clone(),
        service: Mutex::new(Some(running)),
        tool_list_changed,
    }))
}

/// rmcp-backed [`McpConnection`]. Private on purpose: its fields are rmcp
/// types that must not leak into this crate's public API.
struct RmcpConnection {
    /// Cheap handle for issuing requests; stays usable until `shutdown`.
    peer: Peer<RoleClient>,
    /// The running service, taken and gracefully closed by `shutdown`.
    /// `Mutex<Option<..>>` because the trait's `shutdown` takes `&self`.
    service: Mutex<Option<RunningService<RoleClient, NotifyHandler>>>,
    /// Receiver half of the notification feed [`NotifyHandler`] sends to;
    /// closed for good once `shutdown` drops the service (and the handler).
    tool_list_changed: ToolListChanged,
}

#[async_trait]
impl McpConnection for RmcpConnection {
    fn is_closed(&self) -> bool {
        self.peer.is_transport_closed()
            || self
                .service
                .try_lock()
                .map(|s| s.as_ref().is_none_or(|s| s.is_closed()))
                .unwrap_or(false)
    }

    async fn list_tools(&self) -> Result<Vec<McpToolInfo>> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = self
                .peer
                .list_tools(Some(PaginatedRequestParams::default().with_cursor(cursor)))
                .await
                .context("MCP tools/list request failed")?;
            tools.extend(page.tools.into_iter().map(map_tool_info));
            // Servers paginate; an absent nextCursor means the list is done.
            // The loop has no iteration cap: a server that always returns a
            // fresh cursor hangs here, bounded only by the caller.
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        Ok(tools)
    }

    async fn call_tool(&self, name: &str, arguments: &Value) -> Result<McpCallResult> {
        // `CallToolRequestParams` is #[non_exhaustive]; build via constructor
        // and assign the optional arguments field by hand.
        let mut params = CallToolRequestParams::new(name.to_string());
        params.arguments = arguments_to_object(arguments)
            .with_context(|| format!("invalid arguments for MCP tool `{name}`"))?;
        let result = self
            .peer
            .call_tool(params)
            .await
            .with_context(|| format!("MCP tools/call for `{name}` failed"))?;
        Ok(map_call_result(&result))
    }

    async fn list_prompts(&self) -> Result<Vec<McpPromptInfo>> {
        // Same cursor-following loop as list_tools; prompts catalogs are
        // small but the protocol paginates them identically.
        let mut prompts = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = self
                .peer
                .list_prompts(Some(PaginatedRequestParams::default().with_cursor(cursor)))
                .await
                .context("MCP prompts/list request failed")?;
            prompts.extend(page.prompts.into_iter().map(map_prompt_info));
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        Ok(prompts)
    }

    async fn get_prompt(&self, name: &str, arguments: &Value) -> Result<McpPromptResult> {
        // Build params like call_tool does (non_exhaustive + optional args).
        let mut params = GetPromptRequestParams::new(name.to_string());
        params.arguments = arguments_to_object(arguments)
            .with_context(|| format!("invalid arguments for MCP prompt `{name}`"))?;
        // The `_once` (non-MRTR) request: oct supports no elicitation, so a
        // server asking for interactive input gets a refusal error instead of
        // the client silently stalling on the MRTR round driver.
        let response = self
            .peer
            .get_prompt_once(params)
            .await
            .with_context(|| format!("MCP prompts/get for `{name}` failed"))?;
        match response {
            rmcp::model::GetPromptResponse::Complete(result) => Ok(map_prompt_result(&result)),
            rmcp::model::GetPromptResponse::InputRequired(_) => Err(anyhow!(
                "MCP server requested interactive input while rendering prompt `{name}`; \
                 elicitation is not supported"
            )),
            // non_exhaustive: future variants degrade to a refusal error.
            _ => Err(anyhow!(UNSUPPORTED_PROMPTS)),
        }
    }

    async fn list_resources(&self) -> Result<Vec<McpResourceInfo>> {
        let mut resources = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = self
                .peer
                .list_resources(Some(PaginatedRequestParams::default().with_cursor(cursor)))
                .await
                .context("MCP resources/list request failed")?;
            resources.extend(page.resources.into_iter().map(map_resource_info));
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        Ok(resources)
    }

    async fn read_resource(&self, uri: &str) -> Result<McpResourceContent> {
        // `_once` for the same no-elicitation reason as get_prompt.
        let response = self
            .peer
            .read_resource_once(ReadResourceRequestParams::new(uri.to_string()))
            .await
            .with_context(|| format!("MCP resources/read for `{uri}` failed"))?;
        match response {
            rmcp::model::ReadResourceResponse::Complete(result) => {
                Ok(map_resource_content(&result))
            }
            rmcp::model::ReadResourceResponse::InputRequired(_) => Err(anyhow!(
                "MCP server requested interactive input while reading resource `{uri}`; \
                 elicitation is not supported"
            )),
            // non_exhaustive: future variants degrade to a refusal error.
            _ => Err(anyhow!(UNSUPPORTED_RESOURCES)),
        }
    }

    async fn shutdown(&self) {
        if let Some(mut service) = self.service.lock().await.take() {
            // Cancels the service loop and closes the transport: a stdio
            // child's stdin closes (wait up to 3s for exit, then kill —
            // rmcp's TokioChildProcess::graceful_shutdown); an HTTP session
            // is DELETEd server-side (bounded, see rmcp's session cleanup).
            if let Err(error) = service.close().await {
                tracing::warn!(%error, "error while closing MCP connection");
            } else {
                tracing::debug!("MCP connection closed");
            }
        }
    }

    fn tool_list_changed(&self) -> Option<ToolListChanged> {
        Some(self.tool_list_changed.clone())
    }
}

/// Map an rmcp `Tool` to the public [`McpToolInfo`].
fn map_tool_info(tool: Tool) -> McpToolInfo {
    McpToolInfo {
        name: tool.name.into_owned(),
        description: tool.description.map(|d| d.into_owned()),
        input_schema: normalize_input_schema(&Value::Object((*tool.input_schema).clone())),
        output_schema: tool.output_schema.map(|s| Value::Object((*s).clone())),
    }
}

/// Map an rmcp `Prompt` to the public [`McpPromptInfo`]: absent arguments
/// default to an empty list and an absent `required` flag to `false`.
fn map_prompt_info(prompt: Prompt) -> McpPromptInfo {
    McpPromptInfo {
        name: prompt.name,
        description: prompt.description,
        arguments: prompt
            .arguments
            .unwrap_or_default()
            .into_iter()
            .map(|arg| McpPromptArgument {
                name: arg.name,
                description: arg.description,
                required: arg.required.unwrap_or(false),
            })
            .collect(),
    }
}

/// Map an rmcp `GetPromptResult` to the public [`McpPromptResult`]. Only the
/// text of each message survives; non-text content blocks (image, audio,
/// embedded resource, resource link) become bracketed placeholder notes so
/// the rendered prompt stays honest about what was withheld.
fn map_prompt_result(result: &GetPromptResult) -> McpPromptResult {
    McpPromptResult {
        messages: result
            .messages
            .iter()
            .map(|message| McpPromptMessage {
                role: match message.role {
                    Role::User => "user".to_string(),
                    Role::Assistant => "assistant".to_string(),
                },
                text: prompt_block_text(&message.content),
            })
            .collect(),
    }
}

/// Text representation of one prompt content block: text verbatim, anything
/// else a `[...]` placeholder note naming the kind (and MIME/URI when known).
fn prompt_block_text(block: &ContentBlock) -> String {
    match block {
        ContentBlock::Text(text) => text.text.clone(),
        ContentBlock::Image(image) => format!("[image: {}]", image.mime_type),
        ContentBlock::Audio(audio) => format!("[audio: {}]", audio.mime_type),
        ContentBlock::Resource(embedded) => match &embedded.resource {
            ResourceContents::TextResourceContents { uri, .. } => {
                format!("[embedded resource: {uri}]")
            }
            ResourceContents::BlobResourceContents { uri, .. } => {
                format!("[embedded resource: {uri}]")
            }
            _ => "[embedded resource]".to_string(),
        },
        ContentBlock::ResourceLink(resource) => {
            format!("[resource link: {}]", resource.uri)
        }
        // non_exhaustive: unknown future blocks get a generic placeholder.
        _ => "[unsupported content block]".to_string(),
    }
}

/// Map an rmcp `Resource` to the public [`McpResourceInfo`].
fn map_resource_info(resource: Resource) -> McpResourceInfo {
    McpResourceInfo {
        uri: resource.uri,
        name: resource.name,
        description: resource.description,
        mime_type: resource.mime_type,
    }
}

/// Map an rmcp `ReadResourceResult` to the public [`McpResourceContent`]:
/// every text content joins into `text` (`"\n"` separated); with no text,
/// the first blob is taken verbatim into `blob_base64`; `mime_type` is the
/// first non-`None` MIME among the contents.
fn map_resource_content(result: &ReadResourceResult) -> McpResourceContent {
    let mut texts: Vec<String> = Vec::new();
    let mut blob: Option<String> = None;
    let mut mime_type: Option<String> = None;
    for content in &result.contents {
        match content {
            ResourceContents::TextResourceContents {
                uri: _,
                mime_type: mime,
                text,
                ..
            } => {
                if mime_type.is_none() {
                    mime_type = mime.clone();
                }
                texts.push(text.clone());
            }
            ResourceContents::BlobResourceContents {
                uri: _,
                mime_type: mime,
                blob: data,
                ..
            } => {
                if mime_type.is_none() {
                    mime_type = mime.clone();
                }
                if blob.is_none() {
                    blob = Some(data.clone());
                }
            }
            _ => {}
        }
    }
    McpResourceContent {
        mime_type,
        text: if texts.is_empty() {
            None
        } else {
            Some(texts.join("\n"))
        },
        blob_base64: blob,
    }
}

/// Flatten an rmcp `CallToolResult` into the public [`McpCallResult`]:
/// text blocks joined with `"\n"`, image blocks extracted into
/// [`McpCallResult::images`], `structuredContent` verbatim, `isError`
/// defaulted to `false`. See [`McpCallResult`] field docs for the rules.
fn map_call_result(result: &CallToolResult) -> McpCallResult {
    let mut parts = Vec::new();
    let mut images = Vec::new();
    for block in &result.content {
        match block {
            ContentBlock::Text(text) => parts.push(text.text.clone()),
            ContentBlock::Image(image) => images.push(McpImage {
                mime_type: image.mime_type.clone(),
                data: image.data.clone(),
            }),
            // Audio and embedded resources stay skipped: oct has no channel
            // for them in a tool result.
            _ => {}
        }
    }
    McpCallResult {
        text: parts.join("\n"),
        images,
        structured_json: result.structured_content.clone(),
        is_error: result.is_error.unwrap_or(false),
    }
}

/// Convert caller-provided tool arguments to the object rmcp expects.
/// `null` means "no arguments" (the field is omitted on the wire); a JSON
/// object passes through untouched; anything else is a caller bug.
fn arguments_to_object(arguments: &Value) -> Result<Option<Map<String, Value>>> {
    match arguments {
        Value::Null => Ok(None),
        Value::Object(map) => Ok(Some(map.clone())),
        other => Err(anyhow!(
            "MCP tool arguments must be a JSON object or null, got `{}`",
            type_of(other)
        )),
    }
}

fn type_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use serde_json::json;
    use tokio::io::AsyncBufReadExt;
    use tokio::io::AsyncWriteExt;
    use tokio::io::BufReader;
    use tokio::io::duplex;

    // ------------------------------------------------------------------
    // map_call_result (pure)
    // ------------------------------------------------------------------

    fn text_block(text: &str) -> ContentBlock {
        ContentBlock::text(text.to_string())
    }

    #[test]
    fn result_text_only_joins_with_newline() {
        let raw = CallToolResult::success(vec![text_block("hello"), text_block("world")]);
        let mapped = map_call_result(&raw);
        assert_eq!(mapped.text, "hello\nworld");
        assert_eq!(mapped.structured_json, None);
        assert!(!mapped.is_error);
        assert!(mapped.images.is_empty());
    }

    #[test]
    fn result_structured_only() {
        // Drop the text block `structured()` adds so this is truly
        // structured-only, as some servers send.
        let mut raw = CallToolResult::structured(json!({"rows": [], "count": 0}));
        raw.content = vec![];
        raw.is_error = None;
        let mapped = map_call_result(&raw);
        assert_eq!(mapped.text, "");
        assert_eq!(
            mapped.structured_json,
            Some(json!({"rows": [], "count": 0}))
        );
        assert!(!mapped.is_error);
    }

    #[test]
    fn result_mixed_text_and_structured() {
        let mut raw = CallToolResult::success(vec![text_block("summary"), text_block("details")]);
        raw.structured_content = Some(json!({"n": 2}));
        let mapped = map_call_result(&raw);
        assert_eq!(mapped.text, "summary\ndetails");
        assert_eq!(mapped.structured_json, Some(json!({"n": 2})));
        assert!(!mapped.is_error);
    }

    #[test]
    fn result_is_error_flag_is_mapped_and_defaults_to_false() {
        let error = CallToolResult::error(vec![text_block("boom")]);
        let mapped = map_call_result(&error);
        assert_eq!(mapped.text, "boom");
        assert!(mapped.is_error);
        assert_eq!(mapped.structured_json, None);

        // is_error explicitly false stays false.
        let mut ok = CallToolResult::success(vec![text_block("fine")]);
        ok.is_error = Some(false);
        assert!(!map_call_result(&ok).is_error);
    }

    #[test]
    fn result_extracts_image_blocks_and_skips_other_non_text_blocks() {
        let image = ContentBlock::image("aGVsbG8=", "image/png");
        let audio = ContentBlock::audio("aGVsbG8=", "audio/wav");
        let raw = CallToolResult::success(vec![text_block("before"), image, audio]);
        let mapped = map_call_result(&raw);
        assert_eq!(mapped.text, "before");
        assert_eq!(
            mapped.images,
            vec![McpImage {
                mime_type: "image/png".to_string(),
                data: "aGVsbG8=".to_string(),
            }]
        );
    }

    #[test]
    fn result_multiple_images_keep_server_order() {
        let raw = CallToolResult::success(vec![
            ContentBlock::image("AAAA", "image/png"),
            text_block("caption"),
            ContentBlock::image("BBBB", "image/jpeg"),
        ]);
        let mapped = map_call_result(&raw);
        assert_eq!(mapped.text, "caption");
        assert_eq!(
            mapped
                .images
                .iter()
                .map(|i| (i.mime_type.as_str(), i.data.as_str()))
                .collect::<Vec<_>>(),
            vec![("image/png", "AAAA"), ("image/jpeg", "BBBB")]
        );
    }

    #[test]
    fn result_empty_content_yields_empty_text() {
        let raw = CallToolResult::success(vec![]);
        let mapped = map_call_result(&raw);
        assert_eq!(mapped.text, "");
        assert_eq!(mapped.structured_json, None);
        assert!(!mapped.is_error);
    }

    #[test]
    fn arguments_conversion_rejects_non_objects() {
        assert!(arguments_to_object(&json!({"a": 1})).unwrap().is_some());
        assert!(arguments_to_object(&Value::Null).unwrap().is_none());
        for bad in [json!([1, 2]), json!("str"), json!(3), json!(true)] {
            assert!(arguments_to_object(&bad).is_err(), "{bad} must be rejected");
        }
    }

    // ------------------------------------------------------------------
    // Full client chain over an in-memory duplex pipe.
    //
    // rmcp ships no in-memory transport pair, so the "server" here is a
    // hand-rolled JSON-RPC peer speaking the same newline-delimited framing
    // as the stdio transport. The REAL rmcp client stack (initialize
    // handshake, request/response encoding, result decoding) runs against
    // it — no child process, no network, satisfying the CI rules.
    // ------------------------------------------------------------------

    /// Log of (method, params) the scripted server received.
    type RequestLog = Arc<std::sync::Mutex<Vec<(String, Value)>>>;

    /// Tool pages the scripted server hands out for `tools/list`.
    fn page(tools_json: Value, next_cursor: Option<&str>) -> Value {
        let mut result = json!({ "tools": tools_json });
        if let Some(cursor) = next_cursor {
            result["nextCursor"] = json!(cursor);
        }
        result
    }

    /// One `tools/call` reply for the given tool name, exercising every
    /// mapping branch of [`map_call_result`].
    fn call_reply(name: &str, arguments: Option<&Value>) -> Value {
        match name {
            "echo" => json!({
                "content": [
                    {"type": "text", "text": "hello"},
                    {"type": "text", "text": "world"},
                ],
                "structuredContent": {"sum": 3},
            }),
            "boom" => json!({
                "content": [{"type": "text", "text": "exploded"}],
                "isError": true,
            }),
            "mirror" => json!({
                "content": [
                    {"type": "text", "text": serde_json::to_string(&arguments).unwrap()},
                ],
            }),
            "shot" => json!({
                "content": [
                    {"type": "text", "text": "chart rendered"},
                    {"type": "image", "data": "aVBuZw==", "mimeType": "image/png"},
                    {"type": "image", "data": "aGVpZw==", "mimeType": "image/jpeg"},
                    {"type": "audio", "data": "YW==", "mimeType": "audio/wav"},
                ],
            }),
            other => json!({
                "error": {"code": -32602, "message": format!("unknown tool {other}")},
            }),
        }
    }

    /// One `prompts/get` reply, echoing the rendered arguments into the text
    /// and mixing non-text content blocks (image, embedded resource) so the
    /// placeholder mapping is exercised on the wire path.
    fn prompt_get_reply(name: &str, arguments: Option<&Value>) -> Value {
        let name_arg = arguments
            .and_then(|args| args.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("stranger");
        match name {
            "greet" => json!({
                "description": "greet someone",
                "messages": [
                    {"role": "user", "content": {"type": "text", "text": format!("Greet {name_arg}")}},
                    {"role": "assistant", "content": {"type": "image", "data": "aVBuZw==", "mimeType": "image/png"}},
                ],
            }),
            "review" => json!({
                "messages": [
                    {"role": "user", "content": {"type": "text", "text": "Review the diff"}},
                    {"role": "assistant", "content": {"type": "text", "text": "Here is my review"}},
                ],
            }),
            other => json!({
                "error": {"code": -32602, "message": format!("unknown prompt {other}")},
            }),
        }
    }

    /// One `resources/read` reply keyed by URI: text contents (two blocks, to
    /// exercise the join) or a single blob.
    fn resource_read_reply(uri: &str) -> Value {
        match uri {
            "memo://notes" => json!({
                "contents": [
                    {"uri": uri, "mimeType": "text/markdown", "text": "# Notes"},
                    {"uri": uri, "mimeType": "text/markdown", "text": "second block"},
                ],
            }),
            "file://logo.png" => json!({
                "contents": [
                    {"uri": uri, "mimeType": "image/png", "blob": "aVBuZw=="},
                ],
            }),
            other => json!({
                "error": {"code": -32602, "message": format!("unknown resource {other}")},
            }),
        }
    }

    /// Run the scripted server until the client side closes the pipe (read
    /// returns an empty line at EOF). Every incoming message is recorded in
    /// `log`. When `announce_tools_changed` is set, the server pushes two
    /// `notifications/tools/list_changed` frames right after the initialize
    /// response — mirroring a real server whose tool catalog moved twice
    /// before the client asked anything.
    async fn run_scripted_server(
        stream: tokio::io::DuplexStream,
        log: RequestLog,
        announce_tools_changed: bool,
    ) -> std::io::Result<()> {
        let (read_half, mut write_half) = tokio::io::split(stream);
        let mut lines = BufReader::new(read_half).lines();
        while let Some(line) = lines.next_line().await? {
            if line.is_empty() {
                continue;
            }
            let message: Value = match serde_json::from_str(&line) {
                Ok(value) => value,
                Err(error) => return Err(std::io::Error::other(format!("bad JSON: {error}"))),
            };
            let method = message["method"].as_str().unwrap_or_default().to_string();
            log.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push((method.clone(), message["params"].clone()));

            let Some(id) = message.get("id").cloned() else {
                // Notification (notifications/initialized): no reply.
                continue;
            };
            let result = match method.as_str() {
                "initialize" => {
                    // Echo the requested protocol version so negotiation
                    // succeeds; declare no server capabilities beyond tools.
                    json!({
                        "protocolVersion": message["params"]["protocolVersion"],
                        "capabilities": {},
                        "serverInfo": {"name": "scripted", "version": "0.0.0"},
                    })
                }
                "tools/list" => match message["params"]["cursor"].as_str() {
                    None => page(
                        json!([
                            {
                                "name": "echo",
                                "description": "say things",
                                // type missing on purpose: normalize must add it.
                                "inputSchema": {"properties": {"a": {"type": "number"}}},
                            },
                            {
                                "name": "boom",
                                // No description; a minimal-but-present schema
                                // (`inputSchema` is a required field on the wire).
                                "inputSchema": {"type": "object"},
                            },
                        ]),
                        Some("page2"),
                    ),
                    Some("page2") => page(
                        json!([{ "name": "mirror", "inputSchema": {"type": "object"} }]),
                        None,
                    ),
                    Some(other) => {
                        return Err(std::io::Error::other(format!("unexpected cursor {other}")));
                    }
                },
                "tools/call" => {
                    let name = message["params"]["name"].as_str().unwrap_or_default();
                    call_reply(name, message["params"].get("arguments"))
                }
                "prompts/list" => match message["params"]["cursor"].as_str() {
                    None => json!({
                        "prompts": [
                            {
                                "name": "greet",
                                "description": "greet someone by name",
                                "arguments": [
                                    {
                                        "name": "name",
                                        "description": "who to greet",
                                        "required": true,
                                    },
                                    // required omitted on purpose: must default to false.
                                    {"name": "style"},
                                ],
                            },
                        ],
                        "nextCursor": "prompts2",
                    }),
                    Some("prompts2") => json!({
                        "prompts": [{"name": "review", "description": "review the diff"}],
                    }),
                    Some(other) => {
                        return Err(std::io::Error::other(format!(
                            "unexpected prompts cursor {other}"
                        )));
                    }
                },
                "prompts/get" => {
                    let name = message["params"]["name"].as_str().unwrap_or_default();
                    prompt_get_reply(name, message["params"].get("arguments"))
                }
                "resources/list" => json!({
                    "resources": [
                        {
                            "uri": "memo://notes",
                            "name": "notes",
                            "description": "meeting notes",
                            "mimeType": "text/markdown",
                        },
                        // mimeType omitted on purpose: must map to None.
                        {"uri": "file://logo.png", "name": "logo"},
                    ],
                }),
                "resources/read" => {
                    let uri = message["params"]["uri"].as_str().unwrap_or_default();
                    resource_read_reply(uri)
                }
                other => {
                    json!({"error": {"code": -32601, "message": format!("no handler for {other}")}})
                }
            };
            let response = json!({"jsonrpc": "2.0", "id": id, "result": result});
            write_half
                .write_all(format!("{}\n", response.to_string()).as_bytes())
                .await?;
            if method == "initialize" && announce_tools_changed {
                for _ in 0..2 {
                    let notification =
                        json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"});
                    write_half
                        .write_all(format!("{}\n", notification).as_bytes())
                        .await?;
                }
            }
        }
        Ok(())
    }

    /// Wire a client (via the production [`connect_transport`]) to a scripted
    /// server over a duplex pipe; returns (connection, request log).
    async fn connected_client() -> (Box<dyn McpConnection>, RequestLog) {
        connected_client_with(false).await
    }

    /// [`connected_client`] with control over whether the scripted server
    /// announces `tools/list_changed` right after the handshake.
    async fn connected_client_with(
        announce_tools_changed: bool,
    ) -> (Box<dyn McpConnection>, RequestLog) {
        let (client_side, server_side) = duplex(64 * 1024);
        let log: RequestLog = Arc::default();
        let server_log = log.clone();
        let server = tokio::spawn(async move {
            run_scripted_server(server_side, server_log, announce_tools_changed).await
        });

        let connection = connect_transport(client_side, Duration::from_secs(10))
            .await
            .expect("connect over duplex must succeed");

        // Make the server task's failure visible to the test: it is joined in
        // `teardown`, so a panic-free shutdown is asserted implicitly.
        tokio::spawn(async move {
            if let Err(error) = server.await.expect("server task must not panic") {
                panic!("scripted server failed: {error}");
            }
        });

        (connection, log)
    }

    /// Turn a failed connection attempt into its error, panicking if it
    /// unexpectedly succeeded (`Box<dyn McpConnection>` is not `Debug`, so
    /// `expect_err` is not usable here).
    fn expect_connection_err(
        result: Result<Box<dyn McpConnection>>,
        message: &str,
    ) -> anyhow::Error {
        match result {
            Ok(_) => panic!("{message}"),
            Err(error) => error,
        }
    }

    fn logged(log: &RequestLog, method: &str) -> Vec<Value> {
        log.lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .filter(|(m, _)| m == method)
            .map(|(_, params)| params.clone())
            .collect()
    }

    #[tokio::test]
    async fn handshake_lists_paginates_and_calls_over_the_wire() {
        let (connection, log) = connected_client().await;

        // The initialize request must have been sent with our identity and
        // no extra capabilities.
        let inits = logged(&log, "initialize");
        assert_eq!(inits.len(), 1);
        assert_eq!(inits[0]["clientInfo"]["name"], "oct");
        assert_eq!(inits[0]["clientInfo"]["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(inits[0]["capabilities"], json!({}));

        // list_tools follows pagination to exhaustion: page1 (echo, boom) +
        // page2 (mirror), and both cursored requests hit the wire.
        let tools = connection.list_tools().await.expect("list_tools");
        assert_eq!(
            tools,
            vec![
                McpToolInfo {
                    output_schema: None,
                    name: "echo".to_string(),
                    description: Some("say things".to_string()),
                    input_schema: json!({
                        "properties": {"a": {"type": "number"}},
                        "type": "object",
                    }),
                },
                McpToolInfo {
                    output_schema: None,
                    name: "boom".to_string(),
                    description: None,
                    input_schema: json!({"type": "object", "properties": {}}),
                },
                McpToolInfo {
                    output_schema: None,
                    name: "mirror".to_string(),
                    description: None,
                    input_schema: json!({"type": "object", "properties": {}}),
                },
            ]
        );
        let lists = logged(&log, "tools/list");
        assert_eq!(lists.len(), 2, "both pages must have been requested");
        assert_eq!(lists[0]["cursor"], Value::Null);
        assert_eq!(lists[1]["cursor"], json!("page2"));

        // Text + structured + arguments passthrough.
        let echo = connection
            .call_tool("echo", &json!({"a": 1}))
            .await
            .expect("echo call");
        assert_eq!(echo.text, "hello\nworld");
        assert_eq!(echo.structured_json, Some(json!({"sum": 3})));
        assert!(!echo.is_error);
        let calls = logged(&log, "tools/call");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["name"], "echo");
        assert_eq!(
            calls[0]["arguments"],
            json!({"a": 1}),
            "arguments must pass through verbatim"
        );

        // Tool-level error is a successful transport round trip.
        let boom = connection
            .call_tool("boom", &Value::Null)
            .await
            .expect("boom call");
        assert_eq!(boom.text, "exploded");
        assert_eq!(boom.structured_json, None);
        assert!(boom.is_error);
        // Null arguments are omitted from the wire message entirely.
        let calls = logged(&log, "tools/call");
        assert_eq!(calls[1].get("arguments"), None);

        // mirror echoes its received arguments back as text.
        let mirror = connection
            .call_tool("mirror", &json!({"deep": {"x": [1, "两"]}}))
            .await
            .expect("mirror call");
        assert_eq!(mirror.text, r#"{"deep":{"x":[1,"两"]}}"#);

        // Image content blocks survive the round trip as base64+MIME pairs
        // in server order; audio blocks are skipped.
        let shot = connection
            .call_tool("shot", &Value::Null)
            .await
            .expect("shot call");
        assert_eq!(shot.text, "chart rendered");
        assert_eq!(
            shot.images,
            vec![
                McpImage {
                    mime_type: "image/png".to_string(),
                    data: "aVBuZw==".to_string(),
                },
                McpImage {
                    mime_type: "image/jpeg".to_string(),
                    data: "aGVpZw==".to_string(),
                },
            ]
        );
        assert!(!shot.is_error);

        connection.shutdown().await;
    }

    #[tokio::test]
    async fn prompts_list_paginates_and_get_renders_over_the_wire() {
        let (connection, log) = connected_client().await;

        // prompts/list follows pagination to exhaustion and maps arguments
        // (absent `required` defaults to false, absent list to empty).
        let prompts = connection.list_prompts().await.expect("list_prompts");
        assert_eq!(
            prompts,
            vec![
                McpPromptInfo {
                    name: "greet".to_string(),
                    description: Some("greet someone by name".to_string()),
                    arguments: vec![
                        McpPromptArgument {
                            name: "name".to_string(),
                            description: Some("who to greet".to_string()),
                            required: true,
                        },
                        McpPromptArgument {
                            name: "style".to_string(),
                            description: None,
                            required: false,
                        },
                    ],
                },
                McpPromptInfo {
                    name: "review".to_string(),
                    description: Some("review the diff".to_string()),
                    arguments: Vec::new(),
                },
            ]
        );
        let lists = logged(&log, "prompts/list");
        assert_eq!(lists.len(), 2, "both prompt pages must have been requested");
        assert_eq!(lists[1]["cursor"], json!("prompts2"));

        // prompts/get passes arguments verbatim and maps every message:
        // text survives, an image block becomes a bracketed placeholder.
        let rendered = connection
            .get_prompt("greet", &json!({"name": "Ada"}))
            .await
            .expect("get_prompt greet");
        assert_eq!(
            rendered.messages,
            vec![
                McpPromptMessage {
                    role: "user".to_string(),
                    text: "Greet Ada".to_string(),
                },
                McpPromptMessage {
                    role: "assistant".to_string(),
                    text: "[image: image/png]".to_string(),
                },
            ]
        );
        let gets = logged(&log, "prompts/get");
        assert_eq!(gets.len(), 1);
        assert_eq!(gets[0]["name"], "greet");
        assert_eq!(gets[0]["arguments"], json!({"name": "Ada"}));

        // Null arguments are omitted from the wire message entirely.
        let review = connection
            .get_prompt("review", &Value::Null)
            .await
            .expect("get_prompt review");
        assert_eq!(review.messages.len(), 2);
        assert_eq!(review.messages[0].role, "user");
        assert_eq!(review.messages[1].role, "assistant");
        assert_eq!(
            logged(&log, "prompts/get")[1].get("arguments"),
            None,
            "null arguments must be omitted"
        );

        // An unknown prompt is a transport-level JSON-RPC error (rmcp wraps
        // the wire error; the context names the request).
        let error = connection
            .get_prompt("nope", &Value::Null)
            .await
            .expect_err("unknown prompt must fail");
        assert!(
            format!("{error:#}").contains("prompts/get"),
            "error chain should name the request: {error:#}"
        );

        connection.shutdown().await;
    }

    #[tokio::test]
    async fn resources_list_and_read_over_the_wire() {
        let (connection, _log) = connected_client().await;

        let resources = connection.list_resources().await.expect("list_resources");
        assert_eq!(
            resources,
            vec![
                McpResourceInfo {
                    uri: "memo://notes".to_string(),
                    name: "notes".to_string(),
                    description: Some("meeting notes".to_string()),
                    mime_type: Some("text/markdown".to_string()),
                },
                McpResourceInfo {
                    uri: "file://logo.png".to_string(),
                    name: "logo".to_string(),
                    description: None,
                    mime_type: None,
                },
            ]
        );

        // Text read: blocks join with newline, MIME carried through.
        let text = connection
            .read_resource("memo://notes")
            .await
            .expect("read text resource");
        assert_eq!(
            text,
            McpResourceContent {
                mime_type: Some("text/markdown".to_string()),
                text: Some("# Notes\nsecond block".to_string()),
                blob_base64: None,
            }
        );

        // Blob read: base64 verbatim, no text field.
        let blob = connection
            .read_resource("file://logo.png")
            .await
            .expect("read blob resource");
        assert_eq!(
            blob,
            McpResourceContent {
                mime_type: Some("image/png".to_string()),
                text: None,
                blob_base64: Some("aVBuZw==".to_string()),
            }
        );

        // Unknown URI is a transport-level error.
        assert!(connection.read_resource("nope://x").await.is_err());

        connection.shutdown().await;
    }

    /// A connection implementing only the required methods: the prompts and
    /// resources defaults must report "unsupported" instead of panicking or
    /// silently succeeding, so test doubles can opt in per capability.
    struct BareConn;

    #[async_trait]
    impl McpConnection for BareConn {
        async fn list_tools(&self) -> Result<Vec<McpToolInfo>> {
            Ok(Vec::new())
        }
        async fn call_tool(&self, _name: &str, _arguments: &Value) -> Result<McpCallResult> {
            Ok(McpCallResult {
                text: String::new(),
                images: Vec::new(),
                structured_json: None,
                is_error: false,
            })
        }
        async fn shutdown(&self) {}
    }

    #[tokio::test]
    async fn default_prompts_and_resources_impls_report_unsupported() {
        let connection = BareConn;
        for error in [
            connection.list_prompts().await.expect_err("prompts"),
            connection
                .get_prompt("x", &Value::Null)
                .await
                .expect_err("get"),
            connection.list_resources().await.expect_err("resources"),
            connection.read_resource("x://y").await.expect_err("read"),
        ] {
            assert!(
                format!("{error}").contains("does not support"),
                "error should say unsupported: {error}"
            );
        }
    }

    /// Wait until the feed observed at least revision `target` (recv with
    /// timeout 兜底, no sleeps). Watch semantics coalesce a burst into one
    /// `changed()` wake carrying the LATEST revision, so the loop must follow
    /// the revision counter instead of assuming one wake per notification:
    /// when `changed()` resolves the revision may already be past `target`,
    /// and when fewer remain in flight the next `changed()` would hang.
    async fn wait_for_revision(watcher: &mut ToolListChanged, target: u64) {
        loop {
            if watcher.current() >= target {
                return;
            }
            let changed = tokio::time::timeout(Duration::from_secs(5), watcher.changed())
                .await
                .expect("notification must be delivered");
            assert!(changed, "feed must still be alive");
        }
    }

    #[tokio::test]
    async fn tools_list_changed_notifications_reach_the_exposed_feed() {
        let (connection, _log) = connected_client_with(true).await;
        let mut watcher = connection
            .tool_list_changed()
            .expect("stdio connections must expose the notification feed");

        // Two notifications were pushed right after the handshake: both must
        // land on the feed (revision == 2 proves neither was dropped — and
        // the wake may carry them in one coalesced `changed()`).
        wait_for_revision(&mut watcher, 2).await;
        assert_eq!(watcher.current(), 2);

        // The notifications must not disturb request handling on the same
        // stream: list/call work exactly as without them.
        let tools = connection.list_tools().await.expect("list_tools");
        assert_eq!(tools.len(), 3);
        let echo = connection
            .call_tool("echo", &json!({"a": 1}))
            .await
            .expect("echo call");
        assert_eq!(echo.text, "hello\nworld");

        // And the feed must stay quiet afterwards: a short bounded wait on
        // `changed()` (the timeout IS the assertion) proves no spurious
        // signals leak through.
        assert!(
            tokio::time::timeout(Duration::from_millis(100), watcher.changed())
                .await
                .is_err(),
            "no further notification may fire"
        );

        connection.shutdown().await;
    }

    #[tokio::test]
    async fn feed_closes_when_the_connection_shuts_down() {
        let (connection, _log) = connected_client_with(true).await;
        let mut watcher = connection
            .tool_list_changed()
            .expect("stdio connections must expose the notification feed");
        // Settle both scripted notifications so the next `changed()` has
        // nothing pending; then shutdown must close the feed for good.
        wait_for_revision(&mut watcher, 2).await;
        connection.shutdown().await;
        let closed = tokio::time::timeout(Duration::from_secs(5), watcher.changed())
            .await
            .expect("changed() must resolve after the feed closes");
        assert!(!closed, "feed must report closed after shutdown");
    }

    #[tokio::test]
    async fn transport_eof_is_detected_without_local_shutdown() {
        let (client_side, server_side) = duplex(64 * 1024);
        let server = tokio::spawn(run_scripted_server(server_side, Arc::default(), false));
        let connection = connect_transport(client_side, Duration::from_secs(10))
            .await
            .unwrap();
        assert!(!connection.is_closed());
        server.abort();
        let _ = server.await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while !connection.is_closed() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("remote EOF must invalidate the cached connection");
        connection.shutdown().await;
    }

    #[tokio::test]
    async fn call_after_shutdown_fails() {
        let (connection, _log) = connected_client().await;
        connection.shutdown().await;
        // Shutdown is idempotent...
        connection.shutdown().await;
        // ...and the peer is dead afterwards.
        assert!(connection.list_tools().await.is_err());
    }

    #[tokio::test]
    async fn non_object_arguments_are_rejected_before_hitting_the_wire() {
        let (connection, log) = connected_client().await;
        let error = connection
            .call_tool("echo", &json!([1, 2]))
            .await
            .expect_err("array arguments must be rejected");
        assert!(
            format!("{error:#}").contains("JSON object"),
            "error chain should demand a JSON object: {error:#}"
        );
        assert!(
            logged(&log, "tools/call").is_empty(),
            "no tools/call may be sent for invalid arguments"
        );
        connection.shutdown().await;
    }

    #[tokio::test]
    async fn handshake_timeout_fails_the_connect() {
        let (client_side, server_side) = duplex(1024);
        // A server that reads but never answers initialize.
        tokio::spawn(async move {
            let (_read_half, _write_half) =
                tokio::io::split::<tokio::io::DuplexStream>(server_side);
            // Hold the halves until the client side is dropped; the timeout
            // under test is the only clock involved.
            tokio::time::sleep(Duration::from_secs(30)).await;
        });

        let error = expect_connection_err(
            connect_transport(client_side, Duration::from_millis(100)).await,
            "a silent server must trip the handshake timeout",
        );
        assert!(
            format!("{error:#}").contains("handshake"),
            "error chain should mention the handshake: {error:#}"
        );
    }

    #[tokio::test]
    async fn connect_stdio_reports_missing_binary() {
        let cfg = StdioServerConfig {
            command: "/nonexistent/oct-mcp-test-server".to_string(),
            args: vec![],
            env: HashMap::new(),
        };
        let error = expect_connection_err(
            connect_stdio(&cfg, Duration::from_secs(5)).await,
            "spawning a missing binary must fail",
        );
        assert!(
            error
                .to_string()
                .contains("/nonexistent/oct-mcp-test-server"),
            "error should name the command: {error}"
        );
    }
}
