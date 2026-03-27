use async_stream::try_stream;
use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::HashMap;

use crate::core::{ContentPart, FinishReason, Message, ModelError, Role, StreamEvent, Usage};
use crate::model::{ChatModel, ChatRequest, ChatResponse, ChatStream};
use crate::provider::{ModelCapabilities, ModelInfo, ModelLimits, Provider};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
#[allow(private_interfaces)]
pub enum AnthropicContentBlockDelta {
    #[serde(rename = "text_delta")]
    TextDelta { text: String },
    #[serde(rename = "input_json_delta")]
    InputJsonDelta { partial_json: String },
    #[serde(rename = "thinking_delta")]
    ThinkingDelta { thinking: String },
    #[serde(rename = "signature_delta")]
    SignatureDelta { signature: String },
    #[serde(rename = "citations_delta")]
    CitationsDelta { citation: Box<AnthropicCitation> },
}

#[derive(Debug, Clone)]
pub struct AnthropicConfig {
    pub base_url: String,
    pub api_key_env: &'static str,
}

#[derive(Debug, Clone)]
pub struct AnthropicChatModel {
    info: ModelInfo,
    config: AnthropicConfig,
    client: reqwest::Client,
}

#[derive(Debug, Clone)]
pub struct AnthropicProvider {
    pub config: AnthropicConfig,
}

impl AnthropicProvider {
    pub fn new(base_url: impl Into<String>, api_key_env: &'static str) -> Self {
        Self {
            config: AnthropicConfig {
                base_url: base_url.into(),
                api_key_env,
            },
        }
    }

    fn model_info(&self, model: &str) -> ModelInfo {
        ModelInfo::new("anthropic", model)
            .with_capabilities(ModelCapabilities {
                streaming: true,
                native_tools: true,
                vision: true,
                json_mode: false,
                reasoning: true,
                usage: true,
            })
            .with_limits(ModelLimits {
                max_input_tokens: Some(200_000),
                max_output_tokens: Some(8_192),
                max_total_tokens: Some(200_000),
            })
    }
}

impl Default for AnthropicProvider {
    fn default() -> Self {
        Self::new("https://api.anthropic.com/v1", "ANTHROPIC_API_KEY")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(private_interfaces)]
pub struct AnthropicTextBlock {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub citations: Option<Vec<AnthropicCitation>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AnthropicImageBlock {
    source: AnthropicImageSource,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AnthropicImageSource {
    #[serde(rename = "type")]
    source_type: String,
    url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicToolUseBlock {
    pub id: String,
    pub name: String,
    pub input: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AnthropicToolResultBlock {
    tool_use_id: String,
    content: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    is_error: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AnthropicThinkingBlock {
    thinking: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    signature: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AnthropicRedactedThinkingBlock {
    data: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
#[allow(private_interfaces)]
pub enum AnthropicContentBlock {
    #[serde(rename = "text")]
    Text(AnthropicTextBlock),
    #[serde(rename = "image")]
    Image(AnthropicImageBlock),
    #[serde(rename = "tool_use")]
    ToolUse(AnthropicToolUseBlock),
    #[serde(rename = "tool_result")]
    ToolResult(AnthropicToolResultBlock),
    #[serde(rename = "thinking")]
    Thinking(AnthropicThinkingBlock),
    #[serde(rename = "redacted_thinking")]
    RedactedThinking(AnthropicRedactedThinkingBlock),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AnthropicRequestMessage {
    role: String,
    content: Vec<AnthropicContentBlock>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AnthropicToolDefinition {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    input_schema: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
enum AnthropicToolChoice {
    #[serde(rename = "auto")]
    Auto { disable_parallel_tool_use: Option<bool> },
    #[serde(rename = "none")]
    None,
    #[serde(rename = "any")]
    Any { disable_parallel_tool_use: Option<bool> },
    #[serde(rename = "tool")]
    Tool { name: String, disable_parallel_tool_use: Option<bool> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AnthropicMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    user_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AnthropicThinkingConfig {
    #[serde(rename = "type")]
    thinking_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    budget_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    display: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AnthropicWireRequest {
    model: String,
    messages: Vec<AnthropicRequestMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_k: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    stop_sequences: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    tools: Vec<AnthropicToolDefinition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<AnthropicToolChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<AnthropicMetadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<AnthropicThinkingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
    #[serde(flatten)]
    other: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicContainer {
    pub id: String,
    pub expires_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicCacheCreation {
    pub ephemeral_1h_input_tokens: u64,
    pub ephemeral_5m_input_tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicServerToolUsage {
    #[serde(default)]
    pub web_search_requests: u64,
    #[serde(default)]
    pub web_fetch_requests: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation: Option<AnthropicCacheCreation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_tool_use: Option<AnthropicServerToolUsage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inference_geo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicResponse {
    pub id: String,
    #[serde(rename = "type")]
    pub response_type: String,
    pub role: String,
    pub content: Vec<AnthropicContentBlock>,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_sequence: Option<String>,
    pub usage: AnthropicUsage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<AnthropicContainer>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AnthropicMessageDelta {
    stop_reason: Option<String>,
    stop_sequence: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    container: Option<AnthropicContainer>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AnthropicMessageDeltaUsage {
    output_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_creation_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_read_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    server_tool_use: Option<AnthropicServerToolUsage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
#[allow(private_interfaces)]
pub enum AnthropicStreamEvent {
    #[serde(rename = "message_start")]
    MessageStart { message: AnthropicResponse },
    #[serde(rename = "message_delta")]
    MessageDelta { delta: AnthropicMessageDelta, usage: AnthropicMessageDeltaUsage },
    #[serde(rename = "message_stop")]
    MessageStop,
    #[serde(rename = "content_block_start")]
    ContentBlockStart { index: usize, content_block: AnthropicContentBlock },
    #[serde(rename = "content_block_delta")]
    ContentBlockDelta { index: usize, delta: AnthropicContentBlockDelta },
    #[serde(rename = "content_block_stop")]
    ContentBlockStop { index: usize },
    #[serde(rename = "ping")]
    Ping,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AnthropicCitation {
    #[serde(rename = "type")]
    citation_type: String,
    cited_text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    start_char_index: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    end_char_index: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    start_page_number: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    end_page_number: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    start_block_index: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    end_block_index: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    document_index: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    document_title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    encrypted_index: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct AnthropicToolAccumulator {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
    emitted: bool,
}

impl AnthropicChatModel {
    fn new(info: ModelInfo, config: AnthropicConfig) -> Self {
        Self {
            info,
            config,
            client: reqwest::Client::new(),
        }
    }

    fn endpoint(&self) -> String {
        format!("{}/messages", self.config.base_url.trim_end_matches('/'))
    }

    fn build_headers(&self) -> Result<HeaderMap, ModelError> {
        let api_key = std::env::var(self.config.api_key_env).map_err(|_| ModelError::Authentication)?;
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(
            "x-api-key",
            HeaderValue::from_str(&api_key)
                .map_err(|err| ModelError::transport(format!("invalid anthropic api key header: {err}")))?,
        );
        headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        Ok(headers)
    }

    fn to_wire_request(&self, req: &ChatRequest) -> Result<AnthropicWireRequest, ModelError> {
        let mut system_text = String::new();
        let mut messages = Vec::new();

        for message in &req.messages {
            if matches!(message.role, Role::System) {
                for part in &message.parts {
                    match part {
                        ContentPart::Text(text) | ContentPart::Reasoning(text) => {
                            if !system_text.is_empty() {
                                system_text.push_str("\n\n");
                            }
                            system_text.push_str(text);
                        }
                        _ => {
                            return Err(ModelError::unsupported(
                                "anthropic system messages currently support only text/reasoning parts",
                            ))
                        }
                    }
                }
                continue;
            }

            messages.push(map_anthropic_message(message)?);
        }

        let system = if system_text.is_empty() {
            None
        } else {
            Some(Value::String(system_text))
        };

        let tool_choice = req.options.tool_choice.as_ref().map(map_anthropic_tool_choice);

        Ok(AnthropicWireRequest {
            model: self.info.model_id.clone(),
            messages,
            system,
            temperature: req.options.temperature,
            top_p: req.options.top_p,
            top_k: None,
            max_tokens: req.options.max_output_tokens,
            stop_sequences: req.options.stop_sequences.clone(),
            tools: req.tools.iter().map(map_anthropic_tool).collect(),
            tool_choice,
            metadata: None,
            thinking: None,
            stream: None,
            other: req.options.provider_options.as_object().cloned().unwrap_or_default(),
        })
    }
}

#[async_trait]
impl ChatModel for AnthropicChatModel {
    fn info(&self) -> &ModelInfo {
        &self.info
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
                "anthropic returned {}: {}",
                status, body
            )));
        }

        let payload: AnthropicResponse = response
            .json()
            .await
            .map_err(|err| ModelError::transport(format!("invalid response json: {err}")))?;

        map_anthropic_response(payload)
    }

    async fn stream(&self, req: ChatRequest) -> Result<ChatStream, ModelError> {
        let mut wire = self.to_wire_request(&req)?;
        wire.stream = Some(true);

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
                "anthropic returned {}: {}",
                status, body
            )));
        }

        let byte_stream = response.bytes_stream();
        let stream = try_stream! {
            let mut buffer = String::new();
            let mut tool_state: HashMap<usize, AnthropicToolAccumulator> = HashMap::new();
            let mut stop_reason: Option<String> = None;
            futures_util::pin_mut!(byte_stream);

            while let Some(chunk) = byte_stream.next().await {
                let chunk = chunk
                    .map_err(|err| ModelError::transport(format!("stream read failed: {err}")))?;
                let text = std::str::from_utf8(&chunk)
                    .map_err(|err| ModelError::transport(format!("stream chunk was not valid utf-8: {err}")))?;
                buffer.push_str(text);

                while let Some(event) = take_sse_event(&mut buffer) {
                    for normalized in parse_anthropic_sse_event_with_state(&event, &mut tool_state, &mut stop_reason)? {
                        yield normalized;
                    }
                }
            }

            for remainder in flush_sse_buffer(&mut buffer) {
                for normalized in parse_anthropic_sse_event_with_state(&remainder, &mut tool_state, &mut stop_reason)? {
                    yield normalized;
                }
            }
        };

        Ok(Box::pin(stream))
    }
}

impl Provider for AnthropicProvider {
    fn name(&self) -> &'static str {
        "anthropic"
    }

    fn chat_model(&self, model: &str) -> Result<Box<dyn ChatModel>, ModelError> {
        Ok(Box::new(AnthropicChatModel::new(
            self.model_info(model),
            self.config.clone(),
        )))
    }
}

fn map_anthropic_message(message: &Message) -> Result<AnthropicRequestMessage, ModelError> {
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
                source: AnthropicImageSource {
                    source_type: "url".to_string(),
                    url: url.clone(),
                },
            }),
            ContentPart::Reasoning(text) => AnthropicContentBlock::Thinking(AnthropicThinkingBlock {
                thinking: text.clone(),
                signature: None,
            }),
            ContentPart::ToolCall(call) => AnthropicContentBlock::ToolUse(AnthropicToolUseBlock {
                id: call.id.clone(),
                name: call.name.clone(),
                input: call.arguments.clone(),
            }),
            ContentPart::ToolResult(result) => AnthropicContentBlock::ToolResult(AnthropicToolResultBlock {
                tool_use_id: result.call_id.clone(),
                content: result.content.clone(),
                is_error: if result.is_error { Some(true) } else { None },
            }),
        });
    }

    Ok(AnthropicRequestMessage { role, content })
}

fn map_anthropic_tool(tool: &crate::core::ToolSpec) -> AnthropicToolDefinition {
    AnthropicToolDefinition {
        name: tool.name.clone(),
        description: tool.description.clone(),
        input_schema: tool.input_schema.clone(),
    }
}

fn map_anthropic_tool_choice(choice: &crate::core::ToolChoice) -> AnthropicToolChoice {
    match choice {
        crate::core::ToolChoice::Auto => AnthropicToolChoice::Auto { disable_parallel_tool_use: None },
        crate::core::ToolChoice::None => AnthropicToolChoice::None,
        crate::core::ToolChoice::Required => AnthropicToolChoice::Any { disable_parallel_tool_use: None },
        crate::core::ToolChoice::Named(name) => {
            AnthropicToolChoice::Tool { name: name.clone(), disable_parallel_tool_use: None }
        }
    }
}

pub fn map_anthropic_response(payload: AnthropicResponse) -> Result<ChatResponse, ModelError> {
    let usage = Some(map_anthropic_usage(&payload.usage));
    let mut provider_metadata = Map::new();
    provider_metadata.insert("model".to_string(), Value::String(payload.model.clone()));
    if let Some(container) = &payload.container {
        provider_metadata.insert("container".to_string(), serde_json::to_value(container).unwrap_or(Value::Null));
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

fn parse_anthropic_content_block(block: AnthropicContentBlock) -> Result<ContentPart, ModelError> {
    match block {
        AnthropicContentBlock::Text(text_block) => Ok(ContentPart::Text(text_block.text)),
        AnthropicContentBlock::ToolUse(tool_block) => Ok(ContentPart::ToolCall(crate::core::ToolCall {
            id: tool_block.id,
            name: tool_block.name,
            arguments: tool_block.input,
        })),
        AnthropicContentBlock::ToolResult(result_block) => Ok(ContentPart::ToolResult(crate::core::ToolResult {
            call_id: result_block.tool_use_id,
            content: result_block.content,
            is_error: result_block.is_error.unwrap_or(false),
        })),
        AnthropicContentBlock::Thinking(thinking_block) => Ok(ContentPart::Reasoning(thinking_block.thinking)),
        AnthropicContentBlock::RedactedThinking(redacted_block) => {
            Ok(ContentPart::Reasoning(format!("[redacted: {}]", redacted_block.data)))
        }
        AnthropicContentBlock::Image(_) => Err(ModelError::provider("unexpected image block in response")),
    }
}

pub fn map_anthropic_usage(raw: &AnthropicUsage) -> Usage {
    let mut provider_details = Map::new();
    provider_details.insert("raw".to_string(), serde_json::to_value(raw).unwrap_or(Value::Null));
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

pub fn aggregate_anthropic_stream(events: &[StreamEvent]) -> Option<Usage> {
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

pub fn parse_anthropic_sse_event(event: &str) -> Result<Vec<StreamEvent>, ModelError> {
    parse_anthropic_sse_event_with_state(event, &mut HashMap::new(), &mut None)
}

pub fn parse_anthropic_sse_transcript(transcript: &str) -> Result<Vec<StreamEvent>, ModelError> {
    let mut buffer = transcript.to_string();
    let mut tool_state = HashMap::new();
    let mut stop_reason = None;
    let mut events = Vec::new();

    while let Some(event) = take_sse_event(&mut buffer) {
        events.extend(parse_anthropic_sse_event_with_state(
            &event,
            &mut tool_state,
            &mut stop_reason,
        )?);
    }

    for remainder in flush_sse_buffer(&mut buffer) {
        events.extend(parse_anthropic_sse_event_with_state(
            &remainder,
            &mut tool_state,
            &mut stop_reason,
        )?);
    }

    Ok(events)
}

fn parse_anthropic_sse_event_with_state(
    event: &str,
    tool_state: &mut HashMap<usize, AnthropicToolAccumulator>,
    stop_reason: &mut Option<String>,
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

    let payload = data_lines.join("\n");
    let stream_event: AnthropicStreamEvent = serde_json::from_str(&payload)
        .map_err(|err| ModelError::transport(format!("invalid anthropic stream chunk: {err}")))?;
    
    normalize_anthropic_stream_event_with_state(stream_event, tool_state, stop_reason)
}

pub fn normalize_anthropic_stream_event(
    event: AnthropicStreamEvent,
) -> Result<Vec<StreamEvent>, ModelError> {
    normalize_anthropic_stream_event_with_state(event, &mut HashMap::new(), &mut None)
}

fn normalize_anthropic_stream_event_with_state(
    event: AnthropicStreamEvent,
    tool_state: &mut HashMap<usize, AnthropicToolAccumulator>,
    stop_reason: &mut Option<String>,
) -> Result<Vec<StreamEvent>, ModelError> {
    let mut events = Vec::new();

    match event {
        AnthropicStreamEvent::MessageStart { message } => {
            events.push(StreamEvent::Usage(map_anthropic_usage(&message.usage)));
        }
        AnthropicStreamEvent::MessageDelta { delta, usage } => {
            *stop_reason = delta.stop_reason.clone();
            let mut delta_usage = Usage::new(
                usage.input_tokens.map(|v| v as u32),
                Some(usage.output_tokens as u32),
                None,
            );
            delta_usage.cached_input_tokens = usage.cache_read_input_tokens.map(|v| v as u32);
            delta_usage.provider_details.insert(
                "cache_creation_input_tokens".to_string(),
                Value::Number(usage.cache_creation_input_tokens.unwrap_or(0).into()),
            );
            if let Some(server_tool_use) = usage.server_tool_use {
                delta_usage.provider_details.insert(
                    "server_tool_use".to_string(),
                    serde_json::to_value(server_tool_use).unwrap_or(Value::Null),
                );
            }
            events.push(StreamEvent::Usage(delta_usage));
        }
        AnthropicStreamEvent::MessageStop => {
            for (index, acc) in std::mem::take(tool_state) {
                if !acc.emitted && (acc.name.is_some() || !acc.arguments.is_empty()) {
                    events.push(StreamEvent::ToolCall(crate::core::ToolCall {
                        id: acc.id.unwrap_or_else(|| format!("tool_{}", index)),
                        name: acc.name.unwrap_or_default(),
                        arguments: parse_json_string_or_raw(&acc.arguments),
                    }));
                }
            }
            let finish_reason = match stop_reason.as_deref() {
                Some(reason) => anthropic_finish_reason(Some(reason)),
                None => FinishReason::Stop,
            };
            events.push(StreamEvent::Finish(finish_reason));
        }
        AnthropicStreamEvent::ContentBlockStart { index, content_block } => {
            match content_block {
                AnthropicContentBlock::ToolUse(tool_block) => {
                    tool_state.insert(
                        index,
                        AnthropicToolAccumulator {
                            id: Some(tool_block.id.clone()),
                            name: Some(tool_block.name.clone()),
                            arguments: String::new(),
                            emitted: false,
                        },
                    );
                }
                AnthropicContentBlock::Thinking(thinking_block) => {
                    if !thinking_block.thinking.is_empty() {
                        events.push(StreamEvent::ReasoningDelta(thinking_block.thinking));
                    }
                }
                AnthropicContentBlock::Text(text_block) => {
                    if !text_block.text.is_empty() {
                        events.push(StreamEvent::TextDelta(text_block.text));
                    }
                }
                _ => {}
            }
        }
        AnthropicStreamEvent::ContentBlockDelta { index, delta } => {
            match delta {
                AnthropicContentBlockDelta::TextDelta { text } => {
                    events.push(StreamEvent::TextDelta(text));
                }
                AnthropicContentBlockDelta::InputJsonDelta { partial_json } => {
                    let accumulator = tool_state.entry(index).or_default();
                    accumulator.arguments.push_str(&partial_json);
                    let call_id = accumulator.id.clone().unwrap_or_else(|| format!("tool_{}", index));
                    events.push(StreamEvent::ToolCallDelta {
                        call_id,
                        name: None,
                        arguments_delta: partial_json,
                    });
                }
                AnthropicContentBlockDelta::ThinkingDelta { thinking } => {
                    events.push(StreamEvent::ReasoningDelta(thinking));
                }
                AnthropicContentBlockDelta::SignatureDelta { signature: _ } => {
                    // Signature is for multi-turn thinking continuity, we don't expose it
                }
                AnthropicContentBlockDelta::CitationsDelta { citation: _ } => {
                    // Citations are not exposed in our current API
                }
            }
        }
        AnthropicStreamEvent::ContentBlockStop { index } => {
            if let Some(acc) = tool_state.remove(&index)
                && !acc.emitted && (acc.name.is_some() || !acc.arguments.is_empty())
            {
                events.push(StreamEvent::ToolCall(crate::core::ToolCall {
                    id: acc.id.unwrap_or_else(|| format!("tool_{}", index)),
                    name: acc.name.unwrap_or_default(),
                    arguments: parse_json_string_or_raw(&acc.arguments),
                }));
            }
        }
        AnthropicStreamEvent::Ping => {
            // Ping events are keep-alive signals, no action needed
        }
    }

    Ok(events)
}

fn parse_json_string_or_raw(raw: &str) -> Value {
    if raw.is_empty() {
        Value::Object(Map::new())
    } else {
        serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
    }
}