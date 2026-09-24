//! Context compaction: summarize old conversation history into a single
//! marker message when the context window runs low.
//!
//! Trigger policy lives with the callers — the agent loop's turn-top check
//! (before every LLM request) and the manual API endpoint; this module owns
//! the pure pieces (token estimation, threshold/tail budgets, cut-point
//! selection, summary-input serialization, the one-shot summary model call,
//! marker-message construction) plus [`run_compaction`], the ONE shared
//! transaction (generate → persist → rebuild) both entry points run. The
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
use crate::context::ContextEntry;
use crate::db::messages as msg_db;

// Estimation lives in the neutral `context` module (the DB layer's COW anchor
// deduction needs it too — db → agent::compact was a reverse dependency).
// Re-exported here so existing call sites keep their paths.
pub use crate::context::{
    TOOL_IMAGE_ESTIMATE_TOKENS, estimate_context, estimate_message, estimate_messages,
    estimate_str, estimate_tool_specs, tool_result_text,
};

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
/// the budget cut). Shared with microcompact's protection window.
pub(crate) fn last_round_start(messages: &[Message]) -> usize {
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
// Compaction service (the shared transaction)
// ======================================================================

/// Inputs to one compaction transaction, shared by the manual API endpoint
/// and the agent loop's auto trigger. `entries` is the conversation WITHOUT
/// the system prompt, each message at its timeline ordering (the loop's
/// [`crate::context::ContextState`] conversation, or the LlmView zipped the
/// same way on the manual path); `est_before` is the caller's estimate of the
/// current context — the same figure its trigger compared against the
/// threshold.
///
/// Estimation口径, explicitly stated: `est_after` is always the
/// REBUILT-context estimate on the basis the next request will be estimated
/// with — `[system?, marker, tail]` plus tool specs, no anchor (no provider
/// has ever seen the rebuilt history). The manual path passes neither system
/// prompt nor tool specs (it runs outside a run and has neither at hand), so
/// its figure covers messages only and is correspondingly lower — display
/// figures, not accounting.
pub struct CompactRequest<'a> {
    pub entries: &'a [ContextEntry],
    pub est_before: u64,
    /// Present on the in-run path; included in the rebuilt estimate.
    pub system_prompt: Option<&'a str>,
    /// Present on the in-run path; included in the rebuilt estimate.
    pub tool_specs: &'a [ToolSpec],
    /// Context window, when known. Drives the tail budget; `None` (manual
    /// with an unresolvable window) falls back to
    /// [`DEFAULT_TAIL_KEEP_TOKENS`].
    pub window: Option<u32>,
    pub trigger: CompactionTrigger,
    /// Event hub; when present the transaction publishes `Compacting`
    /// (after the cut is found, before the summary call) and `Compacted`
    /// (after the marker row landed). The manual endpoint passes `None`.
    pub hub: Option<&'a EventHub>,
    /// Cancellation; the summary call is selected against it. The manual
    /// endpoint passes `None`.
    pub cancel: Option<&'a CancellationToken>,
}

/// Typed result of one compaction transaction.
pub struct CompactOutcome {
    pub summary_message_id: String,
    /// Cut index into the input `entries` (region is `..cut`, tail `cut..`).
    pub cut: usize,
    /// The rebuilt conversation entries — marker at the cut slot, tail rows
    /// shifted +1 — ready for
    /// [`crate::context::ContextState::apply_compaction`].
    pub rebuilt: Vec<ContextEntry>,
    pub est_before: u64,
    pub est_after: u64,
    pub summary: String,
}

pub enum CompactFailure {
    /// No legal cut for the tail budget — too short, nothing worth
    /// compacting.
    Nothing,
    /// The summary model call failed.
    Summary(String),
    /// Cancellation arrived mid-compaction.
    Cancelled,
    /// The marker row failed to persist (nothing half-written: the summary
    /// is never stored outside the marker row's own insert).
    Persist(String),
}

/// Run the one shared compaction transaction: pick the cut, summarize the
/// region, persist the marker row INTO the timeline at the cut (between the
/// summarized region and the tail — appending it at the end would place the
/// tail before the marker and drop it from every future `load_llm_view`),
/// and rebuild the entries to `[marker, tail+1]`. Callers own trigger
/// policy, error mapping and re-entry guards.
pub async fn run_compaction(
    model: &dyn ChatModel,
    pool: &sqlx::SqlitePool,
    conv_id: &str,
    req: CompactRequest<'_>,
) -> Result<CompactOutcome, CompactFailure> {
    let tail_keep = req
        .window
        .map(tail_keep_tokens)
        .unwrap_or(DEFAULT_TAIL_KEEP_TOKENS);
    // Selection works on plain messages; compaction is rare enough that the
    // one-shot materialization beats threading entries through every pure
    // helper.
    let messages: Vec<Message> = req.entries.iter().map(|e| e.message.clone()).collect();
    let Some(cut) = find_cut_point(&messages, tail_keep) else {
        return Err(CompactFailure::Nothing);
    };
    let region = messages[..cut].to_vec();
    // The DB ordering of the first tail message: the marker takes over this
    // slot and the tail shifts up by one.
    let cut_ordering = req.entries[cut].ordering;

    if let Some(hub) = req.hub {
        hub.publish(AgentEvent::Compacting {
            triggered_by: req.trigger,
        });
    }

    let summary = match req.cancel {
        Some(cancel) => tokio::select! {
            r = generate_summary(model, &region, req.window) => match r {
                Ok(s) => s,
                Err(e) => return Err(CompactFailure::Summary(e.to_string())),
            },
            _ = cancel.cancelled() => return Err(CompactFailure::Cancelled),
        },
        None => generate_summary(model, &region, req.window)
            .await
            .map_err(|e| CompactFailure::Summary(e.to_string()))?,
    };
    if req.cancel.is_some_and(|c| c.is_cancelled()) {
        return Err(CompactFailure::Cancelled);
    }

    let marker = build_marker_message(&summary);
    let rebuilt: Vec<ContextEntry> = std::iter::once(ContextEntry {
        message: marker.clone(),
        ordering: cut_ordering,
    })
    .chain(req.entries[cut..].iter().map(|e| ContextEntry {
        message: e.message.clone(),
        ordering: e.ordering + 1,
    }))
    .collect();
    let est_after = crate::context::estimate_view(
        &rebuilt,
        req.system_prompt,
        req.tool_specs,
        None,
    );

    let details = marker_details(req.trigger, cut, req.est_before, est_after);
    let stored = match msg_db::insert_message_at_ordering(
        pool,
        conv_id,
        &marker,
        None,
        None,
        Some(&details),
        cut_ordering,
    )
    .await
    {
        Ok(s) => s,
        Err(e) => return Err(CompactFailure::Persist(e.to_string())),
    };

    if let Some(hub) = req.hub {
        hub.publish(AgentEvent::Compacted {
            summary_message_id: stored.id.clone(),
            triggered_by: req.trigger,
            compacted_messages: cut as u32,
            est_tokens_before: req.est_before,
            est_tokens_after: est_after,
            summary: summary.clone(),
            context_window_tokens: req.window,
        });
    }

    Ok(CompactOutcome {
        summary_message_id: stored.id,
        cut,
        rebuilt,
        est_before: req.est_before,
        est_after,
        summary,
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

/// Auto trigger wrapper around [`run_compaction`]: same window always, the
/// run's system prompt and tool specs in the rebuilt estimate, the run's hub
/// and cancellation token wired through. Loop-owned state lives on the
/// [`ContextState`] the wrapper mutates (marker splice + anchor reset).
pub async fn run_auto_compaction(
    model: &dyn ChatModel,
    pool: &sqlx::SqlitePool,
    conv_id: &str,
    hub: &EventHub,
    cancel: &CancellationToken,
    context_window: u32,
    tool_specs: &[ToolSpec],
    context: &mut crate::context::ContextState,
    est_before: u64,
) -> CompactAttempt {
    let outcome = match run_compaction(
        model,
        pool,
        conv_id,
        CompactRequest {
            entries: context.conversation(),
            est_before,
            system_prompt: Some(context.system_prompt()),
            tool_specs,
            window: Some(context_window),
            trigger: CompactionTrigger::Auto,
            hub: Some(hub),
            cancel: Some(cancel),
        },
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(CompactFailure::Nothing) => return CompactAttempt::Nothing,
        Err(CompactFailure::Cancelled) => return CompactAttempt::Cancelled,
        Err(CompactFailure::Summary(e)) => {
            tracing::warn!("auto compaction summary failed: {e}");
            return CompactAttempt::Failed;
        }
        Err(CompactFailure::Persist(e)) => {
            tracing::warn!("auto compaction failed to persist marker row: {e}");
            return CompactAttempt::Failed;
        }
    };

    // Marker at the cut slot, tail shifted +1, anchor reset — the same
    // mutations the DB transaction performed, applied to the in-run view.
    context.apply_compaction(outcome.rebuilt);
    CompactAttempt::Done(outcome.est_after)
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
        assert!(est >= 2 * crate::context::PER_MESSAGE_OVERHEAD_TOKENS + 2);
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
