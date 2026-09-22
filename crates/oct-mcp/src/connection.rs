//! stdio MCP client connections.
//!
//! This module wraps rmcp's client stack behind the narrow [`McpConnection`]
//! trait. rmcp types (`RunningService`, `Peer`, model structs) appear ONLY in
//! private items — never in a public signature — so consumers of this crate
//! can treat MCP as "spawn a server, list tools, call tools, shut down"
//! without a direct rmcp dependency.
//!
//! Only the child-process stdio transport is supported (see the crate-level
//! docs for why HTTP transports are deliberately out).

use std::collections::HashMap;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use async_trait::async_trait;
use rmcp::model::CallToolRequestParams;
use rmcp::model::CallToolResult;
use rmcp::model::ClientCapabilities;
use rmcp::model::ClientConfig;
use rmcp::model::ContentBlock;
use rmcp::model::Implementation;
use rmcp::model::PaginatedRequestParams;
use rmcp::model::Tool;
use rmcp::service::Peer;
use rmcp::service::RoleClient;
use rmcp::service::RunningService;
use rmcp::service::serve_client;
use rmcp::transport::IntoTransport;
use rmcp::transport::child_process::TokioChildProcess;
use serde_json::Map;
use serde_json::Value;
use tokio::sync::Mutex;

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
}

/// The outcome of one tool call, flattened from MCP's content-block model.
#[derive(Debug, Clone, PartialEq)]
pub struct McpCallResult {
    /// All text content blocks joined with `"\n"`, in server order.
    /// Non-text blocks (images, audio, resources) are skipped — oct has no
    /// way to feed them to a text-only tool result today.
    pub text: String,
    /// The server's `structuredContent` verbatim, when present.
    pub structured_json: Option<Value>,
    /// The server's `isError` flag (tool-level failure), defaulting to
    /// `false` when absent.
    pub is_error: bool,
}

/// A live connection to one MCP server. All methods take `&self` so a single
/// connection can be shared across the agent loop; drop alone does NOT
/// guarantee a graceful server shutdown — call [`McpConnection::shutdown`].
#[async_trait]
pub trait McpConnection: Send + Sync {
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
}

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

/// Core of [`connect_stdio`], generic over the transport so the tests can
/// drive the SAME production path (initialize handshake, request encoding,
/// result mapping) over an in-memory duplex pipe instead of a child process.
///
/// The client identifies itself as "oct" (version of this crate) and declares
/// NO capabilities beyond the base protocol: oct only ever consumes tools.
/// A server that nevertheless sends sampling/roots/elicitation requests gets
/// them refused by rmcp's default no-op handler.
async fn connect_transport<T, E, A>(
    transport: T,
    timeout: Duration,
) -> Result<Box<dyn McpConnection>>
where
    T: IntoTransport<RoleClient, E, A>,
    E: std::error::Error + Send + Sync + 'static,
{
    let client_info = ClientConfig::new(
        ClientCapabilities::default(),
        Implementation::new("oct", env!("CARGO_PKG_VERSION")),
    );

    // Dropping the serve_client future mid-handshake (timeout path) drops the
    // transport with it, which kills the spawned child — no orphan process
    // is left behind.
    let running = tokio::time::timeout(timeout, serve_client(client_info, transport))
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
    }))
}

/// rmcp-backed [`McpConnection`]. Private on purpose: its fields are rmcp
/// types that must not leak into this crate's public API.
struct RmcpConnection {
    /// Cheap handle for issuing requests; stays usable until `shutdown`.
    peer: Peer<RoleClient>,
    /// The running service, taken and gracefully closed by `shutdown`.
    /// `Mutex<Option<..>>` because the trait's `shutdown` takes `&self`.
    service: Mutex<Option<RunningService<RoleClient, ClientConfig>>>,
}

#[async_trait]
impl McpConnection for RmcpConnection {
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

    async fn shutdown(&self) {
        if let Some(mut service) = self.service.lock().await.take() {
            // Cancels the service loop and closes the transport; for a stdio
            // child that closes its stdin, waits up to 3s for exit, then
            // kills — rmcp's TokioChildProcess::graceful_shutdown.
            if let Err(error) = service.close().await {
                tracing::warn!(%error, "error while closing MCP connection");
            } else {
                tracing::debug!("MCP connection closed");
            }
        }
    }
}

/// Map an rmcp `Tool` to the public [`McpToolInfo`].
fn map_tool_info(tool: Tool) -> McpToolInfo {
    McpToolInfo {
        name: tool.name.into_owned(),
        description: tool.description.map(|d| d.into_owned()),
        input_schema: normalize_input_schema(&Value::Object((*tool.input_schema).clone())),
    }
}

/// Flatten an rmcp `CallToolResult` into the public [`McpCallResult`]:
/// text blocks joined with `"\n"`, `structuredContent` verbatim, `isError`
/// defaulted to `false`. See [`McpCallResult`] field docs for the rules.
fn map_call_result(result: &CallToolResult) -> McpCallResult {
    let mut parts = Vec::new();
    for block in &result.content {
        if let ContentBlock::Text(text) = block {
            parts.push(text.text.clone());
        }
    }
    McpCallResult {
        text: parts.join("\n"),
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
    fn result_skips_non_text_blocks() {
        let image = ContentBlock::image("aGVsbG8=", "image/png");
        let audio = ContentBlock::audio("aGVsbG8=", "audio/wav");
        let raw = CallToolResult::success(vec![text_block("before"), image, audio]);
        assert_eq!(map_call_result(&raw).text, "before");
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
            other => json!({
                "error": {"code": -32602, "message": format!("unknown tool {other}")},
            }),
        }
    }

    /// Run the scripted server until the client side closes the pipe (read
    /// returns an empty line at EOF). Every incoming message is recorded in
    /// `log`.
    async fn run_scripted_server(
        stream: tokio::io::DuplexStream,
        log: RequestLog,
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
                other => {
                    json!({"error": {"code": -32601, "message": format!("no handler for {other}")}})
                }
            };
            let response = json!({"jsonrpc": "2.0", "id": id, "result": result});
            write_half
                .write_all(format!("{}\n", response.to_string()).as_bytes())
                .await?;
        }
        Ok(())
    }

    /// Wire a client (via the production [`connect_transport`]) to a scripted
    /// server over a duplex pipe; returns (connection, request log).
    async fn connected_client() -> (Box<dyn McpConnection>, RequestLog) {
        let (client_side, server_side) = duplex(64 * 1024);
        let log: RequestLog = Arc::default();
        let server_log = log.clone();
        let server =
            tokio::spawn(async move { run_scripted_server(server_side, server_log).await });

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
                    name: "echo".to_string(),
                    description: Some("say things".to_string()),
                    input_schema: json!({
                        "properties": {"a": {"type": "number"}},
                        "type": "object",
                    }),
                },
                McpToolInfo {
                    name: "boom".to_string(),
                    description: None,
                    input_schema: json!({"type": "object", "properties": {}}),
                },
                McpToolInfo {
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
