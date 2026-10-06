//! Scripted [`McpConnection`] for the agent-loop and API e2e tests: a preset
//! (and re-programmable) tool list plus per-tool scripted call outcomes
//! (result / transport error / hang). Every call is recorded so tests can
//! assert the ORIGINAL tool names and verbatim arguments the production
//! adapter dispatched. The connection also carries a live
//! `tools/list_changed` feed tests can fire manually
//! ([`FakeMcpConnection::notify_tool_list_changed`]) to drive the manager's
//! debounced refresh path exactly like a real server announcing catalog
//! changes.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use oct_mcp::ToolListChanged;
use oct_mcp::ToolListNotifier;
use oct_mcp::{McpCallResult, McpConnection, McpToolInfo, tool_list_changed_channel};
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
    tools: Mutex<Vec<McpToolInfo>>,
    outcomes: Mutex<HashMap<String, FakeOutcome>>,
    calls: Mutex<Vec<(String, Value)>>,
    shutdowns: AtomicUsize,
    list_calls: AtomicUsize,
    tool_list_notifier: ToolListNotifier,
    tool_list_watcher: ToolListChanged,
    call_started: tokio::sync::Notify,
}

impl FakeMcpConnection {
    /// A connection advertising `tools`; tools without a scripted outcome
    /// answer with an `is_error` result naming the tool, so an unscripted
    /// call fails loudly instead of silently succeeding.
    pub fn new(tools: Vec<McpToolInfo>) -> Self {
        let (tool_list_notifier, tool_list_watcher) = tool_list_changed_channel();
        Self {
            tools: Mutex::new(tools),
            outcomes: Mutex::new(HashMap::new()),
            calls: Mutex::new(Vec::new()),
            shutdowns: AtomicUsize::new(0),
            list_calls: AtomicUsize::new(0),
            tool_list_notifier,
            tool_list_watcher,
            call_started: Default::default(),
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

    /// Replace the advertised tool list, as a live server would after
    /// installing/removing tools. Pair with
    /// [`Self::notify_tool_list_changed`] to make the manager re-list.
    pub fn set_tools(&self, tools: Vec<McpToolInfo>) {
        *self.tools.lock().unwrap() = tools;
    }

    /// Fire one `tools/list_changed` notification on this connection's feed.
    pub fn notify_tool_list_changed(&self) {
        self.tool_list_notifier.notify();
    }

    /// The `(original tool name, arguments)` pairs received, in order.
    pub fn calls(&self) -> Vec<(String, Value)> {
        self.calls.lock().unwrap().clone()
    }

    pub async fn wait_for_call(&self) {
        loop {
            let notified = self.call_started.notified();
            if !self.calls.lock().unwrap().is_empty() {
                return;
            }
            notified.await;
        }
    }

    /// How many times `list_tools` was served (connect-time listing plus
    /// every notification-driven refresh).
    pub fn list_call_count(&self) -> usize {
        self.list_calls.load(Ordering::SeqCst)
    }

    pub fn shutdown_count(&self) -> usize {
        self.shutdowns.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl McpConnection for FakeMcpConnection {
    async fn list_tools(&self) -> anyhow::Result<Vec<McpToolInfo>> {
        self.list_calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.tools.lock().unwrap().clone())
    }

    async fn call_tool(&self, name: &str, arguments: &Value) -> anyhow::Result<McpCallResult> {
        self.calls
            .lock()
            .unwrap()
            .push((name.to_string(), arguments.clone()));
        self.call_started.notify_waiters();
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
                images: Vec::new(),
            }),
        }
    }

    async fn shutdown(&self) {
        self.shutdowns.fetch_add(1, Ordering::SeqCst);
    }

    fn tool_list_changed(&self) -> Option<ToolListChanged> {
        Some(self.tool_list_watcher.clone())
    }
}

/// An [`McpToolInfo`] with the given name/description and a small object
/// schema (already in normalized form).
pub fn fake_tool(name: &str, description: Option<&str>) -> McpToolInfo {
    McpToolInfo {
        output_schema: None,
        name: name.to_string(),
        description: description.map(str::to_string),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {"a": {"type": "number"}},
        }),
    }
}
