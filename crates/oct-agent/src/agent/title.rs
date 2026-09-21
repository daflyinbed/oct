//! One-shot background conversation-title generation.
//!
//! Fires alongside the first user message's agent run but is fully decoupled
//! from it: an independent, tool-less, NON-streaming model call — the loop
//! never knows a title is being written. Guard/trigger logic (default title?
//! first user message?) lives with the chat endpoint; this module owns the
//! prompt, output normalization, the fallback, and the write-then-publish
//! order. Failure never escalates: the fallback title is used, and if even
//! that is impossible the conversation keeps its default title.

use std::sync::Weak;
use std::time::Duration;

use tracing::debug;

use oct_llm_provider::core::{ContentPart, GenerateOptions, Message, Role};
use oct_llm_provider::model::{ChatModel, ChatRequest};

use super::{AgentEvent, EventHub};
use crate::db::conversations as conv_db;

/// Cap on the first user message text sent to the model (chars).
const INPUT_MAX_CHARS: usize = 2000;
/// Cap for a generated or fallback title (chars, cut on a char boundary).
const TITLE_MAX_CHARS: usize = 80;
/// Fallback titles take this many chars of the first user message.
const FALLBACK_MAX_CHARS: usize = 50;
/// Hard timeout for the model call; on expiry the fallback title is used.
const MODEL_TIMEOUT: Duration = Duration::from_secs(30);

const TITLE_PROMPT: &str = "\
Generate a short title for the conversation the user message below starts.\n\
- At most 50 characters (about 10 words).\n\
- Use the SAME language as the user's message.\n\
- Describe what the user wants to do; keep technical terms, file names, numbers and error codes exact.\n\
- Output ONLY the title: no quotes, no explanation, no markdown, no trailing punctuation.";

/// Everything the background task owns. The hub is held WEAKLY on purpose:
/// holding an `Arc` would keep the run's hub (and with it every SSE body)
/// alive past the run's end until this call finishes or times out. If the run
/// is already gone when the title lands, publishing is skipped — the DB has
/// the title and the next conversation-list fetch picks it up.
pub struct TitleJob {
    pub model: Box<dyn ChatModel>,
    pub pool: sqlx::SqlitePool,
    pub conversation_id: String,
    pub first_user_content: String,
    pub hub: Weak<EventHub>,
}

/// Fire-and-forget: detach a task that generates the title and applies it.
pub fn spawn(job: TitleJob) {
    tokio::spawn(async move {
        run(job).await;
    });
}

async fn run(job: TitleJob) {
    let generated = call_model(&job).await;
    let title = match &generated {
        Ok(text) => normalize_title(text).or_else(|| fallback_title(&job.first_user_content)),
        Err(e) => {
            // High-frequency background path: debug only, never error-level.
            debug!("title generation failed, using fallback: {e}");
            fallback_title(&job.first_user_content)
        }
    };
    let Some(title) = title else {
        return;
    };

    // Write first, publish second: anyone who sees the event can also see
    // the row. `Ok(false)` means the user renamed in the meantime — their
    // name wins and no event is sent.
    match conv_db::set_auto_title(&job.pool, &job.conversation_id, &title).await {
        Ok(true) => {
            if let Some(hub) = job.hub.upgrade() {
                hub.publish(AgentEvent::TitleUpdated { title });
            }
        }
        Ok(false) => {}
        Err(e) => debug!("failed to persist generated title: {e}"),
    }
}

async fn call_model(job: &TitleJob) -> anyhow::Result<String> {
    let input = truncate_chars(&job.first_user_content, INPUT_MAX_CHARS);
    let request = ChatRequest {
        messages: vec![
            Message::text(Role::System, TITLE_PROMPT),
            Message::text(Role::User, input),
        ],
        tools: Vec::new(),
        options: GenerateOptions {
            max_output_tokens: Some(100),
            ..Default::default()
        },
    };

    let response = tokio::time::timeout(MODEL_TIMEOUT, job.model.generate(request))
        .await
        .map_err(|_| anyhow::anyhow!("title model call timed out"))?
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    Ok(response
        .message
        .parts
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect())
}

/// Clean a raw model reply into a usable title: first non-empty line,
/// wrapping quote pairs (straight/curly/CJK) stripped, whitespace collapsed,
/// capped on a char boundary. `None` when nothing usable remains.
fn normalize_title(raw: &str) -> Option<String> {
    let first_line = raw.lines().map(str::trim).find(|l| !l.is_empty())?;
    let stripped = strip_wrapping_quotes(first_line).trim();
    let collapsed = collapse_whitespace(stripped);
    if collapsed.is_empty() {
        return None;
    }
    Some(truncate_chars(&collapsed, TITLE_MAX_CHARS))
}

/// Fallback when the model call fails or returns nothing usable: the first
/// user message itself, whitespace-collapsed and truncated (ellipsis when
/// cut). `None` only for blank input.
fn fallback_title(content: &str) -> Option<String> {
    let collapsed = collapse_whitespace(content);
    if collapsed.is_empty() {
        return None;
    }
    if collapsed.chars().count() > FALLBACK_MAX_CHARS {
        let cut: String = collapsed.chars().take(FALLBACK_MAX_CHARS).collect();
        Some(format!("{cut}…"))
    } else {
        Some(collapsed)
    }
}

fn collapse_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Truncate to at most `max` CHARS (never splitting a multi-byte char).
fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

const QUOTE_OPEN: &[char] = &['"', '\'', '`', '“', '‘', '「', '『', '《'];
const QUOTE_CLOSE: &[char] = &['"', '\'', '`', '”', '’', '」', '』', '》'];

/// Strip matched wrapping quote pairs repeatedly (`"x"` → `x`, `"‘x’"` → `x`).
/// A lone quote character or an unmatched pair is left alone.
fn strip_wrapping_quotes(mut s: &str) -> &str {
    loop {
        let mut chars = s.chars();
        let (Some(first), Some(last)) = (chars.next(), chars.next_back()) else {
            break;
        };
        let matched = QUOTE_OPEN
            .iter()
            .position(|c| *c == first)
            .zip(QUOTE_CLOSE.iter().position(|c| *c == last))
            .is_some_and(|(open, close)| open == close);
        if !matched {
            break;
        }
        s = s[first.len_utf8()..s.len() - last.len_utf8()].trim();
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_takes_first_line_and_trims() {
        assert_eq!(
            normalize_title("  Fix login bug  \nand other text"),
            Some("Fix login bug".to_string())
        );
        assert_eq!(
            normalize_title("\n\n\nonly later line"),
            Some("only later line".to_string())
        );
    }

    #[test]
    fn normalize_strips_wrapping_quote_pairs() {
        assert_eq!(
            normalize_title("\"Fix login\""),
            Some("Fix login".to_string())
        );
        assert_eq!(
            normalize_title("“修复登录按钮”"),
            Some("修复登录按钮".to_string())
        );
        assert_eq!(
            normalize_title("‘`nested quotes`’"),
            Some("nested quotes".to_string())
        );
        // Unmatched/lone quotes stay.
        assert_eq!(
            normalize_title("it's broken"),
            Some("it's broken".to_string())
        );
    }

    #[test]
    fn normalize_collapses_internal_whitespace() {
        // Whitespace collapses within the (single) line; a newline starts a
        // new line and first-line extraction applies instead.
        assert_eq!(
            normalize_title("fix   the\t broken   thing"),
            Some("fix the broken thing".to_string())
        );
        assert_eq!(
            normalize_title("fix   the\nbroken   thing"),
            Some("fix the".to_string())
        );
    }

    #[test]
    fn normalize_rejects_empty_and_caps_length() {
        assert_eq!(normalize_title("   \n  \t "), None);
        assert_eq!(normalize_title("\"\""), None);

        let long = "x".repeat(200);
        let title = normalize_title(&long).expect("capped, not rejected");
        assert_eq!(title.chars().count(), TITLE_MAX_CHARS);
    }

    #[test]
    fn fallback_truncates_with_ellipsis_and_never_splits_chars() {
        assert_eq!(
            fallback_title("  hello   world \n next line "),
            Some("hello world next line".to_string())
        );
        assert_eq!(fallback_title("   "), None);

        // CJK chars count as one each; the cut lands on a boundary.
        let content = "修".repeat(60);
        let title = fallback_title(&content).expect("fallback");
        assert_eq!(title.chars().count(), FALLBACK_MAX_CHARS + 1); // + ellipsis
        assert!(title.ends_with('…'));
        assert!(title.chars().take(FALLBACK_MAX_CHARS).all(|c| c == '修'));
    }
}
