//! Decode immutable message content consistently for history rules and the DB view.
use anyhow::Result;
use oct_llm_provider::core::{ContentPart, Message, Role};
use serde_json::Value;

pub fn parse_message(role: &str, parts_json: &str, details_json: Option<&str>) -> Result<Message> {
    let role = match role {
        "system" => Role::System,
        "user" => Role::User,
        "assistant" => Role::Assistant,
        "tool" => Role::Tool,
        other => anyhow::bail!("Unknown role: {other}"),
    };
    if role == Role::User
        && let Some(content) =
            crate::attachment_types::UserContent::from_stored(parts_json, details_json)
    {
        // Only v2 rows have a canonical frozen snapshot. Legacy rows
        // retain their original request bytes through the v1 reader.
        let details: Value = serde_json::from_str(details_json.unwrap_or("{}"))?;
        if details.get("user_content").is_some() {
            return Ok(content.to_message());
        }
    }
    let parts: Vec<ContentPart> = serde_json::from_str(parts_json)?;
    Ok(Message { role, parts })
}
