//! Scripted [`McpConnection`] for the agent-loop and API e2e tests: a preset
//! tool list plus per-tool scripted call outcomes (result / transport error /
//! hang). Every call is recorded so tests can assert the ORIGINAL tool names
//! and verbatim arguments the production adapter dispatched.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use oct_mcp::{McpCallResult, McpConnection, McpToolInfo};
use serde_json::Value;

/// The scripted outcome of one `tools/call`.
#[derive(Clone)]
pub enum FakeOutcome {
    /// Answer with this call result (tool-level `isError` included).
    Result(McpCallResult),
    /// Fail at the transport level (`call_tool` returns `Err`).
    Error(String),
    /// Never resolve — exercises the caller's per-call timeout.
    Hang,
}

/// A scripted MCP server connection. Build with [`FakeMcpConnection::new`]
/// plus the `with_*` builders, wrap in an `Arc`, inject into a
/// `McpManager` via `insert_connection_for_test`, and keep the `Arc` clone
/// around for assertions (`calls`, `shutdown_count`).
pub struct FakeMcpConnection {
    tools: Vec<McpToolInfo>,
    outcomes: Mutex<HashMap<String, FakeOutcome>>,
    calls: Mutex<Vec<(String, Value)>>,
    shutdowns: AtomicUsize,
}

impl FakeMcpConnection {
    /// A connection advertising `tools`; tools without a scripted outcome
    /// answer with an `is_error` result naming the tool, so an unscripted
    /// call fails loudly instead of silently succeeding.
    pub fn new(tools: Vec<McpToolInfo>) -> Self {
        Self {
            tools,
            outcomes: Mutex::new(HashMap::new()),
            calls: Mutex::new(Vec::new()),
            shutdowns: AtomicUsize::new(0),
        }
    }

    pub fn with_result(self, tool: &str, result: McpCallResult) -> Self {
        self.outcomes
            .lock()
            .unwrap()
            .insert(tool.to_string(), FakeOutcome::Result(result));
        self
    }

    pub fn with_error(self, tool: &str, message: &str) -> Self {
        self.outcomes
            .lock()
            .unwrap()
            .insert(tool.to_string(), FakeOutcome::Error(message.to_string()));
        self
    }

    pub fn with_hang(self, tool: &str) -> Self {
        self.outcomes
            .lock()
            .unwrap()
            .insert(tool.to_string(), FakeOutcome::Hang);
        self
    }

    /// The `(original tool name, arguments)` pairs received, in order.
    pub fn calls(&self) -> Vec<(String, Value)> {
        self.calls.lock().unwrap().clone()
    }

    pub fn shutdown_count(&self) -> usize {
        self.shutdowns.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl McpConnection for FakeMcpConnection {
    async fn list_tools(&self) -> anyhow::Result<Vec<McpToolInfo>> {
        Ok(self.tools.clone())
    }

    async fn call_tool(&self, name: &str, arguments: &Value) -> anyhow::Result<McpCallResult> {
        self.calls
            .lock()
            .unwrap()
            .push((name.to_string(), arguments.clone()));
        // Clone the outcome out of the lock: the Hang arm awaits forever and
        // a held MutexGuard would make the future !Send.
        let outcome = self.outcomes.lock().unwrap().get(name).cloned();
        match outcome {
            Some(FakeOutcome::Result(result)) => Ok(result),
            Some(FakeOutcome::Error(message)) => Err(anyhow::anyhow!("{message}")),
            Some(FakeOutcome::Hang) => futures_util::future::pending().await,
            None => Ok(McpCallResult {
                text: format!("no scripted outcome for tool `{name}`"),
                structured_json: None,
                is_error: true,
            }),
        }
    }

    async fn shutdown(&self) {
        self.shutdowns.fetch_add(1, Ordering::SeqCst);
    }
}

/// An [`McpToolInfo`] with the given name/description and a small object
/// schema (already in normalized form).
pub fn fake_tool(name: &str, description: Option<&str>) -> McpToolInfo {
    McpToolInfo {
        name: name.to_string(),
        description: description.map(str::to_string),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {"a": {"type": "number"}},
        }),
    }
}
