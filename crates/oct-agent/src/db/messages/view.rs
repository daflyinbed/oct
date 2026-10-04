//! Materialize the model view and its usage anchor from the timeline.
use super::*;

pub(super) fn is_compaction_marker(m: &StoredMessage) -> bool {
    details_kind(m.details_json.as_deref()).as_deref() == Some(COMPACTION_MARKER_KIND)
}

/// Build the LLM view for a conversation (see [`LlmView`]).
///
/// Anchor selection and adjustment:
/// * eligibility — the last assistant row with usage whose
///   `appended_at_revision` is LATER than the marker's. A tail assistant
///   appended before the marker had the entire compacted-away prefix inside
///   its input tokens; treating it as the anchor after a reload would
///   over-estimate wildly. No post-marker usage yet (e.g. the run died right
///   after compaction) means no anchor — local schema-aware estimation takes
///   over, mirroring the in-run `anchor = None` reset.
/// * COW deduction — a swap must be subtracted from the anchor's provider
///   tokens iff the original content was counted by the anchor's request
///   (slot ordering earlier than the anchor's) AND the swap happened after
///   that request (`swapped_at_revision > anchor.appended_at_revision`).
///   Provider usage itself is never rewritten; the adjustment is recomputed
///   from persisted state on every load, so it survives crashes.
pub async fn load_llm_view(pool: &SqlitePool, conversation_id: &str) -> Result<LlmView> {
    let stored = list_messages(pool, conversation_id).await?;
    // The view starts AT the last marker (inclusive): the marker IS the
    // summary message the model sees. Older markers are inside the excluded
    // prefix and simply stop mattering.
    let marker_index = stored.iter().rposition(is_compaction_marker);
    let start = marker_index.unwrap_or(0);
    let region = &stored[start..];

    // Orphan tool results (call summarized away or repaired past the marker)
    // are a protocol violation for providers — drop them defensively. Calls
    // are only "seen" once their assistant message appears, so ordering is
    // enforced too.
    let mut messages: Vec<Message> = Vec::with_capacity(region.len());
    let mut kept: Vec<&StoredMessage> = Vec::with_capacity(region.len());
    let mut seen_calls: HashSet<String> = HashSet::new();
    for row in region {
        let parsed = row.to_message()?;
        if parsed.role == Role::Tool {
            let has_live_call = parsed.parts.iter().all(|p| match p {
                ContentPart::ToolResult(tr) => seen_calls.contains(&tr.call_id),
                _ => true,
            });
            if !has_live_call {
                continue;
            }
        } else {
            for part in &parsed.parts {
                if let ContentPart::ToolCall(tc) = part {
                    seen_calls.insert(tc.id.clone());
                }
            }
        }
        kept.push(row);
        messages.push(parsed);
    }
    let orderings: Vec<i64> = kept.iter().map(|row| row.ordering).collect();

    // Anchor: the last kept assistant row that carries usage AND was appended
    // after the marker row (when one exists). Tool rows never carry usage, so
    // dropped orphans cannot have been the anchor.
    let marker_revision = marker_index.map(|i| stored[i].appended_at_revision);
    let mut anchor: Option<(usize, &StoredMessage)> = None;
    for (mi, row) in kept.iter().enumerate() {
        if row.role == "assistant"
            && row.input_tokens.is_some_and(|v| v > 0)
            && marker_revision.is_none_or(|mr| row.appended_at_revision > mr)
        {
            anchor = Some((mi, row));
        }
    }

    let anchor = match anchor {
        Some((mi, row)) => {
            let tokens = cow_adjusted_anchor_tokens(pool, &kept, &messages, row).await?;
            Some(UsageAnchor {
                tokens,
                message_index: mi,
            })
        }
        None => None,
    };

    Ok(LlmView {
        messages,
        orderings,
        anchor,
    })
}

/// The anchor row's provider tokens minus the savings of every COW swap the
/// anchor's request counted but that happened after it (the rule documented
/// on [`load_llm_view`]). Savings are recomputed from the two content rows (a
/// pure function), never stored; a missing or unparsable original is skipped
/// — over-estimating is the safe direction for a trigger.
async fn cow_adjusted_anchor_tokens(
    pool: &SqlitePool,
    kept: &[&StoredMessage],
    messages: &[Message],
    anchor_row: &StoredMessage,
) -> Result<u64> {
    let tokens =
        (anchor_row.input_tokens.unwrap_or(0) + anchor_row.output_tokens.unwrap_or(0)) as u64;
    let anchor_ordering = anchor_row.ordering;
    let anchor_revision = anchor_row.appended_at_revision;
    let is_affected = |r: &StoredMessage| {
        r.swapped_at_revision.is_some_and(|s| s > anchor_revision)
            && r.ordering < anchor_ordering
            && r.supersedes_message_id.is_some()
    };
    let original_ids: Vec<&str> = kept
        .iter()
        .filter(|r| is_affected(r))
        .filter_map(|r| r.supersedes_message_id.as_deref())
        .collect();
    if original_ids.is_empty() {
        return Ok(tokens);
    }
    let ids_json = serde_json::to_string(&original_ids)?;
    let original_rows = sqlx::query!(
        "SELECT id, parts_json FROM messages WHERE id IN (SELECT value FROM json_each($1))",
        ids_json,
    )
    .fetch_all(pool)
    .await?;
    let mut savings = 0u64;
    for (r, current) in kept.iter().zip(messages) {
        if !is_affected(r) {
            continue;
        }
        let Some(original) = original_rows
            .iter()
            .find(|o| o.id == r.supersedes_message_id.as_deref().expect("checked above"))
            .and_then(|o| serde_json::from_str::<Vec<ContentPart>>(&o.parts_json).ok())
            .map(|parts| Message {
                role: Role::Tool,
                parts,
            })
        else {
            continue;
        };
        savings += estimate_message(&original).saturating_sub(estimate_message(current));
    }
    Ok(tokens.saturating_sub(savings))
}

pub async fn load_messages_for_llm(
    pool: &SqlitePool,
    conversation_id: &str,
) -> Result<Vec<Message>> {
    Ok(load_llm_view(pool, conversation_id).await?.messages)
}
