//! Context compaction: summarize old conversation history into a single
//! marker message when the context window runs low.
//!
//! Trigger policy lives with the agent loop (a turn-top check before every
//! LLM request); this module owns the pure pieces — token estimation,
//! threshold/tail budgets, cut-point selection, summary-input serialization,
//! the one-shot summary model call, and marker-message construction. The
//! shape follows the industry-standard design:
//!
//! * the estimate anchors on the provider's real reported usage and only
//!   locally estimates what was appended since (pure local estimation misses
//!   the system prompt and tool schemas entirely);
//! * the newest exchange is kept verbatim — the cut NEVER splits an assistant
//!   tool-call from its tool results;
//! * old rows are never deleted: the marker is a normal `user` message row
//!   (summary text in parts_json, metadata in details_json) and
//!   [`crate::db::messages::load_llm_view`] returns only
//!   `[marker, everything after it]`.
//!
//! The summary call mirrors the title job: same model as the run, a single
//! non-streaming, tool-less `generate`, hard timeout, and failure that never
//! escalates — the run simply continues with the un-compacted history.

use std::time::Duration;

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use oct_llm_provider::core::{ContentPart, GenerateOptions, Message, Role, ToolSpec};
use oct_llm_provider::model::{ChatModel, ChatRequest};

use super::{AgentEvent, CompactionTrigger, EventHub};
use crate::db::messages as msg_db;

/// Auto-compact when the estimated context reaches this fraction of the
/// window. 85% leaves room for the summary call itself plus the response.
pub const TRIGGER_RATIO_PERCENT: u64 = 85;

/// Tail (verbatim-kept) budget: 10% of the window, capped for giant windows.
/// No floor: on a small window a fixed 8k floor would exceed the whole
/// conversation and make compaction impossible (the tail would swallow
/// everything, leaving no region to summarize).
pub fn tail_keep_tokens(window: u32) -> u64 {
    (u64::from(window) / 10).min(32_000)
}

/// Tail budget for manual compaction when the window is unknown (auto
/// compaction is disabled in that case, but manual must still work).
pub const DEFAULT_TAIL_KEEP_TOKENS: u64 = 16_000;

pub fn trigger_threshold(window: u32) -> u64 {
    u64::from(window) * TRIGGER_RATIO_PERCENT / 100
}

/// A conversation whose compactable region is shorter than this isn't worth a
/// summary call — guard for both auto and manual paths.
pub const MIN_MESSAGES_BEFORE_CUT: usize = 6;

// ======================================================================
// Token estimation
// ======================================================================

/// Rough token estimate for a string: ASCII chars ÷ 4, non-ASCII chars × 1.
/// Over-estimates CJK slightly (one char is often < 1 token), which errs on
/// the safe side for a trigger that wants to fire early.
pub fn estimate_str(s: &str) -> u64 {
    let mut ascii = 0u64;
    let mut non_ascii = 0u64;
    for c in s.chars() {
        if c.is_ascii() {
            ascii += 1;
        } else {
            non_ascii += 1;
        }
    }
    ascii / 4 + non_ascii
}

/// Per-message fixed overhead (role/framing tokens every provider charges).
const PER_MESSAGE_OVERHEAD_TOKENS: u64 = 4;
/// Per tool-call/result pairing overhead.
const PER_TOOL_PART_OVERHEAD_TOKENS: u64 = 8;

pub fn estimate_message(msg: &Message) -> u64 {
    PER_MESSAGE_OVERHEAD_TOKENS + msg.parts.iter().map(estimate_part).sum::<u64>()
}

fn estimate_part(part: &ContentPart) -> u64 {
    match part {
        ContentPart::Text(s) | ContentPart::Reasoning(s) => estimate_str(s),
        // The loop never produces images; URL length is a harmless proxy.
        ContentPart::ImageUrl { url } => 64 + estimate_str(url),
        ContentPart::ToolCall(tc) => {
            PER_TOOL_PART_OVERHEAD_TOKENS + estimate_str(&tc.name) + estimate_str(&tc.arguments)
        }
        ContentPart::ToolResult(tr) => {
            PER_TOOL_PART_OVERHEAD_TOKENS + estimate_str(&tool_result_text(&tr.content))
        }
    }
}

pub fn estimate_messages(messages: &[Message]) -> u64 {
    messages.iter().map(estimate_message).sum()
}

/// Tool schemas ride along on every request but are invisible to
/// message-level estimation — needed only on the no-anchor path (see
/// [`estimate_context`]).
pub fn estimate_tool_specs(specs: &[ToolSpec]) -> u64 {
    specs
        .iter()
        .map(|s| {
            16 + estimate_str(&s.name)
                + s.description.as_deref().map(estimate_str).unwrap_or(0)
                + estimate_str(&s.input_schema.to_string())
        })
        .sum()
}

/// Estimated context size of the NEXT request.
///
/// * With an anchor — the provider-reported `input + output` of the request
///   that produced the anchor message — the estimate is anchor tokens plus a
///   local estimate of everything appended after that message. The system
///   prompt and tool schemas were already counted by the provider inside the
///   anchor's input tokens and must NOT be added again.
/// * Without an anchor (nothing in the view has usage yet, e.g. right after a
///   compaction) the full local estimate must explicitly include the system
///   prompt and tool schemas, or it would systematically under-count.
pub fn estimate_context(
    messages: &[Message],
    tool_specs: &[ToolSpec],
    anchor: Option<(u64, usize)>,
) -> u64 {
    match anchor {
        Some((tokens, idx)) if idx < messages.len() => {
            tokens + estimate_messages(&messages[idx + 1..])
        }
        _ => estimate_messages(messages) + estimate_tool_specs(tool_specs),
    }
}

fn tool_result_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

// ======================================================================
// Cut-point selection
// ======================================================================

/// Index into `messages` (the LLM view WITHOUT the system message) where the
/// verbatim tail starts: everything before it gets summarized. `None` when
/// there is nothing worth compacting (too short, or no legal cut).
///
/// A cut is legal only at a `User` or `Assistant` message — cutting at a
/// `Tool` row would orphan its result from the assistant tool-call that will
/// be summarized away (a protocol violation for the provider). The tail also
/// always covers at least the last complete exchange (trailing tool results
/// plus the assistant that issued them), even when a single huge tool result
/// blows the token budget.
pub fn find_cut_point(messages: &[Message], tail_keep: u64) -> Option<usize> {
    // Largest suffix that fits the tail budget: walk from the end until one
    // more message would overflow. `messages.len()` (keep everything) is the
    // default when the whole conversation fits.
    let mut acc = 0u64;
    let mut budget_cut = messages.len();
    for i in (0..messages.len()).rev() {
        let cost = estimate_message(&messages[i]);
        if acc + cost > tail_keep {
            budget_cut = i + 1;
            break;
        }
        acc += cost;
    }

    // Minimum tail: the last round (trailing tool results + the assistant
    // that issued the calls). The model cannot continue without the results
    // it just asked for.
    let min_cut = last_round_start(messages);

    // Budget cut must not go past the minimum tail (budget too small for the
    // last round → keep the round anyway), then advance to a legal boundary.
    let mut cut = budget_cut.min(min_cut);
    while cut < messages.len() && !is_legal_cut(messages, cut) {
        cut += 1;
    }

    if cut >= messages.len() || cut < MIN_MESSAGES_BEFORE_CUT {
        return None;
    }
    Some(cut)
}

/// Start index of the last complete exchange: the trailing block of tool
/// results plus the assistant message that issued them. When the last message
/// is not a tool result, this is simply `messages.len()` (no minimum beyond
/// the budget cut).
fn last_round_start(messages: &[Message]) -> usize {
    let mut i = messages.len();
    while i > 0 && messages[i - 1].role == Role::Tool {
        i -= 1;
    }
    if i < messages.len() && i > 0 && messages[i - 1].role == Role::Assistant {
        i -= 1;
    }
    i
}

fn is_legal_cut(messages: &[Message], i: usize) -> bool {
    matches!(messages[i].role, Role::User | Role::Assistant)
}

// ======================================================================
// Summary input serialization
// ======================================================================

/// Per-part caps (chars) when serializing history for the summary call.
const SUMMARY_TEXT_MAX_CHARS: usize = 8_000;
const SUMMARY_ARGS_MAX_CHARS: usize = 500;
const SUMMARY_RESULT_MAX_CHARS: usize = 1_500;

/// Total serialization cap when the window is unknown.
const DEFAULT_SERIALIZE_TOTAL_CHARS: usize = 400_000;

/// Render the compactable region as flat text for the summary model call.
/// Reasoning parts are dropped (process content, not task state); tool calls
/// are one-liners; tool results are truncated to their head. The whole thing
/// is capped (head + tail kept, middle dropped) so the summary request itself
/// can never overflow the window it is trying to free.
pub fn serialize_for_summary(region: &[Message], window: Option<u32>) -> String {
    let max_total = window
        .map(|w| w as usize * 3)
        .unwrap_or(DEFAULT_SERIALIZE_TOTAL_CHARS);

    let mut out = String::new();
    for msg in region {
        let role = match msg.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        };
        let mut wrote_header = false;
        for part in &msg.parts {
            let line = match part {
                ContentPart::Text(t) => Some(truncate_summary_text(t, SUMMARY_TEXT_MAX_CHARS)),
                // Reasoning is dropped: it is how the model thought, not what
                // the task state is.
                ContentPart::Reasoning(_) => None,
                ContentPart::ImageUrl { .. } => Some("[image]".to_string()),
                ContentPart::ToolCall(tc) => Some(format!(
                    "tool call {}({})",
                    tc.name,
                    truncate_summary_text(&tc.arguments, SUMMARY_ARGS_MAX_CHARS)
                )),
                ContentPart::ToolResult(tr) => Some(format!(
                    "tool result{}: {}",
                    if tr.is_error { " (error)" } else { "" },
                    truncate_summary_text(&tool_result_text(&tr.content), SUMMARY_RESULT_MAX_CHARS)
                )),
            };
            if let Some(line) = line {
                if !wrote_header {
                    out.push_str(&format!("[{role}]\n"));
                    wrote_header = true;
                }
                out.push_str(&line);
                out.push('\n');
            }
        }
    }

    cap_total_chars(out, max_total)
}

fn truncate_summary_text(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut cut: String = s.chars().take(max_chars).collect();
    cut.push_str("…[truncated]");
    cut
}

/// Cap the serialized transcript at `max_chars`, keeping the head and tail in
/// equal halves with an omission marker between: the head carries the user's
/// original goal, the tail the most recent progress.
fn cap_total_chars(s: String, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s;
    }
    let half = max_chars / 2;
    let head: String = s.chars().take(half).collect();
    let tail: String = s.chars().skip(s.chars().count() - half).collect();
    format!("{head}\n…[older content omitted]…\n{tail}")
}

// ======================================================================
// Summary model call
// ======================================================================

const SUMMARY_MAX_OUTPUT_TOKENS: u32 = 2_048;
const SUMMARY_TIMEOUT: Duration = Duration::from_secs(120);

const SUMMARY_SYSTEM_PROMPT: &str = "\
You are compacting the history of an AI coding-agent conversation so work can \
continue in a fresh context window. Summarize the conversation below for the \
agent's future context.\n\
Write the summary in the SAME language as the user's messages.\n\
Use exactly these sections as markdown headings:\n\
## Primary Request and Intent\n\
What the user asked for, as a bullet list — preserve every user request, in \
order, as close to verbatim as possible.\n\
## Work Completed\n\
Files read/created/edited (with paths), commands run, and their outcomes.\n\
## Key Tool Activity\n\
Important tool results, errors, and how they were resolved. Keep error \
messages and identifiers verbatim.\n\
## Current State and Next Steps\n\
What is done, what is in progress, and the immediate next action.\n\
## Important Paths and Identifiers\n\
File paths, branch names, ids, and other tokens likely to be referenced \
again.\n\
\n\
Rules:\n\
- Be concrete and information-dense; no pleasantries, no meta-commentary.\n\
- Prefer exact paths, numbers, and error strings over paraphrase.\n\
- Do not invent facts not present in the conversation.";

/// One-shot, tool-less, non-streaming summary call. Timeout is inside; cancel
/// responsiveness is the caller's job (wrap in `select!` against the run's
/// cancellation token).
pub async fn generate_summary(
    model: &dyn ChatModel,
    region: &[Message],
    window: Option<u32>,
) -> anyhow::Result<String> {
    let serialized = serialize_for_summary(region, window);
    let request = ChatRequest {
        messages: vec![
            Message::text(Role::System, SUMMARY_SYSTEM_PROMPT),
            Message::text(Role::User, serialized),
        ],
        tools: Vec::new(),
        options: GenerateOptions {
            max_output_tokens: Some(SUMMARY_MAX_OUTPUT_TOKENS),
            ..Default::default()
        },
    };

    let response = tokio::time::timeout(SUMMARY_TIMEOUT, model.generate(request))
        .await
        .map_err(|_| anyhow::anyhow!("summary model call timed out"))?
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let text: String = response
        .message
        .parts
        .iter()
        .filter_map(|p| match p {
            ContentPart::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    if text.trim().is_empty() {
        anyhow::bail!("summary model call returned no text");
    }
    Ok(text)
}

// ======================================================================
// Compaction marker row
// ======================================================================

const MARKER_PREFIX: &str = "[Context compaction] The earlier conversation \
history was compacted to free context window space. Below is a summary of \
everything before this point.";

const MARKER_SUFFIX: &str = "Continue the task based on the summary above and \
the recent messages that follow it.";

/// The marker row the LLM sees: an ordinary user message carrying the summary
/// (plus framing), so `load_llm_view` needs no special-casing when rebuilding
/// the view.
pub fn build_marker_message(summary: &str) -> Message {
    Message::text(
        Role::User,
        format!("{MARKER_PREFIX}\n\n<summary>\n{summary}\n</summary>\n\n{MARKER_SUFFIX}"),
    )
}

/// UI-only metadata stored in the marker row's details_json. The `kind` field
/// is what [`crate::db::messages::load_llm_view`] and the frontend use to
/// recognize the row.
pub fn marker_details(
    triggered_by: CompactionTrigger,
    compacted_message_count: usize,
    estimated_tokens_before: u64,
    estimated_tokens_after: u64,
) -> Value {
    json!({
        "kind": msg_db::COMPACTION_MARKER_KIND,
        "triggered_by": triggered_by,
        "compacted_message_count": compacted_message_count,
        "estimated_tokens_before": estimated_tokens_before,
        "estimated_tokens_after": estimated_tokens_after,
    })
}

// ======================================================================
// In-run compaction (auto trigger)
// ======================================================================

/// Result of one turn-top compaction attempt.
pub enum CompactAttempt {
    /// Compaction happened; the value is the new `compact_floor` (estimated
    /// context right after the rebuild).
    Done(u64),
    /// Nothing worth compacting (no legal cut); safe to re-check next turn.
    Nothing,
    /// The summary call or marker write failed; auto compaction is disabled
    /// for the rest of the run.
    Failed,
    /// Cancellation arrived mid-compaction; the caller must publish
    /// `Cancelled` and return.
    Cancelled,
}

/// Run one auto-compaction at a turn boundary: summarize `cut` messages,
/// persist the marker row, and rebuild `full_messages` to
/// `[system, marker, tail]`. On success the usage anchor resets to `None` —
/// no provider has ever seen the rebuilt history, so estimates fall back to
/// the (schema-aware) local path until the next real usage arrives.
#[allow(clippy::too_many_arguments)]
pub async fn run_auto_compaction(
    model: &dyn ChatModel,
    pool: &sqlx::SqlitePool,
    conv_id: &str,
    system_prompt: &str,
    hub: &EventHub,
    cancel: &CancellationToken,
    context_window: u32,
    tool_specs: &[ToolSpec],
    full_messages: &mut Vec<Message>,
    usage_anchor: &mut Option<(u64, usize)>,
    est_before: u64,
) -> CompactAttempt {
    // full_messages[0] is the system prompt; compaction works on the rest.
    let Some(cut) = find_cut_point(&full_messages[1..], tail_keep_tokens(context_window)) else {
        return CompactAttempt::Nothing;
    };
    let region = full_messages[1..1 + cut].to_vec();
    let tail = full_messages[1 + cut..].to_vec();

    hub.publish(AgentEvent::Compacting {
        triggered_by: CompactionTrigger::Auto,
    });

    let summary = tokio::select! {
        r = generate_summary(model, &region, Some(context_window)) => match r {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("auto compaction summary failed: {e}");
                return CompactAttempt::Failed;
            }
        },
        _ = cancel.cancelled() => return CompactAttempt::Cancelled,
    };
    if cancel.is_cancelled() {
        return CompactAttempt::Cancelled;
    }

    let marker = build_marker_message(&summary);
    let rebuilt: Vec<Message> = std::iter::once(Message::text(Role::System, system_prompt))
        .chain(std::iter::once(marker.clone()))
        .chain(tail)
        .collect();
    let est_after = estimate_context(&rebuilt, tool_specs, None);

    let details = marker_details(CompactionTrigger::Auto, cut, est_before, est_after);
    let stored =
        match msg_db::insert_message(pool, conv_id, &marker, None, None, Some(&details)).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("auto compaction failed to persist marker row: {e}");
                return CompactAttempt::Failed;
            }
        };

    *full_messages = rebuilt;
    *usage_anchor = None;
    hub.publish(AgentEvent::Compacted {
        summary_message_id: stored.id,
        triggered_by: CompactionTrigger::Auto,
        compacted_messages: cut as u32,
        est_tokens_before: est_before,
        est_tokens_after: est_after,
        summary,
        context_window_tokens: Some(context_window),
    });
    CompactAttempt::Done(est_after)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oct_llm_provider::core::{ToolCall, ToolResult};

    fn user(text: &str) -> Message {
        Message::text(Role::User, text)
    }

    fn assistant(text: &str) -> Message {
        Message::text(Role::Assistant, text)
    }

    fn assistant_with_call(id: &str) -> Message {
        Message {
            role: Role::Assistant,
            parts: vec![
                ContentPart::Text("running a tool".into()),
                ContentPart::ToolCall(ToolCall {
                    id: id.to_string(),
                    name: "read".to_string(),
                    arguments: "{}".to_string(),
                }),
            ],
        }
    }

    fn tool_result(id: &str, text: &str) -> Message {
        Message {
            role: Role::Tool,
            parts: vec![ContentPart::ToolResult(ToolResult {
                call_id: id.to_string(),
                content: Value::String(text.to_string()),
                is_error: false,
                images: Vec::new(),
            })],
        }
    }

    #[test]
    fn estimate_str_ascii_quarter_and_cjk_full() {
        assert_eq!(estimate_str("abcd"), 1);
        assert_eq!(estimate_str("ab"), 0); // rounds down, intentionally cheap
        assert_eq!(estimate_str("你好"), 2);
        assert_eq!(estimate_str("ab你好"), 2); // 2 ASCII → 0, plus 2 CJK
    }

    #[test]
    fn estimate_messages_counts_parts_and_overhead() {
        let msgs = [user("abcdabcd"), assistant_with_call("c1")];
        let est = estimate_messages(&msgs);
        // 2 messages → 2× overhead; text 8 ASCII → 2; tool call ~ name+args+8.
        assert!(est >= 2 * PER_MESSAGE_OVERHEAD_TOKENS + 2);
    }

    #[test]
    fn estimate_context_with_anchor_skips_tools_with_without_adds_them() {
        let msgs = vec![user("abcd"), assistant("efgh")];
        let tools = [ToolSpec {
            name: "read".to_string(),
            description: Some("read a file".to_string()),
            input_schema: json!({"type": "object"}),
        }];
        let tool_est = estimate_tool_specs(&tools);

        // Anchor on the assistant message (index 1): anchor tokens + nothing
        // after it; the tool schemas are already inside the anchor's real
        // input tokens and must NOT be added again.
        assert_eq!(estimate_context(&msgs, &tools, Some((100, 1))), 100);
        // No anchor: full estimate must include the schemas.
        assert_eq!(
            estimate_context(&msgs, &tools, None),
            estimate_messages(&msgs) + tool_est
        );
    }

    #[test]
    fn cut_point_splits_on_budget_at_legal_boundary() {
        // Alternating exchanges, each ~4 tokens ("abcd" = 1 + overhead).
        let mut msgs = Vec::new();
        for _ in 0..20 {
            msgs.push(user("abcd"));
            msgs.push(assistant("efgh"));
        }
        let cut = find_cut_point(&msgs, 24).expect("cut exists");
        assert!(cut >= MIN_MESSAGES_BEFORE_CUT);
        assert!(matches!(msgs[cut].role, Role::User | Role::Assistant));
        // Tail fits roughly within budget.
        assert!(estimate_messages(&msgs[cut..]) <= 24 + estimate_message(&msgs[cut]));
    }

    #[test]
    fn cut_point_never_splits_tool_call_from_result() {
        let mut msgs = Vec::new();
        for i in 0..8 {
            msgs.push(user("abcd"));
            msgs.push(assistant_with_call(&format!("c{i}")));
            msgs.push(tool_result(&format!("c{i}"), "result text"));
        }
        // Tiny budget: the tail is forced to the last round.
        let cut = find_cut_point(&msgs, 3).expect("cut exists");
        let tail = &msgs[cut..];
        // The tail starts at the last assistant (round start), not at its
        // tool results.
        assert_eq!(tail[0].role, Role::Assistant);
        // Every tool result in the tail has its call in the tail too.
        let call_ids: Vec<&str> = tail
            .iter()
            .flat_map(|m| {
                m.parts.iter().filter_map(|p| match p {
                    ContentPart::ToolCall(tc) => Some(tc.id.as_str()),
                    _ => None,
                })
            })
            .collect();
        for m in tail {
            for p in &m.parts {
                if let ContentPart::ToolResult(tr) = p {
                    assert!(
                        call_ids.contains(&tr.call_id.as_str()),
                        "tail orphaned tool result {}",
                        tr.call_id
                    );
                }
            }
        }
    }

    #[test]
    fn cut_point_huge_last_tool_result_stays_in_tail() {
        let mut msgs = Vec::new();
        for i in 0..6 {
            msgs.push(user("abcd"));
            msgs.push(assistant_with_call(&format!("c{i}")));
            msgs.push(tool_result(&format!("c{i}"), "x"));
        }
        // Last result alone dwarfs the budget; the round must still survive.
        msgs.push(assistant_with_call("big"));
        msgs.push(tool_result("big", &"y".repeat(100_000)));
        let cut = find_cut_point(&msgs, 100).expect("cut exists");
        // The last round (assistant + its huge result) survives verbatim.
        assert_eq!(cut, msgs.len() - 2, "last assistant+result kept verbatim");
        assert_eq!(msgs[cut].role, Role::Assistant);
    }

    #[test]
    fn cut_point_too_short_returns_none() {
        let msgs = [user("a"), assistant("b"), user("c"), assistant("d")];
        assert_eq!(find_cut_point(&msgs, 1_000), None);
    }

    #[test]
    fn serialize_drops_reasoning_and_truncates_results() {
        let region = [
            Message {
                role: Role::Assistant,
                parts: vec![
                    ContentPart::Reasoning("thinking...".into()),
                    ContentPart::Text("did a thing".into()),
                ],
            },
            Message {
                role: Role::Tool,
                parts: vec![ContentPart::ToolResult(ToolResult {
                    call_id: "c1".into(),
                    content: Value::String("x".repeat(5_000)),
                    is_error: true,
                    images: Vec::new(),
                })],
            },
        ];
        let out = serialize_for_summary(&region, None);
        assert!(!out.contains("thinking"), "reasoning dropped");
        assert!(out.contains("did a thing"));
        assert!(out.contains("tool result (error)"));
        assert!(out.contains("[truncated]"));
        assert!(out.len() < 5_000, "result truncated: {} bytes", out.len());
    }

    #[test]
    fn serialize_caps_total_with_head_and_tail_kept() {
        let mut region = Vec::new();
        for i in 0..50 {
            region.push(user(&format!("msg-{i:02} {}", "z".repeat(400))));
        }
        // window 1000 → total cap 3000 chars < ~20k of content: capped.
        let out = serialize_for_summary(&region, Some(1_000));
        assert!(out.contains("msg-00"), "head (user goal) kept");
        assert!(out.contains("msg-49"), "tail (recent progress) kept");
        assert!(out.contains("[older content omitted]"));
    }

    #[test]
    fn marker_message_and_details_shape() {
        let msg = build_marker_message("did stuff");
        assert_eq!(msg.role, Role::User);
        let ContentPart::Text(text) = &msg.parts[0] else {
            panic!("single text part");
        };
        assert!(text.contains("<summary>\ndid stuff\n</summary>"));

        let details = marker_details(CompactionTrigger::Manual, 12, 1000, 200);
        assert_eq!(details["kind"], msg_db::COMPACTION_MARKER_KIND);
        assert_eq!(details["triggered_by"], "manual");
        assert_eq!(details["compacted_message_count"], 12);
    }

    #[test]
    fn threshold_and_tail_budgets() {
        assert_eq!(trigger_threshold(200_000), 170_000);
        assert_eq!(tail_keep_tokens(200_000), 20_000);
        assert_eq!(tail_keep_tokens(40_000), 4_000, "10% of the window");
        assert_eq!(tail_keep_tokens(1_000_000), 32_000, "capped high");
        assert_eq!(
            tail_keep_tokens(500),
            50,
            "no floor: small windows compact too"
        );
    }
}
