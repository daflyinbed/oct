//! `connect_http` end-to-end against an in-process Streamable HTTP server.
//!
//! The mock speaks the server half of Streamable HTTP with plain JSON
//! responses (SSE is optional in the protocol; rmcp handles JSON-only
//! servers and skips the standalone GET stream when the server answers 405).
//! The REAL rmcp client stack — reqwest transport, custom-header injection,
//! session-id handling, initialize handshake, request encoding, result
//! decoding — runs against a real localhost TCP socket. No child process, no
//! external network, no real model: the CI rules hold.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use axum::Json as AxumJson;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::post;
use serde_json::Value;
use serde_json::json;
use tokio::net::TcpListener;

use oct_mcp::HttpServerConfig;
use oct_mcp::McpConnection;
use oct_mcp::connect_http;

/// Bearer value the mock demands on EVERY request (handshake included).
const TOKEN: &str = "Bearer oct-test-token";

/// Session id the mock hands out on initialize and expects back afterwards.
const SESSION_ID: &str = "sess-42";

/// What the mock recorded about one incoming request.
#[derive(Debug, Clone)]
struct RecordedRequest {
    method: String,
    params: Value,
    authorization: Option<String>,
    custom_header: Option<String>,
    session_id: Option<String>,
}

#[derive(Default)]
struct MockState {
    requests: Vec<RecordedRequest>,
    deleted_sessions: Vec<String>,
}

type Shared = Arc<Mutex<MockState>>;

/// Boot the mock on an ephemeral localhost port; returns its shared log and
/// the base URL to reach `/mcp` at. The server task lives until the test
/// process ends (tests never shut it down explicitly).
async fn start_mock() -> (Shared, String) {
    let shared: Shared = Arc::default();
    let app = axum::Router::new()
        .route("/mcp", post(mcp_post).get(mcp_get).delete(mcp_delete))
        .with_state(shared.clone());
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("mock server run");
    });
    (shared, format!("http://{addr}/mcp"))
}

/// POST /mcp: the JSON-RPC heart of the mock. Every request must carry the
/// bearer header; notifications are acknowledged with 202, requests get a
/// JSON-RPC response, and initialize additionally assigns the session id.
async fn mcp_post(
    State(shared): State<Shared>,
    headers: HeaderMap,
    AxumJson(message): AxumJson<Value>,
) -> Response {
    let method = message["method"].as_str().unwrap_or_default().to_string();
    let record = RecordedRequest {
        method: method.clone(),
        params: message["params"].clone(),
        authorization: headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string),
        custom_header: headers
            .get("x-oct-test")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string),
        session_id: headers
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string),
    };
    shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .requests
        .push(record);

    if headers.get("authorization").and_then(|v| v.to_str().ok()) != Some(TOKEN) {
        // No WWW-Authenticate on purpose: the plain-status branch in rmcp's
        // reqwest client is what these tests exercise.
        return (StatusCode::UNAUTHORIZED, "missing or wrong bearer token").into_response();
    }

    // Notifications (no id) are acknowledged without a body.
    let Some(id) = message.get("id").cloned() else {
        return StatusCode::ACCEPTED.into_response();
    };

    let body = match method.as_str() {
        "initialize" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                // Echo the requested version so negotiation succeeds.
                "protocolVersion": message["params"]["protocolVersion"],
                "capabilities": {},
                "serverInfo": {"name": "http-scripted", "version": "0.0.0"},
            },
        }),
        "tools/list" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {"tools": [
                {
                    "name": "echo",
                    "description": "say things",
                    "inputSchema": {"type": "object", "properties": {"a": {"type": "number"}}},
                },
            ]},
        }),
        "tools/call" => {
            let name = message["params"]["name"].as_str().unwrap_or_default();
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "content": [{"type": "text", "text": format!("echo:{name}")}],
                    "structuredContent": {"sum": 3},
                },
            })
        }
        other => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32601, "message": format!("no handler for {other}")},
        }),
    };
    let mut response = AxumJson(body).into_response();
    if method == "initialize" {
        response
            .headers_mut()
            .insert("mcp-session-id", SESSION_ID.parse().expect("static header"));
    }
    response
}

/// GET /mcp: this mock offers no standalone SSE stream. 405 makes rmcp's
/// client log "server doesn't support sse" and skip the stream — the
/// documented way for a JSON-only server.
async fn mcp_get() -> StatusCode {
    StatusCode::METHOD_NOT_ALLOWED
}

/// DELETE /mcp: session teardown on graceful shutdown. Records the session
/// id so the shutdown test can assert the transport really deleted it.
async fn mcp_delete(State(shared): State<Shared>, headers: HeaderMap) -> StatusCode {
    let session = headers
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    if let Some(session) = session {
        shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .deleted_sessions
            .push(session);
    }
    StatusCode::OK
}

/// Turn a failed connection attempt into its error, panicking if it
/// unexpectedly succeeded (`Box<dyn McpConnection>` is not `Debug`, so
/// `expect_err` is not usable here).
fn expect_connection_err(
    result: Result<Box<dyn McpConnection>, anyhow::Error>,
    message: &str,
) -> anyhow::Error {
    match result {
        Ok(_) => panic!("{message}"),
        Err(error) => error,
    }
}

fn http_config(url: &str) -> HttpServerConfig {
    HttpServerConfig {
        url: url.to_string(),
        headers: HashMap::from([
            ("Authorization".to_string(), TOKEN.to_string()),
            ("x-oct-test".to_string(), "custom-value".to_string()),
        ]),
    }
}

#[tokio::test]
async fn handshake_lists_and_calls_tools_over_streamable_http() {
    let (shared, url) = start_mock().await;

    let connection = connect_http(&http_config(&url), Duration::from_secs(10))
        .await
        .expect("connect");
    let tools = connection.list_tools().await.expect("list_tools");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "echo");
    assert_eq!(
        tools[0].input_schema,
        json!({"type": "object", "properties": {"a": {"type": "number"}}}),
    );

    let echo = connection
        .call_tool("echo", &json!({"a": 1}))
        .await
        .expect("call_tool");
    assert_eq!(echo.text, "echo:echo");
    assert_eq!(echo.structured_json, Some(json!({"sum": 3})));
    assert!(!echo.is_error);

    connection.shutdown().await;

    let state = shared.lock().unwrap_or_else(|p| p.into_inner());
    let methods: Vec<&str> = state.requests.iter().map(|r| r.method.as_str()).collect();
    assert!(methods.contains(&"initialize"), "saw {methods:?}");
    assert!(methods.contains(&"tools/list"));
    assert!(methods.contains(&"tools/call"));
    // The auth + custom headers must ride on EVERY request — the initialize
    // POST included (that is the whole point of the header seam).
    for request in &state.requests {
        assert_eq!(
            request.authorization.as_deref(),
            Some(TOKEN),
            "request `{}` missed the bearer header",
            request.method
        );
        assert_eq!(request.custom_header.as_deref(), Some("custom-value"));
    }
    // After initialize the mock-assigned session id must come back on every
    // subsequent POST, and graceful shutdown must DELETE exactly that
    // session.
    let post_init: Vec<&RecordedRequest> = state
        .requests
        .iter()
        .filter(|r| r.method != "initialize")
        .collect();
    assert!(!post_init.is_empty());
    for request in &post_init {
        assert_eq!(
            request.session_id.as_deref(),
            Some(SESSION_ID),
            "request `{}` missed the session header",
            request.method
        );
    }
    assert_eq!(state.deleted_sessions, vec![SESSION_ID.to_string()]);
}

#[tokio::test]
async fn missing_bearer_token_fails_the_handshake() {
    let (shared, url) = start_mock().await;

    let config = HttpServerConfig {
        url,
        headers: HashMap::new(),
    };
    let error = expect_connection_err(
        connect_http(&config, Duration::from_secs(10)).await,
        "a server rejecting every request must fail the connect",
    );
    let chain = format!("{error:#}");
    assert!(
        chain.contains("401") || chain.contains("handshake"),
        "error chain should mention the handshake or the 401: {chain}"
    );

    let state = shared.lock().unwrap_or_else(|p| p.into_inner());
    assert_eq!(state.requests.len(), 1, "only initialize was attempted");
    assert_eq!(state.requests[0].method, "initialize");
}

#[tokio::test]
async fn silent_server_trips_the_handshake_timeout() {
    // A TCP server that accepts connections and never answers: the only clock
    // involved is the connect timeout under test.
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    // Hold every accepted socket open, reading nothing, writing nothing.
    let mut held = Vec::new();
    tokio::spawn(async move {
        loop {
            if let Ok((socket, _)) = listener.accept().await {
                held.push(socket);
            }
        }
    });

    let config = HttpServerConfig {
        url: format!("http://{addr}/mcp"),
        headers: HashMap::new(),
    };
    let error = expect_connection_err(
        connect_http(&config, Duration::from_millis(200)).await,
        "a silent server must trip the handshake timeout",
    );
    assert!(
        format!("{error:#}").contains("handshake"),
        "error chain should mention the handshake: {error:#}"
    );
}

#[tokio::test]
async fn invalid_header_values_fail_before_any_request() {
    let (shared, url) = start_mock().await;

    // A newline can never appear in a header value; the name contains a
    // space, also illegal. Both must be rejected client-side.
    let config = HttpServerConfig {
        url,
        headers: HashMap::from([
            ("Authorization".to_string(), TOKEN.to_string()),
            ("bad name".to_string(), "v".to_string()),
            ("x-bad-value".to_string(), "line\nbreak".to_string()),
        ]),
    };
    let error = expect_connection_err(
        connect_http(&config, Duration::from_secs(10)).await,
        "illegal header name/value must fail the connect",
    );
    let chain = format!("{error:#}");
    assert!(
        chain.contains("bad name") || chain.contains("x-bad-value"),
        "error chain should name the offending header: {chain}"
    );

    let state = shared.lock().unwrap_or_else(|p| p.into_inner());
    assert!(
        state.requests.is_empty(),
        "no request may reach the server for invalid headers"
    );
}
