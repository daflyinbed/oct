//! Mapping between neutral model types and provider wire messages.
use super::*;

pub fn map_chat_completion_response(
    payload: ChatCompletionResponse,
) -> Result<ChatResponse, ModelError> {
    let usage = payload.usage.as_ref().map(map_chat_completion_usage);
    let mut provider_metadata = Map::new();
    if let Some(ref raw_usage) = payload.usage {
        provider_metadata.insert(
            "raw_usage".to_string(),
            serde_json::to_value(raw_usage).unwrap_or(Value::Null),
        );
    }

    let choice = payload
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| ModelError::provider("openai response contained no choices"))?;

    Ok(ChatResponse {
        message: map_response_message(choice.message)?,
        finish_reason: map_openai_finish_reason(choice.finish_reason.as_deref()),
        usage,
        provider_response_id: payload.id,
        provider_metadata,
    })
}

pub fn parse_openai_generate_body(body: &str) -> Result<ChatResponse, ModelError> {
    let payload: ChatCompletionResponse = serde_json::from_str(body)
        .map_err(|err| ModelError::transport(format!("invalid openai response json: {err}")))?;
    map_chat_completion_response(payload)
}

/// Map the whole conversation to OpenAI chat-completion messages, threading
/// tool-result images through the protocol's only legal shape: a `tool`
/// role message cannot carry image parts, so images collected from a run of
/// consecutive tool messages are appended as ONE follow-up user message
/// (data URLs) after the run — the tool messages themselves stay
/// consecutive and text-only, exactly as before images existed.
///
/// Requests without tool-result images map message-for-message, identical
/// to calling [`map_message`] directly on each.
pub(super) fn map_request_messages(
    messages: &[Message],
) -> Result<Vec<ChatCompletionRequestMessage>, ModelError> {
    let mut out = Vec::with_capacity(messages.len());
    let mut images: Vec<RequestMessageContentPart> = Vec::new();
    for message in messages {
        if message.role == crate::core::Role::Tool {
            for part in &message.parts {
                if let ContentPart::ToolResult(result) = part {
                    images.extend(result.images.iter().map(image_data_url_part));
                }
            }
        } else if !images.is_empty() {
            // The run of tool messages ended: flush their images as one user
            // message BEFORE this message, keeping tool messages consecutive.
            out.push(image_carrying_user_message(std::mem::take(&mut images)));
        }
        out.push(map_message(message)?);
    }
    if !images.is_empty() {
        // A request may legitimately end on its tool results (the agent loop
        // feeds results back as the final messages).
        out.push(image_carrying_user_message(images));
    }
    Ok(out)
}

/// One tool-result image as an OpenAI vision content part (data URL).
pub(super) fn image_data_url_part(
    image: &crate::core::ToolResultImage,
) -> RequestMessageContentPart {
    RequestMessageContentPart::ImageUrl {
        image_url: ChatCompletionImageUrl {
            url: format!("data:{};base64,{}", image.mime_type, image.data),
            detail: None,
        },
    }
}

/// The follow-up user message carrying one run's tool-result images. A short
/// text label leads the parts so the model can attribute them to the tool
/// calls above.
pub(super) fn image_carrying_user_message(
    images: Vec<RequestMessageContentPart>,
) -> ChatCompletionRequestMessage {
    let mut parts = Vec::with_capacity(images.len() + 1);
    parts.push(RequestMessageContentPart::Text {
        text: "Images returned by the tool calls above:".to_string(),
    });
    parts.extend(images);
    ChatCompletionRequestMessage {
        role: "user".to_string(),
        content: Some(RequestMessageContentValue::Parts(parts)),
        name: None,
        tool_call_id: None,
        tool_calls: None,
        reasoning_content: None,
    }
}

pub(super) fn map_message(message: &Message) -> Result<ChatCompletionRequestMessage, ModelError> {
    let role = match message.role {
        crate::core::Role::System => "system",
        crate::core::Role::User => "user",
        crate::core::Role::Assistant => "assistant",
        crate::core::Role::Tool => "tool",
    }
    .to_string();

    if message.role == crate::core::Role::Tool {
        let tool_result = message
            .parts
            .iter()
            .find_map(|p| match p {
                ContentPart::ToolResult(r) => Some(r),
                _ => None,
            })
            .ok_or_else(|| ModelError::provider("tool role message missing ToolResult part"))?;

        let content_str = match &tool_result.content {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };

        return Ok(ChatCompletionRequestMessage {
            role,
            content: Some(RequestMessageContentValue::String(content_str)),
            name: None,
            tool_call_id: Some(tool_result.call_id.clone()),
            tool_calls: None,
            reasoning_content: None,
        });
    }

    let mut text_parts = Vec::new();
    let mut tool_calls = Vec::new();
    let mut reasoning_parts = Vec::new();

    for part in &message.parts {
        match part {
            ContentPart::Text(text) => {
                text_parts.push(RequestMessageContentPart::Text { text: text.clone() });
            }
            ContentPart::ImageUrl { url } => {
                text_parts.push(RequestMessageContentPart::ImageUrl {
                    image_url: ChatCompletionImageUrl {
                        url: url.clone(),
                        detail: None,
                    },
                });
            }
            ContentPart::Reasoning(text) => {
                reasoning_parts.push(text.clone());
            }
            ContentPart::ReasoningBlock(block) => {
                // A model switch can reuse the visible text, but opaque
                // signatures and redacted data belong to their own protocol.
                if let Some(text) = block.text() {
                    reasoning_parts.push(text.to_string());
                }
            }
            ContentPart::ToolCall(call) => {
                tool_calls.push(RequestMessageToolCall {
                    id: call.id.clone(),
                    kind: "function".to_string(),
                    function: RequestMessageToolCallFunction {
                        name: call.name.clone(),
                        arguments: call.arguments.clone(),
                    },
                });
            }
            ContentPart::ToolResult(result) => {
                text_parts.push(RequestMessageContentPart::Text {
                    text: json!({
                        "type": "tool_result",
                        "tool_call_id": result.call_id,
                        "content": result.content,
                        "is_error": result.is_error,
                    })
                    .to_string(),
                });
            }
        }
    }

    let content = if text_parts.is_empty() {
        None
    } else if text_parts.len() == 1 {
        match text_parts.into_iter().next().unwrap() {
            RequestMessageContentPart::Text { text } => {
                Some(RequestMessageContentValue::String(text))
            }
            part => Some(RequestMessageContentValue::Parts(vec![part])),
        }
    } else {
        Some(RequestMessageContentValue::Parts(text_parts))
    };

    let tool_calls = if tool_calls.is_empty() {
        None
    } else {
        Some(tool_calls)
    };

    let reasoning_content = if reasoning_parts.is_empty() {
        None
    } else {
        Some(reasoning_parts.join(""))
    };

    Ok(ChatCompletionRequestMessage {
        role,
        content,
        name: None,
        tool_call_id: None,
        tool_calls,
        reasoning_content,
    })
}

pub(super) fn map_response_message(
    message: ChatCompletionResponseMessage,
) -> Result<Message, ModelError> {
    let role = match message.role.as_str() {
        "system" => crate::core::Role::System,
        "user" => crate::core::Role::User,
        "assistant" => crate::core::Role::Assistant,
        "tool" => crate::core::Role::Tool,
        other => {
            return Err(ModelError::provider(format!(
                "unsupported openai response role: {other}"
            )));
        }
    };

    let mut parts = Vec::new();

    let reasoning = message
        .reasoning_content
        .as_ref()
        .or(message.reasoning.as_ref())
        .filter(|s| !s.is_empty());

    if let Some(reasoning) = reasoning {
        parts.push(ContentPart::Reasoning(reasoning.clone()));
    }

    if let Some(content) = message.content
        && !content.is_empty()
    {
        parts.push(ContentPart::Text(content));
    }

    if let Some(tool_calls) = message.tool_calls {
        for call in tool_calls {
            parts.push(ContentPart::ToolCall(crate::core::ToolCall {
                id: call.id,
                name: call.function.name,
                arguments: call.function.arguments,
            }));
        }
    }

    if let Some(function_call) = message.function_call {
        parts.push(ContentPart::ToolCall(crate::core::ToolCall {
            id: String::new(),
            name: function_call.name,
            arguments: function_call.arguments,
        }));
    }

    Ok(Message { role, parts })
}

pub(super) fn map_tool(tool: &ToolSpec) -> ChatCompletionTool {
    ChatCompletionTool {
        kind: "function".to_string(),
        function: ChatCompletionToolFunction {
            name: tool.name.clone(),
            description: tool.description.clone(),
            parameters: tool.input_schema.clone(),
            strict: None,
        },
    }
}

pub(super) fn map_tool_choice(choice: &ToolChoice) -> ChatCompletionToolChoiceOption {
    match choice {
        ToolChoice::Auto => ChatCompletionToolChoiceOption::String("auto".to_string()),
        ToolChoice::None => ChatCompletionToolChoiceOption::String("none".to_string()),
        ToolChoice::Required => ChatCompletionToolChoiceOption::String("required".to_string()),
        ToolChoice::Named(name) => {
            ChatCompletionToolChoiceOption::Named(ChatCompletionNamedToolChoice {
                kind: "function".to_string(),
                function: ChatCompletionNamedToolChoiceFunction { name: name.clone() },
            })
        }
    }
}

pub fn map_chat_completion_usage(usage: &ChatCompletionUsage) -> Usage {
    let mut provider_details = Map::new();
    if let Ok(raw) = serde_json::to_value(usage) {
        provider_details.insert("raw".to_string(), raw);
    }

    Usage {
        input_tokens: Some(usage.prompt_tokens),
        output_tokens: Some(usage.completion_tokens),
        total_tokens: Some(usage.total_tokens),
        reasoning_tokens: usage
            .completion_tokens_details
            .as_ref()
            .and_then(|d| d.reasoning_tokens),
        cached_input_tokens: usage
            .prompt_tokens_details
            .as_ref()
            .and_then(|d| d.cached_tokens),
        provider_details,
    }
}

pub fn map_openai_finish_reason(raw: Option<&str>) -> FinishReason {
    match raw {
        Some("stop") => FinishReason::Stop,
        Some("length") => FinishReason::Length,
        Some("tool_calls") | Some("function_call") => FinishReason::ToolCalls,
        Some("content_filter") => FinishReason::ContentFilter,
        Some("error") => FinishReason::Error,
        _ => FinishReason::Unknown,
    }
}
