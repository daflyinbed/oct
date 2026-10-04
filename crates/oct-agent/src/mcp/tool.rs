//! The `AgentTool` adapter for MCP tools: adapts one cached `tools/list`
//! entry into the agent loop's tool trait.
//!
//! The LLM sees the flat `mcp__{server}__{tool}` name (deduplicated across
//! servers by `oct_mcp::ToolNameMap`), while calls are dispatched over the
//! server's connection under the original tool name, wrapped in that
//! server's `tool_timeout_ms` budget and aborted as soon as the run's
//! cancellation token fires (the shared connection is never shut down by an
//! individual call).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tracing::warn;

use oct_mcp::{McpCallResult, McpConnection, McpToolInfo};

use crate::tools::truncate::MAX_TOOL_OUTPUT_BYTES;
use crate::tools::truncate::truncate_middle;
use crate::tools::{ToolContext, ToolImage, ToolOutput};

/// Cap (in chars) on the LLM-visible description of an MCP tool. The
/// ellipsis is appended on top of the cap.
pub(crate) const MAX_DESCRIPTION_CHARS: usize = 1024;

/// Placeholder description when a server ships a tool without one.
pub(crate) const NO_DESCRIPTION: &str = "MCP tool (the server provided no description)";

/// Content fallback when a server returns neither text nor structured
/// content: an empty tool result is technically legal but unhelpful to the
/// model, so say so explicitly.
pub(crate) const NO_CONTENT: &str = "(MCP tool returned no content)";

/// Cap on images kept per MCP tool result; extras are dropped (count noted
/// in details) so a server returning dozens of images cannot flood the LLM
/// context.
pub(crate) const MAX_RESULT_IMAGES: usize = 8;

/// Cap on one image's base64 payload (chars, i.e. ~3/4 of the decoded
/// bytes); larger images are dropped (count noted in details).
pub(crate) const MAX_IMAGE_BASE64_CHARS: usize = 5 * 1024 * 1024;

/// `[mcp:{server}] {description}` for the LLM, capped at
/// [`MAX_DESCRIPTION_CHARS`] chars plus an ellipsis. A missing (or blank)
/// description gets a placeholder so the tool never advertises nothing.
pub(crate) fn tool_description(server: &str, description: Option<&str>) -> String {
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
pub(crate) fn result_content(result: &McpCallResult) -> String {
    if !result.text.is_empty() {
        result.text.clone()
    } else if let Some(structured) = &result.structured_json {
        serde_json::to_string_pretty(structured).unwrap_or_else(|_| structured.to_string())
    } else {
        NO_CONTENT.to_string()
    }
}

/// One MCP tool behind the [`crate::tools::AgentTool`] trait. The LLM
/// addresses it by the deduplicated exposed name; `execute` dispatches over
/// `connection` under the original server-side name, bounded by `timeout`.
pub struct McpTool {
    server_id: String,
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
            server_id: String::new(),
            exposed_name,
            original_name: info.name,
            server_name,
            connection,
            timeout,
            description,
            input_schema: info.input_schema,
        }
    }

    /// Attach the DB identity without changing the existing test/adapter constructor.
    pub fn with_server_id(mut self, server_id: String) -> Self {
        self.server_id = server_id;
        self
    }
}

#[async_trait]
impl crate::tools::AgentTool for McpTool {
    fn selector(&self) -> crate::agent_settings::ToolSelector {
        crate::agent_settings::ToolSelector::Mcp {
            server_id: self.server_id.clone(),
            tool: self.original_name.clone(),
        }
    }

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
        // dumping megabytes cannot flood the LLM context. Images bypass the
        // text budget but are capped separately (count + per-image size);
        // dropped ones are counted in details so the UI (and the log) can
        // tell something was withheld.
        let content = truncate_middle(&result_content(&result), MAX_TOOL_OUTPUT_BYTES);
        let mut images = Vec::new();
        let mut dropped_images = 0usize;
        for image in &result.images {
            if images.len() >= MAX_RESULT_IMAGES || image.data.len() > MAX_IMAGE_BASE64_CHARS {
                dropped_images += 1;
                continue;
            }
            images.push(ToolImage {
                mime_type: image.mime_type.clone(),
                data: image.data.clone(),
            });
        }
        if dropped_images > 0 {
            warn!(
                server = %self.server_name,
                tool = %self.original_name,
                dropped = dropped_images,
                "dropped MCP result images over the count/size budget"
            );
        }
        let mut details = json!({
            "server": self.server_name,
            "tool": self.original_name,
            "structured": result.structured_json,
        });
        if dropped_images > 0 {
            details["images_dropped"] = json!(dropped_images);
        }
        if result.is_error {
            Ok(ToolOutput::error_with_details(content, details).with_images(images))
        } else {
            Ok(ToolOutput::success_with_details(content, details).with_images(images))
        }
    }
}
