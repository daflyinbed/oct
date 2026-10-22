//! Pure stable-cut, tool-batch, draft and COW normalization rules.
use super::types::*;
use crate::attachment_types::UserContent;
use crate::context::details_kind;
use oct_llm_provider::core::ContentPart;
use std::collections::{HashMap, HashSet};

impl HistoryContent {
    fn to_message(&self) -> anyhow::Result<oct_llm_provider::core::Message> {
        super::content::parse_message(&self.role, &self.parts_json, self.details_json.as_deref())
    }
}

fn invalid(message: &str) -> HistoryPolicyError {
    HistoryPolicyError::InvalidCut(message.into())
}

/// Only real user prompts and complete assistant replies offer message cuts.
pub fn message_cut(row: &HistoryEntry) -> Option<HistoryCut> {
    let message = row.content.to_message().ok()?;
    let side = match row.content.role.as_str() {
        "user" if details_kind(row.content.details_json.as_deref()).is_none() => {
            HistorySide::Before
        }
        "assistant"
            if !message
                .parts
                .iter()
                .any(|part| matches!(part, ContentPart::ToolCall(_))) =>
        {
            HistorySide::After
        }
        _ => return None,
    };
    Some(HistoryCut::Message {
        message_id: row.content.id.clone(),
        side,
    })
}

/// A stopped tool round is a stable cut only after EVERY result has landed.
/// The DTO names the final result row; the UI attaches it to its assistant card.
pub fn tool_cuts(rows: &[HistoryEntry]) -> HashMap<String, HistoryCut> {
    let mut validator = PrefixValidator::default();
    let mut cuts = HashMap::new();
    for row in rows {
        if validator.append(&row.content).is_err() {
            break;
        }
        if row.content.role == "tool" && validator.pending.is_empty() {
            cuts.insert(
                row.content.id.clone(),
                HistoryCut::Message {
                    message_id: row.content.id.clone(),
                    side: HistorySide::After,
                },
            );
        }
    }
    cuts
}

#[derive(Clone, Copy)]
struct CutBoundary {
    length: usize,
    revision: i64,
}

impl CutBoundary {
    fn entries(self, rows: &[HistoryEntry]) -> impl Iterator<Item = &HistoryEntry> {
        rows.iter()
            .take(self.length)
            .filter(move |row| row.appended_at_revision <= self.revision)
    }

    fn restores(self, row: &HistoryEntry) -> bool {
        row.swapped_at_revision
            .is_some_and(|swapped| swapped > self.revision)
    }
}

fn cut_boundary(
    snapshot: &HistorySnapshot,
    cut: &HistoryCut,
) -> Result<CutBoundary, HistoryPolicyError> {
    let rows = &snapshot.entries;
    let (length, revision) = match cut {
        HistoryCut::Head => {
            if rows
                .last()
                .is_some_and(|row| message_cut(row).is_none() && row.content.role != "tool")
            {
                return Err(invalid(
                    "The conversation does not end at a stable message or tool result boundary",
                ));
            }
            (rows.len(), snapshot.revision)
        }
        HistoryCut::Message { message_id, side } => {
            let index = rows
                .iter()
                .position(|row| &row.content.id == message_id)
                .ok_or(HistoryPolicyError::MessageNotFound)?;
            let row = &rows[index];
            if message_cut(row).as_ref() != Some(cut)
                && !(row.content.role == "tool" && *side == HistorySide::After)
            {
                return Err(invalid("Unsupported fork point"));
            }
            match side {
                HistorySide::Before => (index, row.appended_at_revision - 1),
                HistorySide::After => (index + 1, row.appended_at_revision),
            }
        }
    };
    Ok(CutBoundary { length, revision })
}

/// Select only originals the historical prefix needs, before repository IO.
/// Loading and planning must use the same membership snapshot and transaction.
pub(crate) fn original_ids_to_restore<'a>(
    snapshot: &'a HistorySnapshot,
    cut: &HistoryCut,
) -> Result<HashSet<&'a str>, HistoryPolicyError> {
    // Head retains current content and never needs historical originals.
    if matches!(cut, HistoryCut::Head) {
        return Ok(HashSet::new());
    }
    let boundary = cut_boundary(snapshot, cut)?;
    boundary
        .entries(&snapshot.entries)
        .filter(|row| boundary.restores(row))
        .map(|row| {
            row.content
                .supersedes_message_id
                .as_deref()
                .ok_or_else(|| invalid("Missing copy-on-write original"))
        })
        .collect()
}

/// Shared by fork and revert: restore the historical prefix without mutating
/// content. The repository loads and applies this plan in one transaction.
pub fn plan_cut(
    snapshot: &HistorySnapshot,
    cut: &HistoryCut,
) -> Result<HistoryPlan, HistoryPolicyError> {
    let boundary = cut_boundary(snapshot, cut)?;
    let mut draft_attachments = Vec::new();
    let (draft, target) = match cut {
        HistoryCut::Head => (None, None),
        HistoryCut::Message {
            message_id,
            side: HistorySide::Before,
        } => {
            let row = &snapshot.entries[boundary.length];
            let content = UserContent::from_stored(
                &row.content.parts_json,
                row.content.details_json.as_deref(),
            );
            let text = if let Some(content) = content {
                draft_attachments = content
                    .attachments
                    .into_iter()
                    .map(|att| att.asset)
                    .collect();
                content.body
            } else {
                row.content
                    .to_message()?
                    .parts
                    .iter()
                    .filter_map(|part| match part {
                        ContentPart::Text(text) => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            (Some(text), Some(message_id.clone()))
        }
        HistoryCut::Message { message_id, .. } => (None, Some(message_id.clone())),
    };
    let mut prefix = Vec::new();
    let mut content = Vec::new();
    for row in boundary.entries(&snapshot.entries) {
        let restored = boundary.restores(row);
        let selected = if restored {
            let original = row
                .content
                .supersedes_message_id
                .as_deref()
                .and_then(|id| snapshot.originals.get(id))
                .ok_or_else(|| invalid("Missing copy-on-write original"))?;
            if original.supersedes_message_id.is_some() {
                return Err(invalid("Invalid copy-on-write chain"));
            }
            original
        } else {
            &row.content
        };
        prefix.push(HistoryMembership {
            message_id: selected.id.clone(),
            appended_at_revision: row.appended_at_revision,
            swapped_at_revision: if restored {
                None
            } else {
                row.swapped_at_revision
            },
        });
        content.push(selected);
    }
    validate_prefix(&content)?;
    Ok(HistoryPlan {
        prefix,
        cut_revision: boundary.revision,
        draft,
        draft_attachments,
        target,
    })
}

pub fn plan_revert(
    snapshot: &HistorySnapshot,
    cut: &HistoryCut,
) -> Result<HistoryPlan, HistoryPolicyError> {
    if matches!(cut, HistoryCut::Head) {
        return Err(invalid("Revert requires a historical message cut"));
    }
    let plan = plan_cut(snapshot, cut)?;
    if plan.prefix.len() == snapshot.entries.len()
        && plan
            .prefix
            .iter()
            .zip(&snapshot.entries)
            .all(|(slot, row)| slot.message_id == row.content.id)
    {
        return Err(invalid(
            "The selected cut does not change conversation history",
        ));
    }
    Ok(plan)
}

fn validate_prefix(rows: &[&HistoryContent]) -> Result<(), HistoryPolicyError> {
    let mut validator = PrefixValidator::default();
    for row in rows {
        validator.append(row)?;
    }
    if !validator.pending.is_empty() {
        return Err(invalid("Fork point splits a tool call and its result"));
    }
    Ok(())
}

#[derive(Default)]
struct PrefixValidator {
    pending: HashSet<String>,
    seen: HashSet<String>,
}

impl PrefixValidator {
    fn append(&mut self, row: &HistoryContent) -> Result<(), HistoryPolicyError> {
        let message = row
            .to_message()
            .map_err(|_| invalid("Malformed message in fork history"))?;
        if row.role == "tool"
            && (message.parts.is_empty()
                || message
                    .parts
                    .iter()
                    .any(|part| !matches!(part, ContentPart::ToolResult(_))))
        {
            return Err(invalid("Malformed tool result in fork history"));
        }
        if row.role != "tool" && !self.pending.is_empty() {
            return Err(invalid("Fork history contains unfinished tool calls"));
        }
        for part in message.parts {
            match part {
                ContentPart::ToolCall(call) => {
                    if row.role != "assistant" || !self.seen.insert(call.id.clone()) {
                        return Err(invalid("Invalid tool call in fork history"));
                    }
                    self.pending.insert(call.id);
                }
                ContentPart::ToolResult(result)
                    if (row.role != "tool" || !self.pending.remove(&result.call_id)) =>
                {
                    return Err(invalid("Unpaired tool result in fork history"));
                }
                _ => {}
            }
        }
        Ok(())
    }
}
