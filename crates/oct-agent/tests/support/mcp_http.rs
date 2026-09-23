//! In-process Streamable HTTP MCP server for the API e2e tests.
//!
//! Same server shape as `crates/oct-mcp/tests/http_transport.rs` (plain JSON
//! responses, no SSE stream, session id on initialize), but self-contained
//! for the agent-side harness: an `mcp_servers` row with `transport: "http"`
//! points at it, the real backend connects through oct-mcp's `connect_http`,
//! and the tests assert on the requests that actually arrived.

use std::sync::Arc;
use std::sync::Mutex;

use axum::Json as AxumJson;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::post;
use serde_json::Value;
use serde_json::json;

/// Bearer value the mock demands on every request.
pub const TOKEN: &str = "Bearer agent-e2e-token";

#[derive(Debug, Clone, PartialEq)]
pub struct RecordedRequest {
    pub method: String,
    pub authorization: Option<String>,
}

#[derive(Default)]
struct MockState {
    requests: Vec<RecordedRequest>,
}

/// A running mock server; assert on `requests()` after driving the backend.
pub struct McpHttpMock {
    pub url: String,
    shared: Arc<Mutex<MockState>>,
}

impl McpHttpMock {
    /// Bind an ephemeral localhost port and serve `/mcp`. The server task
    /// lives until the test's runtime ends.
    pub async fn spawn() -> Self {
        let shared: Arc<Mutex<MockState>> = Arc::default();
        let app = axum::Router::new()
            .route("/mcp", post(mcp_post).get(mcp_get).delete(mcp_delete))
            .with_state(shared.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock mcp server");
        let addr = listener.local_addr().expect("mock mcp addr");
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("mock mcp server run");
        });
        Self {
            url: format!("http://{addr}/mcp"),
            shared,
        }
    }

    /// Every request the mock saw, in arrival order.
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .requests
            .clone()
    }
}

async fn mcp_post(
    State(shared): State<Arc<Mutex<MockState>>>,
    headers: HeaderMap,
    AxumJson(message): AxumJson<Value>,
) -> Response {
    let method = message["method"].as_str().unwrap_or_default().to_string();
    let authorization = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .requests
        .push(RecordedRequest {
            method: method.clone(),
            authorization: authorization.clone(),
        });

    if authorization.as_deref() != Some(TOKEN) {
        return (StatusCode::UNAUTHORIZED, "missing bearer").into_response();
    }
    let Some(id) = message.get("id").cloned() else {
        return StatusCode::ACCEPTED.into_response();
    };
    let body = match method.as_str() {
        "initialize" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "protocolVersion": message["params"]["protocolVersion"],
                "capabilities": {},
                "serverInfo": {"name": "agent-e2e-http", "version": "0.0.0"},
            },
        }),
        "tools/list" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {"tools": [
                {
                    "name": "remote_tool",
                    "description": "served over http",
                    "inputSchema": {"type": "object", "properties": {}},
                },
            ]},
        }),
        "tools/call" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {"content": [{"type": "text", "text": "remote result"}]},
        }),
        // prompts: one argument-less prompt and one with a required argument.
        "prompts/list" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {"prompts": [
                {"name": "status", "description": "report status"},
                {
                    "name": "deploy",
                    "description": "deploy checklist",
                    "arguments": [{"name": "env", "required": true}],
                },
            ]},
        }),
        "prompts/get" => {
            let name = message["params"]["name"].as_str().unwrap_or_default();
            match name {
                "status" => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {"messages": [
                        {"role": "user", "content": {"type": "text", "text": "Report status"}},
                    ]},
                }),
                "deploy" => {
                    let env = message["params"]["arguments"]["env"]
                        .as_str()
                        .unwrap_or("?");
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {"messages": [
                            {"role": "user", "content": {"type": "text", "text": format!("Deploy to {env}")}},
                            {"role": "assistant", "content": {"type": "text", "text": "Checklist ready"}},
                        ]},
                    })
                }
                other => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": -32602, "message": format!("unknown prompt {other}")},
                }),
            }
        }
        "resources/list" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {"resources": [
                {
                    "uri": "memo://notes",
                    "name": "notes",
                    "description": "meeting notes",
                    "mimeType": "text/markdown",
                },
                {"uri": "file://logo.png", "name": "logo", "mimeType": "image/png"},
            ]},
        }),
        "resources/read" => {
            let uri = message["params"]["uri"].as_str().unwrap_or_default();
            match uri {
                "memo://notes" => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {"contents": [
                        {"uri": uri, "mimeType": "text/markdown", "text": "# Notes"},
                    ]},
                }),
                "file://logo.png" => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {"contents": [
                        {"uri": uri, "mimeType": "image/png", "blob": "aVBuZw=="},
                    ]},
                }),
                other => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": -32602, "message": format!("unknown resource {other}")},
                }),
            }
        }
        other => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32601, "message": format!("no handler for {other}")},
        }),
    };
    let mut response = AxumJson(body).into_response();
    if method == "initialize" {
        response.headers_mut().insert(
            "mcp-session-id",
            "agent-sess".parse().expect("static header"),
        );
    }
    response
}

/// No standalone SSE stream (JSON-only server); rmcp skips it on 405.
async fn mcp_get() -> StatusCode {
    StatusCode::METHOD_NOT_ALLOWED
}

async fn mcp_delete() -> StatusCode {
    StatusCode::OK
}
