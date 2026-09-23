use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ToolChoice {
    Auto,
    None,
    Required,
    Named(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// One image attached to a tool result, passed through to the model as
/// multimodal tool-result content (e.g. an MCP server returning an image
/// content block). Providers map it to their protocol's native form:
/// Anthropic base64 image blocks inside the `tool_result` content array,
/// OpenAI-compatible APIs a follow-up user message with data URLs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolResultImage {
    /// MIME type of the image, e.g. `image/png`.
    pub mime_type: String,
    /// Base64-encoded image bytes.
    pub data: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolResult {
    pub call_id: String,
    pub content: Value,
    pub is_error: bool,
    /// Images attached to this result, delivered alongside the text content.
    /// Skipped on the wire (and in persisted parts_json) when empty, so
    /// text-only results keep their exact pre-images JSON shape.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ToolResultImage>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pre-images parts_json (no `images` key) must keep deserializing: the
    /// field defaults to empty instead of failing the whole message replay.
    #[test]
    fn tool_result_deserializes_without_images() {
        let raw = r#"{"call_id":"call_1","content":"done","is_error":false}"#;
        let result: ToolResult = serde_json::from_str(raw).unwrap();
        assert_eq!(result.call_id, "call_1");
        assert_eq!(result.content, Value::String("done".to_string()));
        assert!(!result.is_error);
        assert!(result.images.is_empty());
    }

    /// Empty images are skipped on serialization: the JSON form of a
    /// text-only result is byte-identical to the pre-images shape; a result
    /// with images round-trips losslessly.
    #[test]
    fn tool_result_skips_empty_images_and_round_trips() {
        let text_only = ToolResult {
            call_id: "call_1".to_string(),
            content: Value::String("done".to_string()),
            is_error: false,
            images: Vec::new(),
        };
        let json = serde_json::to_string(&text_only).unwrap();
        assert!(!json.contains("images"), "got: {json}");

        let with_images = ToolResult {
            images: vec![ToolResultImage {
                mime_type: "image/png".to_string(),
                data: "aGVsbG8=".to_string(),
            }],
            ..text_only
        };
        let json = serde_json::to_string(&with_images).unwrap();
        assert!(json.contains("\"images\""), "got: {json}");
        assert_eq!(
            serde_json::from_str::<ToolResult>(&json).unwrap(),
            with_images
        );
    }
}
