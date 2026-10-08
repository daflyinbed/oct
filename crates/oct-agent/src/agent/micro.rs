//! Microcompact: the cheap, no-LLM first tier of context clearing.
//!
//! Summary compaction (see [`crate::agent::compact`]) spends a model call and
//! discards verbatim history; microcompact spends one local transaction and
//! only blanks out OLD successful tool results, replacing each with a
//! placeholder via copy-on-write (see [`crate::db::messages::apply_cow_swaps`]):
//! content becomes [`PLACEHOLDER_CONTENT`], images are dropped, call_id and
//! the tool-call pairing stay intact — the model keeps the shape of the
//! conversation and loses only bulk it can re-fetch (files can be re-read).
//!
//! Triggers, both evaluated at the turn top BEFORE the summary-compaction
//! check, so clearing may pull the estimate back under the compaction
//! threshold entirely:
//! * waterline — estimated context ≥ [`TRIGGER_RATIO_PERCENT`]% of the window
//!   (summary compaction fires at 85%); an unknown window disables this path;
//! * idle — a run starting ≥ [`IDLE_TRIGGER`] after the conversation's last
//!   activity (captured in the API layer BEFORE the new user message is
//!   inserted). One-shot at the first turn top; the provider prompt cache is
//!   long cold by then anyway. Fires regardless of the window.
//!
//! Selection never touches: error results (the summary prompt wants them
//! verbatim), the trailing round (the exchange the model just consumed), the
//! newest [`PROTECTED_RECENT_TOOL_RESULTS`] results before it, and anything
//! already holding a placeholder (idempotent by construction — a re-trigger
//! finds nothing new to clear, so no anti-loop bookkeeping is needed). A
//! batch whose projected savings fall below [`MIN_SAVINGS_TOKENS`] is skipped
//! entirely.
//!
//! The usage anchor is adjusted in memory with the same rule
//! `db::messages::load_llm_view` recomputes on every load: a cleared result
//! that sat before the anchor was counted inside the anchor's provider
//! tokens, so its savings are subtracted; results after the anchor only
//! shrink the local increment (automatic once `full_messages` is rewritten).

use serde_json::Value;
use sqlx::SqlitePool;

use oct_llm_provider::core::{ContentPart, Message, ToolResult, ToolSpec};

use super::compact;
use super::{AgentEvent, EventHub, MicrocompactReplacement, MicrocompactTrigger};
use crate::context::ContextState;
use crate::db::messages::{self as msg_db, CowSwap};

/// Waterline: estimated context at or above this fraction of the window
/// triggers a clear. Deliberately below summary compaction's 85% so the
/// cheap tier gets its chance first.
pub const TRIGGER_RATIO_PERCENT: u64 = 70;

pub fn trigger_threshold(window: u32) -> u64 {
    u64::from(window) * TRIGGER_RATIO_PERCENT / 100
}

/// A batch projected to save less than this isn't worth a transaction.
pub const MIN_SAVINGS_TOKENS: u64 = 1_024;

/// How many of the newest tool results before the trailing round survive a
/// clear (the model's working set).
pub const PROTECTED_RECENT_TOOL_RESULTS: usize = 5;

/// Idle entry: a run starting this long after the conversation's last
/// activity clears once, regardless of the waterline.
pub const IDLE_TRIGGER: std::time::Duration = std::time::Duration::from_secs(60 * 60);

pub const PLACEHOLDER_CONTENT: &str = "[Old tool result content cleared]";

/// details_json `kind` on a cleared copy row: lets the stored-history replay
/// mark the tool card as cleared (UI-only metadata, like the compaction
/// marker's kind).
pub const CLEARED_DETAILS_KIND: &str = "microcompact_cleared";

// ======================================================================
// Selection
// ======================================================================

/// One selected target of a clear batch.
pub struct MicroTarget {
    /// Index into the conversation entries (system prompt NOT included —
    /// the coordinates [`ContextState::conversation`] uses).
    pub index: usize,
    pub call_id: String,
    /// estimate(original) − estimate(placeholder copy).
    pub savings: u64,
}

/// Pick the clearable tool results (see the module doc for the protection
/// rules). `messages` is the conversation WITHOUT the system prompt; the
/// returned indices address the same slice. Tool rows carry a single
/// ToolResult part in practice; a multi-part row is only cleared when EVERY
/// result part is a non-error non-placeholder (the savings then come from
/// the whole row).
pub fn select_targets(messages: &[Message]) -> Vec<MicroTarget> {
    // The trailing round (results the model just consumed) is untouchable.
    let round_start = compact::last_round_start(messages);

    let mut candidates: Vec<usize> = Vec::new();
    for (i, msg) in messages.iter().enumerate() {
        if i >= round_start {
            continue;
        }
        if !msg.parts.iter().any(is_result_part) {
            continue;
        }
        // Errors are exactly what the summary prompt wants verbatim;
        // placeholders are the idempotency guard (already cleared).
        if !msg.parts.iter().all(|p| !is_error_or_placeholder_part(p)) {
            continue;
        }
        candidates.push(i);
    }

    // Protect the newest few results before the trailing round.
    let cutoff = candidates
        .len()
        .saturating_sub(PROTECTED_RECENT_TOOL_RESULTS);
    let selected = &candidates[..cutoff];

    let targets: Vec<MicroTarget> = selected
        .iter()
        .filter_map(|&i| {
            let msg = &messages[i];
            let call_id = first_call_id(msg)?;
            let copy = cleared_copy(msg);
            Some(MicroTarget {
                index: i,
                call_id,
                savings: compact::estimate_message(msg)
                    .saturating_sub(compact::estimate_message(&copy)),
            })
        })
        .collect();

    let total: u64 = targets.iter().map(|t| t.savings).sum();
    if total < MIN_SAVINGS_TOKENS {
        return Vec::new();
    }
    targets
}

fn is_result_part(part: &ContentPart) -> bool {
    matches!(part, ContentPart::ToolResult(_))
}

fn is_error_or_placeholder_part(part: &ContentPart) -> bool {
    match part {
        ContentPart::ToolResult(tr) => {
            tr.is_error || compact::tool_result_text(&tr.content) == PLACEHOLDER_CONTENT
        }
        _ => false,
    }
}

fn first_call_id(msg: &Message) -> Option<String> {
    msg.parts.iter().find_map(|p| match p {
        ContentPart::ToolResult(tr) => Some(tr.call_id.clone()),
        _ => None,
    })
}

/// The cleared replacement for one tool-result message: placeholder text,
/// images emptied, pairing identifiers intact. Non-result parts (there are
/// none in practice) pass through untouched.
pub fn cleared_copy(original: &Message) -> Message {
    let parts = original
        .parts
        .iter()
        .map(|part| match part {
            ContentPart::ToolResult(tr) => ContentPart::ToolResult(ToolResult {
                call_id: tr.call_id.clone(),
                content: Value::String(PLACEHOLDER_CONTENT.into()),
                is_error: tr.is_error,
                images: Vec::new(),
            }),
            other => other.clone(),
        })
        .collect();
    Message {
        role: original.role,
        parts,
    }
}

// ======================================================================
// In-run application
// ======================================================================

/// Result of one turn-top microcompaction attempt.
pub enum MicroAttempt {
    /// Cleared; the value is the estimated context right after the rewrite.
    Done(u64),
    /// Nothing worth clearing (protection windows or savings floor); safe to
    /// re-check next turn.
    Nothing,
    /// The swap batch failed to persist; micro is disabled for the rest of
    /// the run.
    Failed,
}

/// Run one microcompaction: select, persist the COW batch, apply the clears
/// to the in-run context state, publish the event. `prompt_env_refresh`
/// flags the conversation's frozen prompt environment dirty once swaps land
/// (the prefix cache breaks with the rewritten history; the next run
/// re-reads the instruction files).
pub async fn run_microcompaction(
    pool: &SqlitePool,
    conv_id: &str,
    hub: &EventHub,
    trigger: MicrocompactTrigger,
    context: &mut ContextState,
    tool_specs: &[ToolSpec],
    prompt_env_refresh: Option<&super::repo_instructions::PromptEnvRefresh>,
) -> MicroAttempt {
    // Selection works on plain messages (conversation coordinates — no
    // system prompt, same indices as the context entries).
    let conversation: Vec<Message> = context
        .conversation()
        .iter()
        .map(|e| e.message.clone())
        .collect();
    let targets = select_targets(&conversation);
    if targets.is_empty() {
        return MicroAttempt::Nothing;
    }

    let swaps: Vec<CowSwap> = targets
        .iter()
        .map(|t| CowSwap {
            call_id: t.call_id.clone(),
            copy: cleared_copy(&conversation[t.index]),
            details: Some(serde_json::json!({ "kind": CLEARED_DETAILS_KIND })),
        })
        .collect();
    let applied = match msg_db::apply_cow_swaps(pool, conv_id, &swaps).await {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!("microcompaction failed to persist swaps: {e}");
            return MicroAttempt::Failed;
        }
    };
    if applied.is_empty() {
        // The in-memory view and the DB disagree (e.g. another writer swapped
        // first); leave memory untouched and let the next selection re-sync.
        return MicroAttempt::Nothing;
    }

    // Swaps landed: the history changed under the frozen prompt head — flag
    // the environment so the next run's assembly refreshes it.
    if let Some(refresh) = prompt_env_refresh {
        refresh.mark_dirty();
    }

    // Account ONLY for the swaps that actually landed: `applied` can be a
    // strict subset of `targets` when a call_id has no live DB slot (e.g. its
    // row failed to persist earlier this run). Memory rewrite, anchor
    // deduction, savings and the event must all share this subset's 口径,
    // or the model would see content the DB never swapped.
    let applied_ids: std::collections::HashSet<&str> =
        applied.iter().map(|a| a.call_id.as_str()).collect();
    let landed: Vec<(usize, u64)> = targets
        .iter()
        .filter(|t| applied_ids.contains(t.call_id.as_str()))
        .map(|t| (t.index, t.savings))
        .collect();

    // Anchor deduction (pre-anchor clears) and the placeholder rewrite are
    // the context state's job — same rule `load_llm_view` recomputes from
    // persisted state on every load.
    context.apply_clears(&landed, PLACEHOLDER_CONTENT);

    let est_tokens_saved: u64 = landed.iter().map(|(_, s)| s).sum();
    let est_after = context.estimate(tool_specs);

    hub.publish(AgentEvent::Microcompacted {
        triggered_by: trigger,
        replacements: applied
            .into_iter()
            .map(|a| MicrocompactReplacement {
                call_id: a.call_id,
                copy_message_id: a.copy.id,
            })
            .collect(),
        est_tokens_saved,
        est_context_tokens: est_after,
    });
    MicroAttempt::Done(est_after)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oct_llm_provider::core::{Role, ToolCall};

    fn tool_result_msg(call_id: &str, text: &str) -> Message {
        Message {
            role: Role::Tool,
            parts: vec![ContentPart::ToolResult(ToolResult {
                call_id: call_id.to_string(),
                content: Value::String(text.to_string()),
                is_error: false,
                images: Vec::new(),
            })],
        }
    }

    fn assistant_with_call(id: &str) -> Message {
        Message {
            role: Role::Assistant,
            parts: vec![ContentPart::ToolCall(ToolCall {
                id: id.to_string(),
                name: "read".to_string(),
                arguments: "{}".to_string(),
            })],
        }
    }

    /// [user, (assistant, tool) × n, user] — every tool result is a
    /// candidate except the trailing round's and the newest few before it.
    /// Conversation coordinates: no system prompt.
    fn history_with_results(n: usize, text: &str) -> Vec<Message> {
        let mut msgs = vec![Message::text(Role::User, "go")];
        for i in 0..n {
            msgs.push(assistant_with_call(&format!("c{i}")));
            msgs.push(tool_result_msg(&format!("c{i}"), text));
        }
        msgs.push(Message::text(Role::User, "next"));
        msgs
    }

    #[test]
    fn selection_protects_last_round_and_recent_five() {
        // 8 rounds; the last message is a user message → the trailing round
        // is empty, so candidates are all 8 results minus the newest 5.
        let msgs = history_with_results(8, &"x".repeat(4_000));
        let targets = select_targets(&msgs);
        assert_eq!(targets.len(), 3, "8 results − 5 protected");
        // The OLDEST results are selected: c0, c1, c2.
        let ids: Vec<&str> = targets.iter().map(|t| t.call_id.as_str()).collect();
        assert_eq!(ids, vec!["c0", "c1", "c2"]);
    }

    #[test]
    fn selection_never_touches_the_trailing_round() {
        // Ending on tool results: last_round_start covers the final
        // assistant+result exchange; the newest 5 before it are also kept.
        let mut msgs = history_with_results(8, &"x".repeat(4_000));
        msgs.pop(); // drop the trailing user message
        msgs.push(assistant_with_call("c8"));
        msgs.push(tool_result_msg("c8", &"x".repeat(4_000)));
        let targets = select_targets(&msgs);
        assert!(
            targets.iter().all(|t| t.call_id != "c8"),
            "trailing round survives"
        );
        // 8 older results − 5 protected = 3.
        assert_eq!(targets.len(), 3);
    }

    #[test]
    fn selection_skips_errors_placeholders_and_tiny_batches() {
        let mut msgs = Vec::new();
        for i in 0..8 {
            msgs.push(assistant_with_call(&format!("e{i}")));
            let mut m = tool_result_msg(&format!("e{i}"), &"x".repeat(4_000));
            let ContentPart::ToolResult(tr) = &mut m.parts[0] else {
                unreachable!()
            };
            tr.is_error = true;
            msgs.push(m);
        }
        assert!(
            select_targets(&msgs).is_empty(),
            "error results are never cleared"
        );

        let msgs = history_with_results(8, PLACEHOLDER_CONTENT);
        assert!(
            select_targets(&msgs).is_empty(),
            "already-cleared rows are skipped"
        );

        // Tiny results: savings below the floor → nothing.
        let msgs = history_with_results(8, "x");
        assert!(select_targets(&msgs).is_empty(), "below MIN_SAVINGS");
    }

    #[test]
    fn cleared_copy_blankets_content_and_images() {
        let mut original = tool_result_msg("c1", "huge output");
        let ContentPart::ToolResult(tr) = &mut original.parts[0] else {
            unreachable!()
        };
        tr.images = vec![oct_llm_provider::core::ToolResultImage {
            mime_type: "image/png".into(),
            data: "aGk=".into(),
        }];
        let copy = cleared_copy(&original);
        let ContentPart::ToolResult(tr) = &copy.parts[0] else {
            unreachable!()
        };
        assert_eq!(tr.content, Value::String(PLACEHOLDER_CONTENT.into()));
        assert!(tr.images.is_empty());
        assert_eq!(tr.call_id, "c1");
        assert!(!tr.is_error);
    }
}
