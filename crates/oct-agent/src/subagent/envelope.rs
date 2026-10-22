//! Read the durable worker result for live delegation and crash recovery.
use super::manager::is_terminal_status;
use crate::db::{messages as msg_db, runs as run_db};
use crate::tools::truncate::{MAX_TOOL_OUTPUT_BYTES, truncate_middle};
use anyhow::Result;
use oct_llm_provider::core::ContentPart;
use serde_json::{Value, json};

pub struct TaskEnvelope {
    pub content: String,
    pub is_error: bool,
    pub details: Value,
}

/// Build the task envelope from a SETTLED run row + the child pool:
/// shared verbatim by the live path, the crash backfill and the
/// recovery — the DB row is the single source of truth.
pub async fn build_task_envelope(
    pool: &sqlx::SqlitePool,
    run: &run_db::AgentRun,
) -> Result<TaskEnvelope> {
    anyhow::ensure!(
        is_terminal_status(&run.status),
        "child run {} is not durably settled",
        run.id
    );
    let output = final_output_text(pool, run).await?;
    let truncated_output = truncate_middle(&output, MAX_TOOL_OUTPUT_BYTES);
    let truncated = truncated_output != output;

    let mut envelope = json!({
        "agent_id": run.conversation_id,
        "run_id": run.id,
        "status": run.status,
        "output": truncated_output,
        "truncated": truncated,
        "usage": usage_json(run),
    });
    if let Some(error) = &run.error {
        envelope["error"] = json!(error);
    }
    // Interrupted runs may still carry persisted partial output — it must
    // not read as a settled success.
    if run.status != "completed" && !output.is_empty() {
        envelope["partial"] = json!(true);
    }

    let description = run.description.clone().unwrap_or_default();
    Ok(TaskEnvelope {
        content: serde_json::to_string(&envelope)?,
        is_error: run.status != "completed",
        details: json!({
            "kind": "subagent_task",
            "agent_id": run.conversation_id,
            "run_id": run.id,
            "description": description,
            "status": run.status,
        }),
    })
}

/// `{"input_tokens":..,"output_tokens":..}` when either side is known,
/// `null` when usage never arrived (unknown ≠ zero).
fn usage_json(run: &run_db::AgentRun) -> Value {
    match (run.input_tokens, run.output_tokens) {
        (None, None) => Value::Null,
        (input, output) => json!({
            "input_tokens": input,
            "output_tokens": output,
        }),
    }
}

/// The run's final assistant text: the `final_message_id` content row when
/// present, else the last assistant row of the child conversation (crash
/// paths may have persisted the message without updating the run row).
async fn final_output_text(pool: &sqlx::SqlitePool, run: &run_db::AgentRun) -> Result<String> {
    if let Some(final_id) = &run.final_message_id
        && let Some(row) = msg_db::get_message_content(pool, final_id).await?
    {
        return Ok(text_of_parts(&row.parts_json));
    }
    let stored = msg_db::list_messages(pool, &run.conversation_id).await?;
    Ok(stored
        .iter()
        .rev()
        .find(|m| m.role == "assistant")
        .map(|m| text_of_parts(&m.parts_json))
        .unwrap_or_default())
}

/// Concatenated text parts of a serialized `Vec<ContentPart>`.
fn text_of_parts(parts_json: &str) -> String {
    serde_json::from_str::<Vec<ContentPart>>(parts_json)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| match p {
                    ContentPart::Text(text) => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}
