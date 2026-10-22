use super::{policy::*, *};
use crate::attachment_types::{AttachmentDto, MessageAttachment, TextSnapshot, UserContent};
use oct_llm_provider::core::{ContentPart, Message, Role, ToolCall, ToolResult};
use serde_json::json;
use std::collections::HashMap;

fn row(id: &str, role: Role, parts: Vec<ContentPart>, revision: i64) -> HistoryEntry {
    HistoryEntry {
        content: HistoryContent {
            id: id.into(),
            role: match role {
                Role::System => "system",
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::Tool => "tool",
            }
            .into(),
            parts_json: serde_json::to_string(&parts).unwrap(),
            details_json: None,
            supersedes_message_id: None,
        },
        appended_at_revision: revision,
        swapped_at_revision: None,
    }
}
fn text(id: &str, role: Role, revision: i64) -> HistoryEntry {
    row(id, role, vec![ContentPart::Text(id.into())], revision)
}
fn calls(ids: &[&str], revision: i64) -> HistoryEntry {
    row(
        "calls",
        Role::Assistant,
        ids.iter()
            .map(|id| {
                ContentPart::ToolCall(ToolCall {
                    id: (*id).into(),
                    name: "read_file".into(),
                    arguments: "{}".into(),
                })
            })
            .collect(),
        revision,
    )
}
fn result(id: &str, call: &str, revision: i64) -> HistoryEntry {
    row(
        id,
        Role::Tool,
        vec![ContentPart::ToolResult(ToolResult {
            call_id: call.into(),
            content: json!(id),
            is_error: false,
            images: Vec::new(),
        })],
        revision,
    )
}
fn cut(id: &str, side: HistorySide) -> HistoryCut {
    HistoryCut::Message {
        message_id: id.into(),
        side,
    }
}
fn snapshot(entries: Vec<HistoryEntry>) -> HistorySnapshot {
    HistorySnapshot {
        revision: entries
            .iter()
            .map(|row| row.appended_at_revision)
            .max()
            .unwrap_or(0),
        entries,
        originals: HashMap::new(),
    }
}

#[test]
fn parallel_tool_round_has_only_one_completed_cut() {
    let history = snapshot(vec![
        text("prompt", Role::User, 1),
        calls(&["a", "b"], 2),
        result("first", "a", 3),
        result("last", "b", 4),
    ]);
    assert!(matches!(
        plan_cut(&history, &cut("first", HistorySide::After)),
        Err(HistoryPolicyError::InvalidCut(_))
    ));
    let cuts = tool_cuts(&history.entries);
    assert_eq!(cuts.len(), 1);
    assert_eq!(cuts["last"], cut("last", HistorySide::After));
    let plan = plan_cut(&history, &cuts["last"]).unwrap();
    assert_eq!(plan.prefix.len(), 4);
    assert_eq!(plan.cut_revision, 4);
}

fn cow_history() -> HistorySnapshot {
    let original = result("original", "a", 3).content;
    let mut copy = result("cleared", "a", 3);
    copy.content.supersedes_message_id = Some(original.id.clone());
    copy.swapped_at_revision = Some(7);
    let mut marker = text("summary", Role::User, 8);
    marker.content.details_json = Some(json!({"kind":"compaction"}).to_string());
    let mut history = snapshot(vec![
        text("prompt", Role::User, 1),
        marker,
        calls(&["a"], 2),
        copy,
        text("answer", Role::Assistant, 4),
    ]);
    history.originals.insert(original.id.clone(), original);
    history
}

#[test]
fn revision_filters_reordered_markers_and_restores_original_membership() {
    let history = cow_history();
    let historical = plan_cut(&history, &cut("answer", HistorySide::After)).unwrap();
    assert_eq!(
        historical
            .prefix
            .iter()
            .map(|slot| slot.message_id.as_str())
            .collect::<Vec<_>>(),
        ["prompt", "calls", "original", "answer"]
    );
    assert_eq!(historical.prefix[2].appended_at_revision, 3);
    assert_eq!(historical.prefix[2].swapped_at_revision, None);
    let head = plan_cut(&history, &HistoryCut::Head).unwrap();
    assert_eq!(head.prefix[1].message_id, "summary");
    assert_eq!(head.prefix[3].message_id, "cleared");
    assert_eq!(head.prefix[3].swapped_at_revision, Some(7));
    assert_eq!(
        history.entries[3].content.id, "cleared",
        "planning never mutates the snapshot"
    );
}

#[test]
fn originals_are_required_only_for_a_cut_that_restores_them_and_must_be_one_hop() {
    let mut history = cow_history();
    history.originals.clear();
    assert!(plan_cut(&history, &HistoryCut::Head).is_ok());
    assert!(
        matches!(plan_cut(&history, &cut("answer", HistorySide::After)), Err(HistoryPolicyError::InvalidCut(message)) if message == "Missing copy-on-write original")
    );
    let mut original = result("original", "a", 3).content;
    original.supersedes_message_id = Some("older".into());
    history.originals.insert(original.id.clone(), original);
    assert!(
        matches!(plan_cut(&history, &cut("answer", HistorySide::After)), Err(HistoryPolicyError::InvalidCut(message)) if message == "Invalid copy-on-write chain")
    );
}

#[test]
fn before_user_extracts_body_and_assets_without_materializing_frozen_attachment_text() {
    let asset = AttachmentDto {
        id: "asset-1".into(),
        filename: "notes.txt".into(),
        mime_type: "text/plain".into(),
        size_bytes: 100,
    };
    let content = UserContent {
        body: "edit this question".into(),
        attachments: vec![MessageAttachment {
            asset: asset.clone(),
            snapshot: Some(TextSnapshot {
                text: "frozen private attachment text".into(),
                truncated: false,
                included_bytes: 30,
            }),
        }],
    };
    let Message { parts, .. } = content.storage_message();
    let mut target = row("target", Role::User, parts, 3);
    target.content.details_json = Some(json!({"user_content":content}).to_string());
    let history = snapshot(vec![
        text("prompt", Role::User, 1),
        text("answer", Role::Assistant, 2),
        target,
    ]);
    let plan = plan_cut(&history, &cut("target", HistorySide::Before)).unwrap();
    assert_eq!(plan.draft.as_deref(), Some("edit this question"));
    assert_eq!(plan.draft_attachments, [asset]);
    assert_eq!(plan.cut_revision, 2);
    assert_eq!(plan.prefix.len(), 2);
}

#[test]
fn malformed_history_and_noop_revert_fail_without_a_database() {
    let mut history = snapshot(vec![
        text("prompt", Role::User, 1),
        text("answer", Role::Assistant, 2),
    ]);
    assert!(matches!(
        plan_revert(&history, &HistoryCut::Head),
        Err(HistoryPolicyError::InvalidCut(_))
    ));
    assert!(matches!(
        plan_revert(&history, &cut("answer", HistorySide::After)),
        Err(HistoryPolicyError::InvalidCut(_))
    ));
    assert!(matches!(
        plan_cut(&history, &cut("foreign", HistorySide::After)),
        Err(HistoryPolicyError::MessageNotFound)
    ));
    history.entries[0].content.parts_json = "invalid json".into();
    assert!(
        matches!(plan_cut(&history, &HistoryCut::Head), Err(HistoryPolicyError::InvalidCut(message)) if message == "Malformed message in fork history")
    );
}

#[test]
fn command_receipts_preserve_the_previous_serialized_request_shape() {
    let fork = json!({"operation_id":"old-operation", "expected_revision":4, "cut":{"kind":"message", "message_id":"old-message", "side":"before"}, "title":null});
    let parsed: ForkRequest = serde_json::from_value(fork.clone()).unwrap();
    assert_eq!(serde_json::to_value(parsed).unwrap(), fork);
    let revert = json!({"operation_id":"old-operation", "expected_revision":4, "cut":{"kind":"message", "message_id":"old-message", "side":"after"}});
    let parsed: RevertRequest = serde_json::from_value(revert.clone()).unwrap();
    assert_eq!(serde_json::to_value(parsed).unwrap(), revert);
}
