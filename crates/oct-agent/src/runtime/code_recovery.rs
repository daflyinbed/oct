//! Recover diagnostic results for the last run's dangling code calls.
use super::RunService;
use crate::codemode::{catalog, diagnostics};
use crate::db::{messages, runs};
use oct_llm_provider::core::ContentPart;
use std::collections::HashMap;

impl RunService {
    pub(crate) async fn repair_dangling_tools(
        &self,
        conversation: &str,
        exclude: &[String],
    ) -> anyhow::Result<Vec<String>> {
        let mut outputs = HashMap::new();
        let dangling = messages::find_dangling_tool_calls(&self.pool, conversation).await?;
        if dangling.is_empty() {
            return Ok(Vec::new());
        }
        if let Some(run) = runs::latest_run_for_conversation(&self.pool, conversation).await? {
            let root = self
                .codemode_root()
                .join(conversation)
                .join(run.id)
                .join("cells");
            // Only calls actually present in this conversation's effective
            // timeline can locate artifacts. Never accept a client file path.
            let stored = messages::list_messages(&self.pool, conversation).await?;
            let mut pending = Vec::new();
            for row in stored {
                let Ok(message) = row.to_message() else {
                    continue;
                };
                for part in message.parts {
                    let ContentPart::ToolCall(call) = part else {
                        continue;
                    };
                    if call.name != "execute_code"
                        || !dangling.contains(&call.id)
                        || exclude.contains(&call.id)
                    {
                        continue;
                    }
                    let Ok(arguments) = serde_json::from_str::<serde_json::Value>(&call.arguments)
                    else {
                        continue;
                    };
                    let Some(code) = arguments.get("code").and_then(serde_json::Value::as_str)
                    else {
                        continue;
                    };
                    pending.push((call.id, code.to_owned()));
                }
            }
            outputs = tokio::task::spawn_blocking(move || {
                let mut outputs = HashMap::new();
                for (call, code) in pending {
                    let dir = root.join(catalog::digest(&call));
                    if !diagnostics::source_matches(&dir, &code) {
                        continue;
                    }
                    if let Some(output) =
                        diagnostics::interrupted_cell(&dir, &call, "backend_restarted")
                    {
                        outputs.insert(call, (output.content, output.details.unwrap()));
                    }
                }
                outputs
            })
            .await?;
        }
        messages::repair_dangling_tool_calls_with_outputs(
            &self.pool,
            conversation,
            exclude,
            &outputs,
        )
        .await
    }
}
