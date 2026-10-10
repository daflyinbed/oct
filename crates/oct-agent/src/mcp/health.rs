//! Authentication failure retires an HTTP connection without replaying its
//! requests. Existing callers finish independently; the next assembly owns
//! credential refresh and replacement under the manager's server gate.
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use anyhow::Result;
use async_trait::async_trait;
use oct_mcp::{
    McpCallResult, McpConnection, McpPromptInfo, McpPromptResult, McpResourceContent,
    McpResourceInfo, McpToolInfo, ToolListChanged,
};
use serde_json::Value;

pub(crate) struct AuthAwareConnection {
    inner: Arc<dyn McpConnection>,
    rejected: Arc<AtomicBool>,
}

impl AuthAwareConnection {
    pub(crate) fn new(inner: Arc<dyn McpConnection>, rejected: Arc<AtomicBool>) -> Self {
        Self { inner, rejected }
    }

    fn observe<T>(&self, result: Result<T>) -> Result<T> {
        if let Err(error) = &result
            && super::credential::looks_like_auth_rejection(&format!("{error:#}"))
        {
            self.rejected.store(true, Ordering::Release);
        }
        result
    }
}

impl Drop for AuthAwareConnection {
    fn drop(&mut self) {
        // The registry and each run own Arc leases of this adapter. Automatic
        // credential replacement must not close a transport still borrowed
        // by another run; the final lease performs graceful cleanup instead.
        // Outside a runtime, dropping the inner service still cancels it.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let inner = self.inner.clone();
            runtime.spawn(async move { inner.shutdown().await });
        }
    }
}

#[async_trait]
impl McpConnection for AuthAwareConnection {
    fn is_closed(&self) -> bool {
        self.rejected.load(Ordering::Acquire) || self.inner.is_closed()
    }
    async fn list_tools(&self) -> Result<Vec<McpToolInfo>> {
        self.observe(self.inner.list_tools().await)
    }
    async fn call_tool(&self, name: &str, args: &Value) -> Result<McpCallResult> {
        self.observe(self.inner.call_tool(name, args).await)
    }
    async fn list_prompts(&self) -> Result<Vec<McpPromptInfo>> {
        self.observe(self.inner.list_prompts().await)
    }
    async fn get_prompt(&self, name: &str, args: &Value) -> Result<McpPromptResult> {
        self.observe(self.inner.get_prompt(name, args).await)
    }
    async fn list_resources(&self) -> Result<Vec<McpResourceInfo>> {
        self.observe(self.inner.list_resources().await)
    }
    async fn read_resource(&self, uri: &str) -> Result<McpResourceContent> {
        self.observe(self.inner.read_resource(uri).await)
    }
    async fn shutdown(&self) {
        self.inner.shutdown().await;
    }
    fn tool_list_changed(&self) -> Option<ToolListChanged> {
        self.inner.tool_list_changed()
    }
}
