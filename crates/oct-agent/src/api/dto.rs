//! The typed message contract of the messages API.
//!
//! The DB stores `parts_json`/`details_json` as opaque strings (Rust serde's
//! externally-tagged `ContentPart` representation, plus per-tool metadata);
//! serving those raw forced the frontend to re-implement the discrimination
//! with no compile-time checks. These DTOs are the public contract instead:
//! parts are tagged by `type`, the known metadata kinds (compaction marker,
//! microcompact-cleared copy, reasoning duration) are discriminated enums,
//! and anything unrecognized passes through unchanged — old rows and future
//! tool-specific metadata keep working without a migration.
//!
//! UI-only metadata never enters model-visible messages: the DTO layer only
//! READS what tools already persisted; `ContentPart` stays the single shape
//! the agent loop and providers exchange.

use serde::Serialize;
use serde_json::Value;
use utoipa::ToSchema;

use oct_llm_provider::core::ContentPart;

use crate::agent::CompactionTrigger;
use crate::db::messages::{MessageContent, StoredMessage};

/// One message part of the messages API, tagged by `type`.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MessagePart {
    Text {
        text: String,
    },
    Reasoning {
        text: String,
    },
    ImageUrl {
        url: String,
    },
    ToolCall {
        id: String,
        name: String,
        /// JSON object text, exactly as the model produced it.
        arguments: String,
    },
    ToolResult {
        call_id: String,
        /// Model-visible payload: usually a string, JSON for non-string
        /// contents.
        content: Value,
        is_error: bool,
        /// Base64 attachments passed through to the model as multimodal
        /// tool-result content.
        #[serde(skip_serializing_if = "Vec::is_empty", default)]
        images: Vec<ToolResultImage>,
    },
}

/// One base64 image attached to a tool result.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ToolResultImage {
    pub mime_type: String,
    pub data: String,
}

impl From<&ContentPart> for MessagePart {
    fn from(part: &ContentPart) -> Self {
        match part {
            ContentPart::Text(text) => MessagePart::Text { text: text.clone() },
            ContentPart::Reasoning(text) => MessagePart::Reasoning { text: text.clone() },
            ContentPart::ImageUrl { url } => MessagePart::ImageUrl { url: url.clone() },
            ContentPart::ToolCall(call) => MessagePart::ToolCall {
                id: call.id.clone(),
                name: call.name.clone(),
                arguments: call.arguments.clone(),
            },
            ContentPart::ToolResult(result) => MessagePart::ToolResult {
                call_id: result.call_id.clone(),
                content: result.content.clone(),
                is_error: result.is_error,
                images: result
                    .images
                    .iter()
                    .map(|img| ToolResultImage {
                        mime_type: img.mime_type.clone(),
                        data: img.data.clone(),
                    })
                    .collect(),
            },
        }
    }
}

/// The metadata of a message row as the API serves it: one object with a
/// discriminating `kind` for the kinds known structurally, and everything
/// the API does not model passed through verbatim in `extra` (tool-specific
/// UI hints such as titles or truncation flags — the same free-form object
/// the live `tool_result` event's `details` carries).
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct MessageDetails {
    /// The known structural kind, when the row carries one; null for plain
    /// tool-metadata rows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<KnownDetailsKind>,
    /// Present on compaction marker rows (`kind = "compaction"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compaction: Option<CompactionMetadata>,
    /// Present when `kind = "reasoning_duration"` (assistant rows).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_duration_ms: Option<u64>,
    /// Unrecognized fields, verbatim.
    #[serde(skip_serializing_if = "serde_json::Map::is_empty", default)]
    pub extra: serde_json::Map<String, Value>,
}

/// Metadata kinds the API understands structurally.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum KnownDetailsKind {
    /// Compaction marker row: the summary message the LLM view starts at
    /// (written by the shared compaction transaction).
    Compaction,
    /// Microcompact placeholder copy: the tool result this row replaced was
    /// cleared from the context (the original stays reachable via the
    /// `supersedes_message_id` back-pointer — see the agent's
    /// `MicrocompactReplacement` event payload).
    MicrocompactCleared,
    /// A tool result the permission gate refused: the model
    /// saw the denial text; the frontend renders the "denied" badge.
    PermissionDenied,
    /// Assistant-row thinking duration (older rows store this without a
    /// `kind` field; recognized structurally here).
    ReasoningDuration,
}

/// Compaction marker metadata (mirrors what the `compacted` SSE event
/// carries, minus the summary text itself which lives in the parts).
#[derive(Debug, Clone, Serialize, serde::Deserialize, ToSchema)]
pub struct CompactionMetadata {
    pub triggered_by: CompactionTrigger,
    pub compacted_message_count: u32,
    pub estimated_tokens_before: u64,
    pub estimated_tokens_after: u64,
}

/// Parse a stored `parts_json` (the serialized `Vec<ContentPart>`).
/// Unparseable/legacy payloads surface as a single text part carrying the
/// raw string — history never fails to load because one row is corrupt.
fn parse_parts(parts_json: &str) -> Vec<MessagePart> {
    serde_json::from_str::<Vec<ContentPart>>(parts_json)
        .map(|parts| parts.iter().map(MessagePart::from).collect())
        .unwrap_or_else(|_| {
            vec![MessagePart::Text {
                text: parts_json.to_string(),
            }]
        })
}

/// Recognize the stored metadata: known kinds land in their typed fields,
/// everything else (including fields of a partially-parseable known kind)
/// passes through `extra` — old rows and future tool metadata keep working
/// without a migration.
fn parse_details(details_json: Option<&str>) -> Option<MessageDetails> {
    let raw: Value = serde_json::from_str(details_json?).ok()?;
    let Value::Object(map) = raw else {
        // Non-object metadata (never produced today): nothing to serve.
        return None;
    };
    let mut extra = map.clone();
    let kind = match map.get("kind").and_then(Value::as_str) {
        Some("compaction") => {
            extra.remove("kind");
            if let Ok(compaction) =
                serde_json::from_value::<CompactionMetadata>(serde_json::Value::Object(map.clone()))
            {
                return Some(MessageDetails {
                    kind: Some(KnownDetailsKind::Compaction),
                    compaction: Some(compaction),
                    reasoning_duration_ms: None,
                    extra: extra_without(
                        &extra,
                        &[
                            "triggered_by",
                            "compacted_message_count",
                            "estimated_tokens_before",
                            "estimated_tokens_after",
                        ],
                    ),
                });
            }
            None
        }
        Some("microcompact_cleared") => {
            extra.remove("kind");
            return Some(MessageDetails {
                kind: Some(KnownDetailsKind::MicrocompactCleared),
                compaction: None,
                reasoning_duration_ms: None,
                extra,
            });
        }
        Some("permission_denied") => {
            extra.remove("kind");
            return Some(MessageDetails {
                kind: Some(KnownDetailsKind::PermissionDenied),
                compaction: None,
                reasoning_duration_ms: None,
                extra,
            });
        }
        Some("reasoning_duration") => Some(KnownDetailsKind::ReasoningDuration),
        _ => None,
    };
    let mut reasoning_duration_ms = None;
    if let Some(ms) = map.get("reasoning_duration_ms").and_then(Value::as_u64) {
        reasoning_duration_ms = Some(ms);
        extra.remove("reasoning_duration_ms");
        if kind.is_none() {
            // Older rows store the duration without a `kind` field.
            return Some(MessageDetails {
                kind: None,
                compaction: None,
                reasoning_duration_ms,
                extra,
            });
        }
    }
    Some(MessageDetails {
        kind,
        compaction: None,
        reasoning_duration_ms,
        extra,
    })
}

/// Drop the given keys from a passthrough map (they were consumed by a typed
/// field).
fn extra_without(
    extra: &serde_json::Map<String, Value>,
    consumed: &[&str],
) -> serde_json::Map<String, Value> {
    extra
        .iter()
        .filter(|(k, _)| !consumed.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// One message row of the messages API: the stored content parsed into
/// typed parts plus the row's identity, usage and membership-relevant
/// fields. `parts_json`/`details_json` never leave the backend.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct MessageDto {
    pub id: String,
    pub role: String,
    pub parts: Vec<MessagePart>,
    pub details: Option<MessageDetails>,
    /// Timeline slot within the conversation. `None` on original-content
    /// responses (a superseded row no longer has a membership slot).
    pub ordering: Option<i64>,
    pub provider_id: Option<String>,
    pub model_id: Option<String>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub reasoning_tokens: Option<i64>,
    /// Copy-on-write back-pointer: names the original content row this
    /// cleared copy superseded (exactly one hop).
    pub supersedes_message_id: Option<String>,
    /// Authoritative persisted cut; streaming placeholders never have one.
    pub fork_cut: Option<crate::db::history::ForkCut>,
    /// Shared history has no runtime ownership in the fork.
    pub inherited: Option<bool>,
    pub created_at: chrono::NaiveDateTime,
}

impl MessageDto {
    /// Parse a stored row. Unparseable/legacy `parts_json` surfaces as a
    /// single text part carrying the raw string — history never fails to
    /// load because one row is corrupt.
    pub fn from_stored(row: &StoredMessage) -> Self {
        Self {
            id: row.id.clone(),
            role: row.role.clone(),
            parts: parse_parts(&row.parts_json),
            details: parse_details(row.details_json.as_deref()),
            ordering: Some(row.ordering),
            provider_id: row.provider_id.clone(),
            model_id: row.model_id.clone(),
            input_tokens: row.input_tokens,
            output_tokens: row.output_tokens,
            reasoning_tokens: row.reasoning_tokens,
            supersedes_message_id: row.supersedes_message_id.clone(),
            fork_cut: crate::db::history::message_cut(row),
            inherited: None,
            created_at: row.created_at,
        }
    }

    /// The original-content response of the microcompact "view original"
    /// endpoint: same contract as [`Self::from_stored`] over a pure content
    /// row (no usage or membership fields).
    pub fn from_content(row: &MessageContent) -> Self {
        Self {
            id: row.id.clone(),
            role: row.role.clone(),
            parts: parse_parts(&row.parts_json),
            details: parse_details(row.details_json.as_deref()),
            ordering: None,
            provider_id: None,
            model_id: None,
            input_tokens: None,
            output_tokens: None,
            reasoning_tokens: None,
            supersedes_message_id: None,
            fork_cut: None,
            inherited: None,
            created_at: row.created_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stored_row(role: &str, parts_json: &str, details_json: Option<&str>) -> StoredMessage {
        StoredMessage {
            id: "m1".into(),
            conversation_id: "c1".into(),
            role: role.into(),
            parts_json: parts_json.into(),
            details_json: details_json.map(str::to_string),
            ordering: 1,
            provider_id: None,
            model_id: None,
            input_tokens: None,
            output_tokens: None,
            reasoning_tokens: None,
            supersedes_message_id: None,
            appended_at_revision: 1,
            swapped_at_revision: None,
            created_at: chrono::Utc::now().naive_utc(),
        }
    }

    #[test]
    fn parses_typed_parts_and_known_details() {
        let row = stored_row(
            "tool",
            r#"[{"ToolResult":{"call_id":"c1","content":"out","is_error":false,"images":[]}}]"#,
            Some(r#"{"kind":"microcompact_cleared"}"#),
        );
        let dto = MessageDto::from_stored(&row);
        let details = dto.details.expect("details");
        assert_eq!(details.kind, Some(KnownDetailsKind::MicrocompactCleared));
        assert!(details.compaction.is_none());
        match &dto.parts[0] {
            MessagePart::ToolResult {
                call_id, content, ..
            } => {
                assert_eq!(call_id, "c1");
                assert_eq!(*content, serde_json::json!("out"));
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn parses_compaction_marker_and_reasoning_duration() {
        let row = stored_row(
            "user",
            r#"[{"Text":"marker"}]"#,
            Some(
                r#"{"kind":"compaction","triggered_by":"manual","compacted_message_count":9,"estimated_tokens_before":100,"estimated_tokens_after":20}"#,
            ),
        );
        let dto = MessageDto::from_stored(&row);
        let details = dto.details.expect("details");
        assert_eq!(details.kind, Some(KnownDetailsKind::Compaction));
        let compaction = details.compaction.expect("compaction metadata");
        assert_eq!(compaction.triggered_by, CompactionTrigger::Manual);
        assert_eq!(compaction.compacted_message_count, 9);
        assert_eq!(compaction.estimated_tokens_before, 100);
        assert_eq!(compaction.estimated_tokens_after, 20);
        assert!(details.extra.is_empty(), "consumed fields not duplicated");

        // Older rows store the duration without a `kind` field.
        let row = stored_row(
            "assistant",
            r#"[{"Reasoning":"thought"}]"#,
            Some(r#"{"reasoning_duration_ms":1500}"#),
        );
        let dto = MessageDto::from_stored(&row);
        let details = dto.details.expect("details");
        assert_eq!(details.reasoning_duration_ms, Some(1500));
        assert!(details.kind.is_none());
    }

    #[test]
    fn parses_permission_denied_marker() {
        // The permission gate stamps refused tool results.
        let row = stored_row(
            "tool",
            r#"[{"ToolResult":{"call_id":"c1","content":"Permission denied by rule: /etc/hosts","is_error":true,"images":[]}}]"#,
            Some(r#"{"kind":"permission_denied"}"#),
        );
        let dto = MessageDto::from_stored(&row);
        let details = dto.details.expect("details");
        assert_eq!(details.kind, Some(KnownDetailsKind::PermissionDenied));
        assert!(
            details.extra.is_empty(),
            "kind consumed, nothing duplicated"
        );
    }

    #[test]
    fn unknown_details_pass_through_and_corrupt_parts_degrade() {
        let row = stored_row(
            "tool",
            r#"[{"ToolResult":{"call_id":"c1","content":"o","is_error":false,"images":[]}}]"#,
            Some(r#"{"title":"cat a.txt","truncated":true}"#),
        );
        let dto = MessageDto::from_stored(&row);
        let details = dto.details.expect("details");
        assert!(details.kind.is_none());
        assert_eq!(
            details.extra.get("truncated"),
            Some(&serde_json::json!(true))
        );
        assert_eq!(
            details.extra.get("title"),
            Some(&serde_json::json!("cat a.txt"))
        );

        // Legacy/corrupt payload: one raw text part, never a load failure.
        let row = stored_row("user", "not json at all", None);
        let dto = MessageDto::from_stored(&row);
        assert!(matches!(
            dto.parts[0],
            MessagePart::Text { ref text } if text == "not json at all"
        ));
    }
}
