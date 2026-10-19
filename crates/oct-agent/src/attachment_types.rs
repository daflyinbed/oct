//! Neutral attachment/message contracts. No database, filesystem or HTTP work
//! belongs here; persistence, history operations and APIs share these types.

use oct_llm_provider::core::{ContentPart, Message};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;
use utoipa::ToSchema;

pub const ATTACHMENT_URL_PREFIX: &str = "oct-attachment://";

/// An upload endpoint returns a ticket id; a committed message/draft returns
/// an immutable asset id. Both are accepted by the byte-read endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct AttachmentDto {
    pub id: String,
    pub filename: String,
    pub mime_type: String,
    pub size_bytes: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct TextSnapshot {
    pub text: String,
    pub truncated: bool,
    pub included_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct MessageAttachment {
    pub asset: AttachmentDto,
    /// Frozen at send time; changing model/window never rewrites history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<TextSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct UserContent {
    pub body: String,
    pub attachments: Vec<MessageAttachment>,
}

impl UserContent {
    pub fn to_message(&self) -> Message {
        let mut parts = Vec::new();
        if !self.body.trim().is_empty() || self.attachments.is_empty() {
            parts.push(ContentPart::Text(self.body.clone()));
        }
        for attachment in &self.attachments {
            parts.push(match &attachment.snapshot {
                Some(snapshot) => ContentPart::Text(snapshot.text.clone()),
                None => ContentPart::ImageUrl {
                    url: attachment_url(&attachment.asset.id),
                },
            });
        }
        Message {
            role: oct_llm_provider::core::Role::User,
            parts,
        }
    }

    /// Persist snapshot text once, in the typed user-content envelope. The
    /// lightweight parts remain readable by old clients and pin resource ids.
    pub fn storage_message(&self) -> Message {
        let mut message = self.to_message();
        let start = usize::from(!self.body.trim().is_empty() || self.attachments.is_empty());
        for (index, attachment) in (start..).zip(&self.attachments) {
            if attachment.snapshot.is_some() {
                message.parts[index] = ContentPart::Text(format!(
                    "[attachment: {}; full file: {}]",
                    attachment.asset.filename,
                    attachment_url(&attachment.asset.id)
                ));
            }
        }
        message
    }
    /// The explicit content contract is authoritative for new rows. Positional
    /// recovery is isolated to this v1 reader and never used by new writers.
    pub fn from_stored(parts_json: &str, details_json: Option<&str>) -> Option<Self> {
        let details: Value = serde_json::from_str(details_json?).ok()?;
        if let Some(content) = details.get("user_content") {
            return serde_json::from_value(content.clone()).ok();
        }
        let attachments: Vec<AttachmentDto> =
            serde_json::from_value(details.get("attachments")?.clone()).ok()?;
        let parts: Vec<ContentPart> = serde_json::from_str(parts_json).ok()?;
        let body_len = parts.len().checked_sub(attachments.len())?;
        let body = parts[..body_len]
            .iter()
            .filter_map(|part| match part {
                ContentPart::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        Some(Self {
            body,
            attachments: attachments
                .into_iter()
                .zip(&parts[body_len..])
                .map(|(asset, part)| MessageAttachment {
                    asset,
                    snapshot: match part {
                        ContentPart::Text(text) => Some(TextSnapshot {
                            text: text.clone(),
                            truncated: text.contains("[truncated at"),
                            included_bytes: 0,
                        }),
                        _ => None,
                    },
                })
                .collect(),
        })
    }
}

pub fn attachment_url(id: &str) -> String {
    format!("{ATTACHMENT_URL_PREFIX}{id}")
}

/// Hosted URLs and inline data URLs bypass attachment storage entirely.
pub fn parse_attachment_id(url: &str) -> Option<&str> {
    url.strip_prefix(ATTACHMENT_URL_PREFIX).filter(|id| {
        !id.is_empty()
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    })
}

/// Ownership is indexed in the same transaction as every content-row insert.
/// Reference-shaped text also pins bytes, preserving legacy full-file hints.
pub fn referenced_assets(message: &Message, details: Option<&Value>) -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    fn visit(value: &Value, ids: &mut BTreeSet<String>) {
        match value {
            Value::String(text) => {
                let mut rest = text.as_str();
                while let Some(index) = rest.find(ATTACHMENT_URL_PREFIX) {
                    rest = &rest[index + ATTACHMENT_URL_PREFIX.len()..];
                    let len = rest
                        .bytes()
                        .take_while(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
                        .count();
                    if len > 0 {
                        ids.insert(rest[..len].to_owned());
                    }
                    rest = &rest[len..];
                }
            }
            Value::Array(values) => {
                for value in values {
                    visit(value, ids);
                }
            }
            Value::Object(values) => {
                for value in values.values() {
                    visit(value, ids);
                }
            }
            _ => {}
        }
    }
    if let Ok(parts) = serde_json::to_value(&message.parts) {
        visit(&parts, &mut ids);
    }
    if let Some(details) = details {
        if let Some(atts) = details.get("attachments").and_then(Value::as_array) {
            for att in atts {
                if let Some(id) = att.get("id").and_then(Value::as_str) {
                    ids.insert(id.into());
                }
            }
        }
        if let Some(content) = details
            .get("user_content")
            .and_then(|v| serde_json::from_value::<UserContent>(v.clone()).ok())
        {
            ids.extend(content.attachments.into_iter().map(|att| att.asset.id));
        }
    }
    ids
}
