//! Tool execution shared by the model loop and nested code-mode calls.
use super::{AgentTool, CodeToolResult, ToolContext, ToolOutput};
use crate::agent::EventHub;
use crate::permissions::gate::{GateVerdict, PermissionGate};
use anyhow::Result;
use serde_json::Value;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

pub async fn direct(
    tool: Option<&Arc<dyn AgentTool>>,
    name: &str,
    args: Value,
    ctx: &ToolContext,
    gate: &PermissionGate,
    hub: &Arc<EventHub>,
) -> ToolOutput {
    let Some(tool) = tool else {
        return ToolOutput::error(format!("Unknown tool: {name}"));
    };
    if let Err(error) = tool.validate_input(&args) {
        return ToolOutput::error(format!("Invalid tool arguments: {error:#}"));
    }
    // Authorize by the CANONICAL name (`authz_name`), not the model-facing
    // alias the call came in under: permission rules are written against
    // current server names and must keep matching across frozen renames.
    match gate
        .check(
            tool.authz_name(),
            &args,
            ctx.call_id(),
            hub,
            ctx.cancel_token(),
        )
        .await
    {
        GateVerdict::Denied(output) => output,
        GateVerdict::Proceed => tool
            .execute(args, ctx)
            .await
            .unwrap_or_else(|e| ToolOutput::error(format!("Tool execution error: {e:#}"))),
    }
}

pub async fn nested(
    tool: &Arc<dyn AgentTool>,
    args: Value,
    ctx: &ToolContext,
    gate: &PermissionGate,
    hub: &Arc<EventHub>,
    approvals: &AtomicUsize,
) -> Result<CodeToolResult> {
    tool.validate_input(&args)
        .map_err(|error| oct_codemode::CallError::new("invalid_arguments", format!("{error:#}")))?;
    match gate
        .check_with_wait_counter(
            tool.authz_name(),
            &args,
            ctx.call_id(),
            hub,
            ctx.cancel_token(),
            approvals,
        )
        .await
    {
        GateVerdict::Denied(output) => {
            Err(oct_codemode::CallError::new("permission_denied", output.content).into())
        }
        GateVerdict::Proceed => {
            let result = tool.execute_for_code(args, ctx).await?;
            if result.is_error {
                let mut error = oct_codemode::CallError::new(
                    "tool_error",
                    format!(
                        "{} failed: {}",
                        if matches!(
                            tool.selector(),
                            crate::agent_settings::ToolSelector::Mcp { .. }
                        ) {
                            "MCP tool"
                        } else {
                            "Tool"
                        },
                        crate::mcp::tool::result_content(
                            &result.text,
                            result.structured_content.as_ref()
                        )
                    ),
                );
                error.result = Some(result_value(result));
                return Err(error.into());
            }
            Ok(result)
        }
    }
}

pub(crate) fn result_value(result: CodeToolResult) -> Value {
    let mut value = serde_json::json!({"text":result.text,"isError":result.is_error,"images":result.images.into_iter().map(|i|serde_json::json!({"mimeType":i.mime_type,"data":i.data})).collect::<Vec<_>>()});
    if let Some(structured) = result.structured_content {
        value["structuredContent"] = structured;
    }
    value
}
