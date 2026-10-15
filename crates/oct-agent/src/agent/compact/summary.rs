//! Budgeted transcript serialization and the one-shot summary model call.
use crate::context::tool_result_text;
use oct_llm_provider::core::{ContentPart, GenerateOptions, Message, Role};
use oct_llm_provider::model::{ChatModel, ChatRequest};
use std::time::Duration;

// ======================================================================
// Summary input serialization
// ======================================================================

/// Per-part caps (chars) when serializing history for the summary call.
const SUMMARY_TEXT_MAX_CHARS: usize = 8_000;
const SUMMARY_ARGS_MAX_CHARS: usize = 500;
const SUMMARY_RESULT_MAX_CHARS: usize = 1_500;

/// Fallback transcript budget when the model's window is unknown.
const DEFAULT_SUMMARY_INPUT_TOKENS: u64 = 100_000;

struct SummaryBudget {
    input_tokens: u64,
    output_tokens: u32,
}

fn summary_budget(window: Option<u32>) -> SummaryBudget {
    let output_tokens = window
        .map(|w| (w / 4).clamp(1, SUMMARY_MAX_OUTPUT_TOKENS))
        .unwrap_or(SUMMARY_MAX_OUTPUT_TOKENS);
    let input_tokens = window.map_or(DEFAULT_SUMMARY_INPUT_TOKENS, |w| {
        // Reserve the complete prompt, output and 10% for estimation/wire
        // overhead. Small windows also need a smaller output allowance.
        u64::from(w)
            .saturating_sub(u64::from(w).div_ceil(10))
            .saturating_sub(summary_text_tokens(SUMMARY_SYSTEM_PROMPT))
            .saturating_sub(2 * crate::context::PER_MESSAGE_OVERHEAD_TOKENS)
            .saturating_sub(u64::from(output_tokens))
    });
    SummaryBudget {
        input_tokens,
        output_tokens,
    }
}

/// Quarter-token units avoid rounding each ASCII character up. Count every
/// non-ASCII UTF-8 byte conservatively: CJK and emoji must not be budgeted
/// as interchangeable single-token characters. This remains a heuristic;
/// the request budget separately reserves a safety margin.
fn summary_char_units(c: char) -> u64 {
    if c.is_ascii() {
        1
    } else {
        c.len_utf8() as u64 * 4
    }
}

fn summary_text_tokens(s: &str) -> u64 {
    s.chars().map(summary_char_units).sum::<u64>().div_ceil(4)
}

/// Render the compactable region as flat text for the summary model call.
/// Reasoning parts are dropped (process content, not task state); tool calls
/// are one-liners; tool results are truncated to their head. The whole thing
/// is capped by its token budget (head + tail kept, middle dropped), reserving
/// room for the summary prompt and output instead of assuming chars = tokens.
pub fn serialize_for_summary(region: &[Message], window: Option<u32>) -> String {
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
                ContentPart::Reasoning(_) | ContentPart::ReasoningBlock(_) => None,
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

    cap_summary_tokens(out, summary_budget(window).input_tokens)
}

fn truncate_summary_text(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut cut: String = s.chars().take(max_chars).collect();
    cut.push_str("…[truncated]");
    cut
}

/// Keep the head (original goal) and tail (recent progress) without splitting
/// UTF-8. The omission marker itself is included in the budget.
fn cap_summary_tokens(s: String, budget: u64) -> String {
    if summary_text_tokens(&s) <= budget {
        return s;
    }
    const OMITTED: &str = "\n…[older content omitted]…\n";
    let marker_cost = summary_text_tokens(OMITTED);
    if budget < marker_cost {
        return summary_prefix(&s, budget).to_string();
    }
    let remaining = budget - marker_cost;
    let head = summary_prefix(&s, remaining / 2);
    let tail = summary_suffix(&s, remaining - remaining / 2);
    format!("{head}{OMITTED}{tail}")
}

fn summary_prefix(s: &str, budget: u64) -> &str {
    let mut units = 0;
    for (index, c) in s.char_indices() {
        units += summary_char_units(c);
        if units > budget.saturating_mul(4) {
            return &s[..index];
        }
    }
    s
}

fn summary_suffix(s: &str, budget: u64) -> &str {
    let mut units = 0;
    for (index, c) in s.char_indices().rev() {
        units += summary_char_units(c);
        if units > budget.saturating_mul(4) {
            return &s[index + c.len_utf8()..];
        }
    }
    s
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
    let budget = summary_budget(window);
    if budget.input_tokens == 0 {
        anyhow::bail!("context window is too small for the summary prompt and output");
    }
    let serialized = serialize_for_summary(region, window);
    let request = ChatRequest {
        messages: vec![
            Message::text(Role::System, SUMMARY_SYSTEM_PROMPT),
            Message::text(Role::User, serialized),
        ],
        tools: Vec::new(),
        options: GenerateOptions {
            max_output_tokens: Some(
                model
                    .info()
                    .limits
                    .max_output_tokens
                    .filter(|limit| *limit > 0)
                    .map_or(budget.output_tokens, |limit| {
                        limit.min(budget.output_tokens)
                    }),
            ),
            ..Default::default()
        },
    };

    // The with_tags scope covers the generate() call, where the wire ctx
    // captures purpose=compact (on top of the run's conversation tags).
    let mut compact_tags = oct_llm_provider::wire::current_tags();
    compact_tags.purpose = Some("compact".to_string());
    let response = tokio::time::timeout(
        SUMMARY_TIMEOUT,
        oct_llm_provider::wire::with_tags(compact_tags, model.generate(request)),
    )
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

#[cfg(test)]
mod tests {
    use super::*;
    use oct_llm_provider::core::ToolResult;
    use serde_json::Value;
    fn user(text: &str) -> Message {
        Message::text(Role::User, text)
    }
    fn assistant(text: &str) -> Message {
        Message::text(Role::Assistant, text)
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
        // A 1000-token window must reserve the prompt and output too.
        let out = serialize_for_summary(&region, Some(1_000));
        assert!(out.contains("msg-00"), "head (user goal) kept");
        assert!(out.contains("msg-49"), "tail (recent progress) kept");
        assert!(out.contains("[older content omitted]"));
    }

    #[test]
    fn summary_serialization_reserves_prompt_output_and_margin_for_unicode() {
        for text in [
            "汉".repeat(8_000),
            "🧑‍💻".repeat(4_000),
            "abc汉😀".repeat(4_000),
        ] {
            let mut region = vec![user(&format!("original-goal {text}")); 10];
            region.push(assistant("latest-progress"));
            for window in [500, 1_000, 8_000, 32_000] {
                let budget = summary_budget(Some(window));
                let out = serialize_for_summary(&region, Some(window));
                let request_tokens = summary_text_tokens(SUMMARY_SYSTEM_PROMPT)
                    + 2 * crate::context::PER_MESSAGE_OVERHEAD_TOKENS
                    + summary_text_tokens(&out);
                assert!(
                    request_tokens
                        + u64::from(budget.output_tokens)
                        + u64::from(window).div_ceil(10)
                        <= u64::from(window),
                    "summary request exceeds window {window}"
                );
                assert!(out.contains("original-goal"), "preserve the goal");
                assert!(out.contains("latest-progress"), "preserve recent progress");
                assert!(out.contains("[older content omitted]"));
            }
        }
    }

    #[test]
    fn summary_truncation_accounts_for_marker_and_small_utf8_budgets() {
        let input = "goal: 汉😀ascii🧑‍💻 progress".repeat(20);
        for budget in 0..100 {
            let out = cap_summary_tokens(input.clone(), budget);
            assert!(summary_text_tokens(&out) <= budget, "budget {budget}");
        }
        assert_eq!(summary_budget(Some(128)).input_tokens, 0);
    }
}
