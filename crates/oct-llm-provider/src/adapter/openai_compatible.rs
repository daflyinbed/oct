use async_trait::async_trait;
use async_stream::try_stream;
use futures_util::StreamExt;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::HashMap;

use crate::core::{
    ContentPart, FinishReason, Message, ModelError, StreamEvent, ToolChoice, ToolSpec, Usage,
};
use crate::model::{ChatModel, ChatRequest, ChatResponse, ChatStream};
use crate::provider::ModelInfo;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OpenAiCompatibleConfig {
    pub provider_name: String,
    pub base_url: String,
    pub api_key_env: String,
    pub default_headers: Map<String, Value>,
    pub model_info: ModelInfo,
    pub use_responses_api: bool,
}

#[derive(Debug, Clone)]
pub struct OpenAiCompatibleChatModel {
    pub config: OpenAiCompatibleConfig,
    client: reqwest::Client,
    api_key: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct OpenAiToolCallAccumulator {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatCompletionUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    #[serde(default)]
    pub completion_tokens_details: Option<ChatCompletionUsageCompletionDetails>,
    #[serde(default)]
    pub prompt_tokens_details: Option<ChatCompletionUsagePromptDetails>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ChatCompletionUsageCompletionDetails {
    #[serde(default)]
    pub reasoning_tokens: Option<u32>,
    #[serde(default)]
    pub audio_tokens: Option<u32>,
    #[serde(default)]
    pub accepted_prediction_tokens: Option<u32>,
    #[serde(default)]
    pub rejected_prediction_tokens: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ChatCompletionUsagePromptDetails {
    #[serde(default)]
    pub cached_tokens: Option<u32>,
    #[serde(default)]
    pub audio_tokens: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatCompletionFunctionCall {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatCompletionMessageToolCallFunction {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatCompletionMessageToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: ChatCompletionMessageToolCallFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatCompletionContentPart {
    Text { text: String },
    ImageUrl { image_url: ChatCompletionImageUrl },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatCompletionImageUrl {
    pub url: String,
    #[serde(default)]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatCompletionResponseMessage {
    pub role: String,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub reasoning_content: Option<String>,
    #[serde(default)]
    pub reasoning: Option<String>,
    #[serde(default)]
    pub refusal: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<ChatCompletionMessageToolCall>>,
    #[serde(default)]
    pub function_call: Option<ChatCompletionFunctionCall>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatCompletionChoice {
    pub index: u32,
    pub message: ChatCompletionResponseMessage,
    pub finish_reason: Option<String>,
    #[serde(default)]
    pub logprobs: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatCompletionResponse {
    pub id: Option<String>,
    pub object: Option<String>,
    pub created: Option<u64>,
    pub model: Option<String>,
    #[serde(default)]
    pub choices: Vec<ChatCompletionChoice>,
    #[serde(default)]
    pub usage: Option<ChatCompletionUsage>,
    #[serde(default)]
    pub system_fingerprint: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatCompletionStreamOptions {
    #[serde(default)]
    pub include_usage: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatCompletionChunkDeltaToolCallFunction {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatCompletionChunkDeltaToolCall {
    pub index: u32,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub function: Option<ChatCompletionChunkDeltaToolCallFunction>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ChatCompletionChunkDelta {
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub refusal: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<ChatCompletionChunkDeltaToolCall>>,
    #[serde(default)]
    pub function_call: Option<ChatCompletionFunctionCall>,
    #[serde(default)]
    pub reasoning_content: Option<String>,
    #[serde(default)]
    pub reasoning: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatCompletionChunkChoice {
    pub index: u32,
    pub delta: ChatCompletionChunkDelta,
    pub finish_reason: Option<String>,
    #[serde(default)]
    pub logprobs: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatCompletionChunk {
    pub id: Option<String>,
    pub object: Option<String>,
    pub created: Option<u64>,
    pub model: Option<String>,
    #[serde(default)]
    pub choices: Vec<ChatCompletionChunkChoice>,
    #[serde(default)]
    pub usage: Option<ChatCompletionUsage>,
    #[serde(default)]
    pub system_fingerprint: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatCompletionToolFunction {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub parameters: Value,
    #[serde(default)]
    pub strict: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatCompletionTool {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: ChatCompletionToolFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatCompletionResponseFormat {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub json_schema: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatCompletionNamedToolChoice {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: ChatCompletionNamedToolChoiceFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatCompletionNamedToolChoiceFunction {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum ChatCompletionToolChoiceOption {
    String(String),
    Named(ChatCompletionNamedToolChoice),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RequestMessageContent {
    Text { text: String },
    ImageUrl { image_url: ChatCompletionImageUrl },
    InputAudio { input_audio: ChatCompletionInputAudio },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatCompletionInputAudio {
    pub data: String,
    pub format: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RequestMessageToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: RequestMessageToolCallFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RequestMessageToolCallFunction {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatCompletionRequestMessage {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<RequestMessageContentValue>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<RequestMessageToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum RequestMessageContentValue {
    String(String),
    Parts(Vec<RequestMessageContentPart>),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RequestMessageContentPart {
    Text { text: String },
    ImageUrl { image_url: ChatCompletionImageUrl },
    InputAudio { input_audio: ChatCompletionInputAudio },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<ChatCompletionRequestMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub stop: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub tools: Vec<ChatCompletionTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ChatCompletionToolChoiceOption>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_format: Option<ChatCompletionResponseFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub n: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f32>,
    #[serde(flatten)]
    pub provider_options: Value,
    #[serde(skip_serializing_if = "std::ops::Not::not", default)]
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<ChatCompletionStreamOptions>,
}

impl OpenAiCompatibleChatModel {
    pub fn new(config: OpenAiCompatibleConfig) -> Self {
        Self {
            config,
            client: reqwest::Client::new(),
            api_key: None,
        }
    }

    pub fn new_with_api_key(config: OpenAiCompatibleConfig, api_key: String, client: reqwest::Client) -> Self {
        Self {
            config,
            client,
            api_key: Some(api_key),
        }
    }

    fn endpoint(&self) -> String {
        let base = self.config.base_url.trim_end_matches('/');
        if self.config.use_responses_api {
            format!("{base}/responses")
        } else {
            format!("{base}/chat/completions")
        }
    }

    fn auth_header_value(&self) -> Result<HeaderValue, ModelError> {
        let api_key = match &self.api_key {
            Some(key) => key.clone(),
            None => std::env::var(&self.config.api_key_env).map_err(|_| ModelError::Authentication)?,
        };
        HeaderValue::from_str(&format!("Bearer {api_key}"))
            .map_err(|err| ModelError::transport(format!("invalid auth header: {err}")))
    }

    fn build_headers(&self) -> Result<HeaderMap, ModelError> {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(AUTHORIZATION, self.auth_header_value()?);

        for (key, value) in &self.config.default_headers {
            let value = value
                .as_str()
                .ok_or_else(|| ModelError::provider(format!("default header {key} must be a string")))?;
            let name = HeaderName::from_bytes(key.as_bytes())
                .map_err(|err| ModelError::transport(format!("invalid header name {key}: {err}")))?;
            let value = HeaderValue::from_str(value)
                .map_err(|err| ModelError::transport(format!("invalid header value for {key}: {err}")))?;
            headers.insert(name, value);
        }

        Ok(headers)
    }

    pub fn to_wire_request(&self, req: &ChatRequest) -> Result<ChatCompletionRequest, ModelError> {
        let messages = req
            .messages
            .iter()
            .map(map_message)
            .collect::<Result<Vec<_>, _>>()?;

        let tools = req.tools.iter().map(map_tool).collect();
        let tool_choice = req.options.tool_choice.as_ref().map(map_tool_choice);
        let response_format = req.options.json_schema.clone().map(|schema| ChatCompletionResponseFormat {
            kind: "json_schema".to_string(),
            json_schema: Some(schema),
        });

        Ok(ChatCompletionRequest {
            model: self.config.model_info.model_id.clone(),
            messages,
            temperature: req.options.temperature,
            top_p: req.options.top_p,
            max_tokens: req.options.max_output_tokens,
            max_completion_tokens: None,
            stop: req.options.stop_sequences.clone(),
            tools,
            tool_choice,
            response_format,
            n: req.options.n,
            presence_penalty: req.options.presence_penalty,
            frequency_penalty: req.options.frequency_penalty,
            provider_options: req.options.provider_options.clone(),
            stream: false,
            stream_options: None,
        })
    }

    fn to_streaming_wire_request(&self, req: &ChatRequest) -> Result<ChatCompletionRequest, ModelError> {
        let mut wire = self.to_wire_request(req)?;
        wire.stream = true;
        wire.stream_options = Some(ChatCompletionStreamOptions { include_usage: true });
        Ok(wire)
    }
}

#[async_trait]
impl ChatModel for OpenAiCompatibleChatModel {
    fn info(&self) -> &ModelInfo {
        &self.config.model_info
    }

    async fn generate(&self, req: ChatRequest) -> Result<ChatResponse, ModelError> {
        let wire = self.to_wire_request(&req)?;
        let response = self
            .client
            .post(self.endpoint())
            .headers(self.build_headers()?)
            .json(&wire)
            .send()
            .await
            .map_err(|err| ModelError::transport(format!("request failed: {err}")))?;

        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(ModelError::Authentication);
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(ModelError::RateLimited);
        }
        if !status.is_success() {
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "<failed to read error body>".to_string());
            return Err(ModelError::provider(format!(
                "{} returned {}: {}",
                self.config.provider_name, status, body
            )));
        }

        let payload: ChatCompletionResponse = response
            .json()
            .await
            .map_err(|err| ModelError::transport(format!("invalid response json: {err}")))?;

        map_chat_completion_response(payload)
    }

    async fn stream(&self, req: ChatRequest) -> Result<ChatStream, ModelError> {
        let wire = self.to_streaming_wire_request(&req)?;
        let response = self
            .client
            .post(self.endpoint())
            .headers(self.build_headers()?)
            .json(&wire)
            .send()
            .await
            .map_err(|err| ModelError::transport(format!("request failed: {err}")))?;

        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(ModelError::Authentication);
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(ModelError::RateLimited);
        }
        if !status.is_success() {
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "<failed to read error body>".to_string());
            return Err(ModelError::provider(format!(
                "{} returned {}: {}",
                self.config.provider_name, status, body
            )));
        }

        let byte_stream = response.bytes_stream();
        let stream = try_stream! {
            let mut buffer = String::new();
            let mut tool_state = HashMap::new();
            futures_util::pin_mut!(byte_stream);

            while let Some(chunk) = byte_stream.next().await {
                let chunk = chunk
                    .map_err(|err| ModelError::transport(format!("stream read failed: {err}")))?;
                let text = std::str::from_utf8(&chunk)
                    .map_err(|err| ModelError::transport(format!("stream chunk was not valid utf-8: {err}")))?;
                buffer.push_str(text);

                while let Some(event) = take_sse_event(&mut buffer) {
                    for normalized in parse_openai_sse_event_with_state(&event, &mut tool_state)? {
                        yield normalized;
                    }
                }
            }

            for remainder in flush_sse_buffer(&mut buffer) {
                for normalized in parse_openai_sse_event_with_state(&remainder, &mut tool_state)? {
                    yield normalized;
                }
            }
        };

        Ok(Box::pin(stream))
    }
}

pub fn map_chat_completion_response(payload: ChatCompletionResponse) -> Result<ChatResponse, ModelError> {
    let usage = payload.usage.as_ref().map(map_chat_completion_usage);
    let mut provider_metadata = Map::new();
    if let Some(ref raw_usage) = payload.usage {
        provider_metadata.insert("raw_usage".to_string(), serde_json::to_value(raw_usage).unwrap_or(Value::Null));
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

fn map_message(message: &Message) -> Result<ChatCompletionRequestMessage, ModelError> {
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
                    image_url: ChatCompletionImageUrl { url: url.clone(), detail: None },
                });
            }
            ContentPart::Reasoning(text) => {
                reasoning_parts.push(text.clone());
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
            RequestMessageContentPart::Text { text } => Some(RequestMessageContentValue::String(text)),
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

fn map_response_message(message: ChatCompletionResponseMessage) -> Result<Message, ModelError> {
    let role = match message.role.as_str() {
        "system" => crate::core::Role::System,
        "user" => crate::core::Role::User,
        "assistant" => crate::core::Role::Assistant,
        "tool" => crate::core::Role::Tool,
        other => {
            return Err(ModelError::provider(format!(
                "unsupported openai response role: {other}"
            )))
        }
    };

    let mut parts = Vec::new();

    let reasoning = message.reasoning_content.as_ref()
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

fn map_tool(tool: &ToolSpec) -> ChatCompletionTool {
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

fn map_tool_choice(choice: &ToolChoice) -> ChatCompletionToolChoiceOption {
    match choice {
        ToolChoice::Auto => ChatCompletionToolChoiceOption::String("auto".to_string()),
        ToolChoice::None => ChatCompletionToolChoiceOption::String("none".to_string()),
        ToolChoice::Required => ChatCompletionToolChoiceOption::String("required".to_string()),
        ToolChoice::Named(name) => ChatCompletionToolChoiceOption::Named(ChatCompletionNamedToolChoice {
            kind: "function".to_string(),
            function: ChatCompletionNamedToolChoiceFunction { name: name.clone() },
        }),
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
        reasoning_tokens: usage.completion_tokens_details.as_ref().and_then(|d| d.reasoning_tokens),
        cached_input_tokens: usage.prompt_tokens_details.as_ref().and_then(|d| d.cached_tokens),
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

pub fn aggregate_stream_events(events: &[StreamEvent]) -> Option<Usage> {
    events.iter().rev().find_map(|event| match event {
        StreamEvent::Usage(usage) => Some(usage.clone()),
        _ => None,
    })
}

fn take_sse_event(buffer: &mut String) -> Option<String> {
    if let Some(index) = buffer.find("\n\n") {
        let event = buffer[..index].to_string();
        buffer.drain(..index + 2);
        Some(event)
    } else if let Some(index) = buffer.find("\r\n\r\n") {
        let event = buffer[..index].to_string();
        buffer.drain(..index + 4);
        Some(event)
    } else {
        None
    }
}

fn flush_sse_buffer(buffer: &mut String) -> Vec<String> {
    if buffer.trim().is_empty() {
        Vec::new()
    } else {
        vec![std::mem::take(buffer)]
    }
}

pub fn parse_openai_sse_event(event: &str) -> Result<Vec<StreamEvent>, ModelError> {
    parse_openai_sse_event_with_state(event, &mut HashMap::new())
}

pub fn parse_openai_sse_transcript(transcript: &str) -> Result<Vec<StreamEvent>, ModelError> {
    let mut buffer = transcript.to_string();
    let mut tool_state = HashMap::new();
    let mut events = Vec::new();

    while let Some(event) = take_sse_event(&mut buffer) {
        events.extend(parse_openai_sse_event_with_state(&event, &mut tool_state)?);
    }

    for remainder in flush_sse_buffer(&mut buffer) {
        events.extend(parse_openai_sse_event_with_state(&remainder, &mut tool_state)?);
    }

    Ok(events)
}

fn parse_openai_sse_event_with_state(
    event: &str,
    tool_state: &mut HashMap<String, OpenAiToolCallAccumulator>,
) -> Result<Vec<StreamEvent>, ModelError> {
    let mut data_lines = Vec::new();
    for line in event.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.trim());
        }
    }

    if data_lines.is_empty() {
        return Ok(Vec::new());
    }

    let data = data_lines.join("\n");
    if data == "[DONE]" {
        return Ok(Vec::new());
    }

    let chunk: ChatCompletionChunk = serde_json::from_str(&data)
        .map_err(|err| ModelError::transport(format!("invalid openai stream chunk: {err}")))?;

    normalize_chat_completion_chunk_with_state(chunk, tool_state)
}

pub fn normalize_chat_completion_chunk(chunk: ChatCompletionChunk) -> Result<Vec<StreamEvent>, ModelError> {
    normalize_chat_completion_chunk_with_state(chunk, &mut HashMap::new())
}

fn normalize_chat_completion_chunk_with_state(
    chunk: ChatCompletionChunk,
    tool_state: &mut HashMap<String, OpenAiToolCallAccumulator>,
) -> Result<Vec<StreamEvent>, ModelError> {
    let mut events = Vec::new();

    if let Some(ref usage) = chunk.usage {
        events.push(StreamEvent::Usage(map_chat_completion_usage(usage)));
    }

    for choice in chunk.choices {
        let reasoning = choice.delta.reasoning_content.as_ref()
            .or(choice.delta.reasoning.as_ref())
            .filter(|s| !s.is_empty());
        
        if let Some(reasoning) = reasoning {
            events.push(StreamEvent::ReasoningDelta(reasoning.clone()));
        }

        if let Some(ref content) = choice.delta.content
            && !content.is_empty()
        {
            events.push(StreamEvent::TextDelta(content.clone()));
        }

        if let Some(tool_calls) = choice.delta.tool_calls {
            for tool_call in tool_calls {
                let (delta_event, final_event) = parse_tool_call_delta(tool_call, tool_state)?;
                events.push(delta_event);
                if let Some(final_event) = final_event {
                    events.push(final_event);
                }
            }
        }

        if let Some(reason) = choice.finish_reason {
            if reason == "tool_calls" || reason == "function_call" {
                for (_, accumulator) in std::mem::take(tool_state) {
                    if accumulator.id.is_some() || accumulator.name.is_some() {
                        events.push(StreamEvent::ToolCall(crate::core::ToolCall {
                            id: accumulator.id.unwrap_or_default(),
                            name: accumulator.name.unwrap_or_default(),
                            arguments: accumulator.arguments,
                        }));
                    }
                }
            }
            events.push(StreamEvent::Finish(map_openai_finish_reason(Some(&reason))));
        }
    }

    Ok(events)
}

fn parse_tool_call_delta(
    tool_call: ChatCompletionChunkDeltaToolCall,
    tool_state: &mut HashMap<String, OpenAiToolCallAccumulator>,
) -> Result<(StreamEvent, Option<StreamEvent>), ModelError> {
    let index = tool_call.index.to_string();

    let call_id_str = tool_call.id.clone();
    let (name, arguments_delta) = if let Some(ref function) = tool_call.function {
        (
            function.name.clone(),
            function.arguments.clone().unwrap_or_default(),
        )
    } else {
        (None, String::new())
    };

    let accumulator = tool_state.entry(index.clone()).or_default();
    if let Some(ref id) = call_id_str {
        accumulator.id = Some(id.clone());
    }
    if let Some(n) = name.clone() {
        accumulator.name = Some(n);
    }
    accumulator.arguments.push_str(&arguments_delta);

    let delta_call_id = call_id_str.unwrap_or_else(|| index.clone());

    Ok((
        StreamEvent::ToolCallDelta {
            call_id: delta_call_id,
            name,
            arguments_delta,
        },
        None,
    ))
}

pub type OpenAiWireRequest = ChatCompletionRequest;
pub type OpenAiGenerateResponse = ChatCompletionResponse;
pub type OpenAiChoice = ChatCompletionChoice;
pub type OpenAiRequestMessage = ChatCompletionRequestMessage;
pub type OpenAiResponseFormat = ChatCompletionResponseFormat;
pub type OpenAiToolDefinition = ChatCompletionTool;
pub type OpenAiStreamChunk = ChatCompletionChunk;
pub type OpenAiStreamChunkChoice = ChatCompletionChunkChoice;
pub type OpenAiStreamChunkChoiceDelta = ChatCompletionChunkDelta;

pub fn map_openai_generate_response(payload: OpenAiGenerateResponse) -> Result<ChatResponse, ModelError> {
    map_chat_completion_response(payload)
}

pub fn map_openai_usage(raw: &ChatCompletionUsage) -> Usage {
    map_chat_completion_usage(raw)
}

pub fn normalize_openai_stream_chunk(chunk: OpenAiStreamChunk) -> Result<Vec<StreamEvent>, ModelError> {
    normalize_chat_completion_chunk(chunk)
}