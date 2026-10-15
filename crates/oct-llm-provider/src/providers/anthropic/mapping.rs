//! Mapping between neutral model types and provider wire messages.
use super::*;

pub(super) fn map_anthropic_message(
    message: &Message,
) -> Result<AnthropicRequestMessage, ModelError> {
    let role = match message.role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "user",
        Role::System => unreachable!(),
    }
    .to_string();

    let mut content = Vec::with_capacity(message.parts.len());
    for part in &message.parts {
        content.push(match part {
            ContentPart::Text(text) => AnthropicContentBlock::Text(AnthropicTextBlock {
                text: text.clone(),
                citations: None,
            }),
            ContentPart::ImageUrl { url } => AnthropicContentBlock::Image(AnthropicImageBlock {
                source: anthropic_image_source(url),
            }),
            // Legacy/other-provider reasoning has no Anthropic signature.
            // Do not fabricate an unauthenticated thinking block on replay.
            ContentPart::Reasoning(_) => continue,
            ContentPart::ReasoningBlock(crate::core::ReasoningBlock::Signed {
                text,
                signature,
                prefix_fingerprint,
            }) => AnthropicContentBlock::Thinking(AnthropicThinkingBlock {
                thinking: text.clone(),
                signature: Some(signature.clone()),
                prefix_fingerprint: prefix_fingerprint.clone(),
            }),
            ContentPart::ReasoningBlock(crate::core::ReasoningBlock::Redacted {
                data,
                prefix_fingerprint,
            }) => AnthropicContentBlock::RedactedThinking(AnthropicRedactedThinkingBlock {
                data: data.clone(),
                prefix_fingerprint: prefix_fingerprint.clone(),
            }),
            ContentPart::ToolCall(call) => AnthropicContentBlock::ToolUse(AnthropicToolUseBlock {
                id: call.id.clone(),
                name: call.name.clone(),
                input: serde_json::from_str(&call.arguments)
                    .unwrap_or(Value::Object(Default::default())),
            }),
            ContentPart::ToolResult(result) => {
                AnthropicContentBlock::ToolResult(AnthropicToolResultBlock {
                    tool_use_id: result.call_id.clone(),
                    content: anthropic_tool_result_content(result),
                    is_error: if result.is_error { Some(true) } else { None },
                })
            }
        });
    }

    Ok(AnthropicRequestMessage { role, content })
}

/// Wire `content` of one tool result. Text-only results stay a plain string
/// (the compact form every pre-images snapshot depends on); results carrying
/// images become Anthropic's block array — a text block first (when the text
/// is non-empty), then one base64 image block per attached image, which the
/// API renders natively inside `tool_result`.
pub(super) fn anthropic_tool_result_content(result: &ToolResult) -> Value {
    if result.images.is_empty() {
        return result.content.clone();
    }
    let mut blocks = Vec::with_capacity(result.images.len() + 1);
    let text = match &result.content {
        Value::String(text) => Some(text.clone()),
        Value::Null => None,
        // Non-string content (never produced by the agent loop today) still
        // travels as its compact JSON rendering so nothing is lost.
        other => Some(other.to_string()),
    };
    if let Some(text) = text.filter(|t| !t.is_empty()) {
        // Wrap in the content-block enum so serialization carries the
        // `type: "text"` tag the array form requires.
        blocks.push(
            serde_json::to_value(AnthropicContentBlock::Text(AnthropicTextBlock {
                text,
                citations: None,
            }))
            .unwrap_or(Value::Null),
        );
    }
    for image in &result.images {
        blocks.push(
            serde_json::to_value(AnthropicContentBlock::Image(AnthropicImageBlock {
                source: AnthropicImageSource::Base64 {
                    media_type: image.mime_type.clone(),
                    data: image.data.clone(),
                },
            }))
            .unwrap_or(Value::Null),
        );
    }
    Value::Array(blocks)
}

pub(super) fn map_anthropic_tool(tool: &crate::core::ToolSpec) -> AnthropicToolDefinition {
    AnthropicToolDefinition {
        name: tool.name.clone(),
        description: tool.description.clone(),
        input_schema: tool.input_schema.clone(),
    }
}

pub(super) fn map_anthropic_tool_choice(choice: &crate::core::ToolChoice) -> AnthropicToolChoice {
    match choice {
        crate::core::ToolChoice::Auto => AnthropicToolChoice::Auto {
            disable_parallel_tool_use: None,
        },
        crate::core::ToolChoice::None => AnthropicToolChoice::None,
        crate::core::ToolChoice::Required => AnthropicToolChoice::Any {
            disable_parallel_tool_use: None,
        },
        crate::core::ToolChoice::Named(name) => AnthropicToolChoice::Tool {
            name: name.clone(),
            disable_parallel_tool_use: None,
        },
    }
}

pub fn map_anthropic_response(payload: AnthropicResponse) -> Result<ChatResponse, ModelError> {
    let usage = Some(map_anthropic_usage(&payload.usage));
    let mut provider_metadata = Map::new();
    provider_metadata.insert("model".to_string(), Value::String(payload.model.clone()));
    if let Some(container) = &payload.container {
        provider_metadata.insert(
            "container".to_string(),
            serde_json::to_value(container).unwrap_or(Value::Null),
        );
    }

    Ok(ChatResponse {
        message: Message {
            role: Role::Assistant,
            parts: payload
                .content
                .into_iter()
                .map(parse_anthropic_content_block)
                .collect::<Result<Vec<_>, _>>()?,
        },
        finish_reason: anthropic_finish_reason(payload.stop_reason.as_deref()),
        usage,
        provider_response_id: Some(payload.id),
        provider_metadata,
    })
}

pub fn parse_anthropic_generate_body(body: &str) -> Result<ChatResponse, ModelError> {
    let payload: AnthropicResponse = serde_json::from_str(body)
        .map_err(|err| ModelError::transport(format!("invalid anthropic response json: {err}")))?;
    map_anthropic_response(payload)
}

pub(super) fn parse_anthropic_content_block(
    block: AnthropicContentBlock,
) -> Result<ContentPart, ModelError> {
    match block {
        AnthropicContentBlock::Text(text_block) => Ok(ContentPart::Text(text_block.text)),
        AnthropicContentBlock::ToolUse(tool_block) => {
            Ok(ContentPart::ToolCall(crate::core::ToolCall {
                id: tool_block.id,
                name: tool_block.name,
                arguments: serde_json::to_string(&tool_block.input).unwrap_or_default(),
            }))
        }
        AnthropicContentBlock::ToolResult(result_block) => {
            Ok(ContentPart::ToolResult(crate::core::ToolResult {
                call_id: result_block.tool_use_id,
                content: result_block.content,
                is_error: result_block.is_error.unwrap_or(false),
                // Assistant responses never carry tool_result blocks with
                // images (that is a request-side shape), so nothing to parse.
                images: Vec::new(),
            }))
        }
        AnthropicContentBlock::Thinking(thinking_block) => Ok(ContentPart::ReasoningBlock(
            crate::core::ReasoningBlock::Signed {
                text: thinking_block.thinking,
                signature: thinking_block
                    .signature
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        ModelError::provider("anthropic thinking block is missing its signature")
                    })?,
                prefix_fingerprint: thinking_block.prefix_fingerprint,
            },
        )),
        AnthropicContentBlock::RedactedThinking(redacted_block) => Ok(ContentPart::ReasoningBlock(
            crate::core::ReasoningBlock::Redacted {
                data: redacted_block.data,
                prefix_fingerprint: redacted_block.prefix_fingerprint,
            },
        )),
        AnthropicContentBlock::Image(_) => {
            Err(ModelError::provider("unexpected image block in response"))
        }
    }
}

pub fn map_anthropic_usage(raw: &AnthropicUsage) -> Usage {
    let mut provider_details = Map::new();
    provider_details.insert(
        "raw".to_string(),
        serde_json::to_value(raw).unwrap_or(Value::Null),
    );
    if let Some(cache_creation) = &raw.cache_creation {
        provider_details.insert(
            "cache_creation".to_string(),
            serde_json::to_value(cache_creation).unwrap_or(Value::Null),
        );
    }
    if let Some(server_tool_use) = &raw.server_tool_use {
        provider_details.insert(
            "server_tool_use".to_string(),
            serde_json::to_value(server_tool_use).unwrap_or(Value::Null),
        );
    }
    if let Some(ref geo) = raw.inference_geo {
        provider_details.insert("inference_geo".to_string(), Value::String(geo.clone()));
    }
    if let Some(ref tier) = raw.service_tier {
        provider_details.insert("service_tier".to_string(), Value::String(tier.clone()));
    }

    let input_tokens = Some(raw.input_tokens as u32);
    let output_tokens = Some(raw.output_tokens as u32);
    let total_tokens = Some((raw.input_tokens + raw.output_tokens) as u32);

    Usage {
        input_tokens,
        output_tokens,
        total_tokens,
        reasoning_tokens: None,
        cached_input_tokens: raw.cache_read_input_tokens.map(|v| v as u32),
        provider_details,
    }
}

pub fn anthropic_finish_reason(raw: Option<&str>) -> FinishReason {
    match raw {
        Some("end_turn") | Some("stop_sequence") => FinishReason::Stop,
        Some("max_tokens") => FinishReason::Length,
        Some("tool_use") => FinishReason::ToolCalls,
        Some("pause_turn") => FinishReason::Stop,
        Some("refusal") => FinishReason::ContentFilter,
        Some("error") => FinishReason::Error,
        _ => FinishReason::Unknown,
    }
}
