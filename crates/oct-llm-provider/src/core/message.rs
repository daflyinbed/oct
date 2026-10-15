use serde::{Deserialize, Serialize};

use crate::core::tool::{ToolCall, ToolResult};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// Provider-authenticated reasoning carried unchanged through persistence.
/// Only the matching adapter should replay the opaque signature/data.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ReasoningBlock {
    Signed {
        text: String,
        signature: String,
        /// Local request provenance, persisted but never sent to the provider.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prefix_fingerprint: Option<String>,
    },
    Redacted {
        data: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prefix_fingerprint: Option<String>,
    },
}

impl ReasoningBlock {
    pub fn text(&self) -> Option<&str> {
        match self {
            Self::Signed { text, .. } => Some(text),
            Self::Redacted { .. } => None,
        }
    }

    pub(crate) fn bind_prefix(&mut self, fingerprint: &str) {
        match self {
            Self::Signed {
                prefix_fingerprint, ..
            }
            | Self::Redacted {
                prefix_fingerprint, ..
            } => {
                *prefix_fingerprint = Some(fingerprint.to_string());
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ContentPart {
    Text(String),
    ImageUrl { url: String },
    Reasoning(String),
    ReasoningBlock(ReasoningBlock),
    ToolCall(ToolCall),
    ToolResult(ToolResult),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Message {
    pub role: Role,
    pub parts: Vec<ContentPart>,
}

impl Message {
    pub fn new(role: Role, parts: Vec<ContentPart>) -> Self {
        Self { role, parts }
    }

    pub fn text(role: Role, text: impl Into<String>) -> Self {
        Self {
            role,
            parts: vec![ContentPart::Text(text.into())],
        }
    }
}
