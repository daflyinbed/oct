//! Local provenance for authenticated thinking. Persistence keeps the
//! original blocks; only the model request omits blocks whose prefix changed.
use sha2::{Digest, Sha256};
use std::fmt::Write as _;

use super::*;

pub(super) fn prepare(wire: &mut AnthropicWireRequest) -> Result<(), ModelError> {
    let mut prefix = Sha256::new();
    append(
        &mut prefix,
        &serde_json::json!({
            "system": wire.other.get("system").or(wire.system.as_ref()),
            "tools": wire.other.get("tools").cloned().unwrap_or(serde_json::to_value(&wire.tools)
                .map_err(|err| ModelError::transport(format!("invalid anthropic tools: {err}")))?),
        }),
    )?;

    // An explicitly supplied wire messages array owns its own replay. Still
    // capture the effective request prefix rather than the neutral messages.
    if let Some(Value::Array(messages)) = wire.other.get("messages") {
        for message in messages {
            append(&mut prefix, message)?;
        }
        wire.reasoning_prefix = fingerprint(&prefix);
        return Ok(());
    }

    let bound = uses_prefix_binding(&wire.model);
    for message in &mut wire.messages {
        if bound {
            let expected = fingerprint(&prefix);
            message.content.retain(|block| match block {
                AnthropicContentBlock::Thinking(block) => {
                    block.prefix_fingerprint.as_deref() == Some(&expected)
                }
                AnthropicContentBlock::RedactedThinking(block) => {
                    block.prefix_fingerprint.as_deref() == Some(&expected)
                }
                _ => true,
            });
        }
        if !message.content.is_empty() {
            append(&mut prefix, message)?;
        }
    }
    wire.messages.retain(|message| !message.content.is_empty());
    wire.reasoning_prefix = fingerprint(&prefix);
    Ok(())
}

fn uses_prefix_binding(model: &str) -> bool {
    // These models enforce prefix binding. Earlier models' manual thinking
    // remains replayable after a prefix edit and is required in tool loops.
    // https://platform.claude.com/docs/en/build-with-claude/preserved-thinking
    let model = model.replace('.', "-");
    [
        "claude-fable-5-1",
        "claude-opus-5-5",
        "claude-sonnet-5-5",
        "claude-haiku-5-5",
    ]
    .iter()
    .any(|name| model.contains(name))
}

fn append(prefix: &mut Sha256, value: &impl Serialize) -> Result<(), ModelError> {
    let bytes = serde_json::to_vec(value)
        .map_err(|err| ModelError::transport(format!("invalid anthropic prefix: {err}")))?;
    prefix.update((bytes.len() as u64).to_be_bytes());
    prefix.update(bytes);
    Ok(())
}

fn fingerprint(prefix: &Sha256) -> String {
    let mut fingerprint = String::with_capacity(64);
    for byte in prefix.clone().finalize() {
        write!(fingerprint, "{byte:02x}").expect("writing to a string cannot fail");
    }
    fingerprint
}
