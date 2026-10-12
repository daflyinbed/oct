use async_stream::try_stream;
use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue, USER_AGENT};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::HashMap;

use crate::core::{
    ContentPart, FinishReason, Message, ModelError, Role, StreamEvent, ToolResult, Usage,
};
use crate::model::{ChatModel, ChatRequest, ChatResponse, ChatStream};
use crate::provider::{ConnectionConfig, ModelCapabilities, ModelInfo, ModelLimits, Provider};
use crate::transport::{SseDecoder, check_response_status};
use crate::wire;

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
    pub api_key: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AnthropicChatModel {
    info: ModelInfo,
    config: AnthropicConfig,
    client: reqwest::Client,
}

/// Built-in endpoint used when neither the DB provider row nor the
/// environment names another one.
pub const ANTHROPIC_DEFAULT_BASE_URL: &str = "https://api.anthropic.com/v1";

/// The only environment variable the Anthropic family reads — once, at
/// provider construction (see [`AnthropicProvider::from_env`]).
pub const ANTHROPIC_API_KEY_ENV: &str = "ANTHROPIC_API_KEY";

#[derive(Debug, Clone)]
pub struct AnthropicProvider {
    /// Default connection: the built-in base URL plus the API key
    /// materialized from [`ANTHROPIC_API_KEY_ENV`] when the provider was
    /// constructed. DB rows override it per model via
    /// [`Provider::chat_model_with_connection`].
    pub config: AnthropicConfig,
    /// Shared HTTP client handed to every model this provider builds (a
    /// reqwest client clones cheaply and shares its connection pool, so
    /// per-run model construction no longer pays a fresh pool).
    client: reqwest::Client,
}

impl AnthropicProvider {
    /// Provider with explicit default connection settings.
    pub fn new(base_url: impl Into<String>, api_key: Option<String>) -> Self {
        Self::with_client(base_url, api_key, reqwest::Client::new())
    }

    /// [`AnthropicProvider::new`] over a caller-supplied (shared) HTTP
    /// client.
    pub fn with_client(
        base_url: impl Into<String>,
        api_key: Option<String>,
        client: reqwest::Client,
    ) -> Self {
        Self {
            config: AnthropicConfig {
                base_url: base_url.into(),
                api_key: api_key.filter(|key| !key.trim().is_empty()),
            },
            client,
        }
    }

    /// The Anthropic family's single environment entry point: reads
    /// [`ANTHROPIC_API_KEY_ENV`] once and materializes it into the default
    /// connection. Adapters built from this provider never read the env.
    pub fn from_env() -> Self {
        Self::from_env_with_client(reqwest::Client::new())
    }

    /// [`AnthropicProvider::from_env`] over a shared HTTP client (what
    /// `register_defaults` uses so every default provider shares one pool).
    pub fn from_env_with_client(client: reqwest::Client) -> Self {
        Self::with_client(
            ANTHROPIC_DEFAULT_BASE_URL,
            crate::provider::connection::env_api_key(ANTHROPIC_API_KEY_ENV),
            client,
        )
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
        Self::from_env()
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

/// Anthropic image source: a hosted URL or inline base64 bytes. `base64` is
/// what tool results (and data-URL user images) use — the API accepts raw
/// bytes only in that form.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
enum AnthropicImageSource {
    #[serde(rename = "url")]
    Url { url: String },
    #[serde(rename = "base64")]
    Base64 { media_type: String, data: String },
}

/// Map an internal image URL to an Anthropic image source. Data URLs
/// (`data:image/png;base64,…`) become inline base64 sources — Anthropic
/// expects the decoded form, not the data URL verbatim; anything else
/// passes through as a hosted URL source.
fn anthropic_image_source(url: &str) -> AnthropicImageSource {
    if let Some(rest) = url.strip_prefix("data:")
        && let Some((mime_type, data)) = rest.split_once(";base64,")
        && !mime_type.is_empty()
        && !data.is_empty()
    {
        return AnthropicImageSource::Base64 {
            media_type: mime_type.to_string(),
            data: data.to_string(),
        };
    }
    AnthropicImageSource::Url {
        url: url.to_string(),
    }
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
    Auto {
        disable_parallel_tool_use: Option<bool>,
    },
    #[serde(rename = "none")]
    None,
    #[serde(rename = "any")]
    Any {
        disable_parallel_tool_use: Option<bool>,
    },
    #[serde(rename = "tool")]
    Tool {
        name: String,
        disable_parallel_tool_use: Option<bool>,
    },
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
    MessageDelta {
        delta: AnthropicMessageDelta,
        usage: AnthropicMessageDeltaUsage,
    },
    #[serde(rename = "message_stop")]
    MessageStop,
    #[serde(rename = "content_block_start")]
    ContentBlockStart {
        index: usize,
        content_block: AnthropicContentBlock,
    },
    #[serde(rename = "content_block_delta")]
    ContentBlockDelta {
        index: usize,
        delta: AnthropicContentBlockDelta,
    },
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
    fn new(info: ModelInfo, config: AnthropicConfig, client: reqwest::Client) -> Self {
        Self {
            info,
            config,
            client,
        }
    }

    fn endpoint(&self) -> String {
        format!("{}/messages", self.config.base_url.trim_end_matches('/'))
    }

    fn build_headers(&self) -> Result<HeaderMap, ModelError> {
        // The key was materialized at construction (DB row or the startup
        // env snapshot) — the adapter never reads the process environment.
        // Missing/blank means nobody configured one: fail before traffic.
        let api_key = self
            .config
            .api_key
            .as_deref()
            .filter(|key| !key.is_empty())
            .ok_or(ModelError::Authentication)?;
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(
            "x-api-key",
            HeaderValue::from_str(api_key).map_err(|err| {
                ModelError::transport(format!("invalid anthropic api key header: {err}"))
            })?,
        );
        headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        headers.insert(USER_AGENT, crate::transport::user_agent());
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
                            ));
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

        let tool_choice = req
            .options
            .tool_choice
            .as_ref()
            .map(map_anthropic_tool_choice);

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
            other: req
                .options
                .provider_options
                .as_object()
                .cloned()
                .unwrap_or_default(),
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
        let ctx = wire::WireCtx::new("anthropic", &self.info.model_id, self.endpoint());
        if wire::enabled()
            && let Ok(body) = serde_json::to_string(&wire)
        {
            wire::log_request(&ctx, &body, false);
        }
        let response = self
            .client
            .post(self.endpoint())
            .headers(self.build_headers()?)
            .json(&wire)
            .send()
            .await
            .map_err(|err| {
                let message = format!("request failed: {err}");
                wire::log_error(&ctx, "connect", None, &message, None);
                ModelError::transport(message)
            })?;
        let status_code = response.status().as_u16();
        let response = match check_response_status(response, "anthropic").await {
            Ok(response) => response,
            Err(err) => {
                wire::log_error(&ctx, "status", Some(status_code), &err.to_string(), None);
                return Err(err);
            }
        };
        let body = response.text().await.map_err(|err| {
            let message = format!("failed to read response body: {err}");
            wire::log_error(&ctx, "read", Some(status_code), &message, None);
            ModelError::transport(message)
        })?;
        wire::log_response(&ctx, status_code, &body, ctx.elapsed_ms());
        let payload: AnthropicResponse = serde_json::from_str(&body).map_err(|err| {
            let message = format!("invalid response json: {err}");
            wire::log_error(&ctx, "parse", Some(status_code), &message, Some(&body));
            ModelError::transport(message)
        })?;

        map_anthropic_response(payload)
    }

    async fn stream(&self, req: ChatRequest) -> Result<ChatStream, ModelError> {
        let mut wire = self.to_wire_request(&req)?;
        wire.stream = Some(true);

        let ctx = wire::WireCtx::new("anthropic", &self.info.model_id, self.endpoint());
        if wire::enabled()
            && let Ok(body) = serde_json::to_string(&wire)
        {
            wire::log_request(&ctx, &body, true);
        }
        let response = self
            .client
            .post(self.endpoint())
            .headers(self.build_headers()?)
            .json(&wire)
            .send()
            .await
            .map_err(|err| {
                let message = format!("request failed: {err}");
                wire::log_error(&ctx, "connect", None, &message, None);
                ModelError::transport(message)
            })?;
        let status_code = response.status().as_u16();
        let response = match check_response_status(response, "anthropic").await {
            Ok(response) => response,
            Err(err) => {
                wire::log_error(&ctx, "status", Some(status_code), &err.to_string(), None);
                return Err(err);
            }
        };

        let byte_stream = response.bytes_stream();
        let stream = try_stream! {
            // The generator is polled by the consumer, NOT inside the
            // caller's task-local scope, so the wire ctx (tags captured at
            // stream() entry) is moved in and every emission goes through
            // it. The guard reports llm_stream_end exactly once, including
            // the dropped-mid-flight (cancellation) case via Drop.
            let ctx = ctx;
            let mut decoder = SseDecoder::new();
            let mut tool_state: HashMap<usize, AnthropicToolAccumulator> = HashMap::new();
            let mut stop_reason: Option<String> = None;
            let guard = wire::StreamEndGuard::new(ctx.clone());
            futures_util::pin_mut!(byte_stream);

            while let Some(chunk) = byte_stream.next().await {
                let chunk = chunk.map_err(|err| {
                    let message = format!("stream read failed: {err}");
                    wire::log_error(&ctx, "read", None, &message, None);
                    guard.mark_error();
                    ModelError::transport(message)
                })?;
                for event in decoder.push(&chunk).inspect_err(|err| {
                    wire::log_error(&ctx, "parse", None, &err.to_string(), None);
                    guard.mark_error();
                })? {
                    wire::log_sse(&ctx, guard.next_seq(), &event);
                    guard.count();
                    for normalized in parse_anthropic_sse_with_wire(&event, &ctx, &guard, &mut tool_state, &mut stop_reason)? {
                        yield normalized;
                    }
                }
            }

            for event in decoder.finish().inspect_err(|err| {
                wire::log_error(&ctx, "parse", None, &err.to_string(), None);
                guard.mark_error();
            })? {
                wire::log_sse(&ctx, guard.next_seq(), &event);
                guard.count();
                for normalized in parse_anthropic_sse_with_wire(&event, &ctx, &guard, &mut tool_state, &mut stop_reason)? {
                    yield normalized;
                }
            }
            guard.finish();
        };

        Ok(Box::pin(stream))
    }
}

/// Parse one framed SSE event with wire-log error reporting: a parse
/// failure is logged (stage `parse`) and marks the stream outcome `error`
/// before propagating — the generator's bare `?` can do neither.
fn parse_anthropic_sse_with_wire(
    event: &str,
    ctx: &wire::WireCtx,
    guard: &wire::StreamEndGuard,
    tool_state: &mut HashMap<usize, AnthropicToolAccumulator>,
    stop_reason: &mut Option<String>,
) -> Result<Vec<StreamEvent>, ModelError> {
    parse_anthropic_sse_event_with_state(event, tool_state, stop_reason).inspect_err(|err| {
        wire::log_error(ctx, "parse", None, &err.to_string(), None);
        guard.mark_error();
    })
}

impl Provider for AnthropicProvider {
    fn name(&self) -> &'static str {
        "anthropic"
    }

    fn chat_model(&self, model: &str) -> Result<Box<dyn ChatModel>, ModelError> {
        self.chat_model_with_connection(model, &ConnectionConfig::new())
    }

    /// DB-driven construction: every `Some` field of `connection` (from a
    /// provider row) wins outright; `None` falls back to the family's own
    /// default — the built-in base URL and the `ANTHROPIC_API_KEY`
    /// materialized when this provider was constructed.
    fn chat_model_with_connection(
        &self,
        model: &str,
        connection: &ConnectionConfig,
    ) -> Result<Box<dyn ChatModel>, ModelError> {
        let config = AnthropicConfig {
            base_url: connection
                .base_url
                .clone()
                .unwrap_or_else(|| self.config.base_url.clone()),
            api_key: connection.api_key.clone().or(self.config.api_key.clone()),
        };
        Ok(Box::new(AnthropicChatModel::new(
            self.model_info(model),
            config,
            self.client.clone(),
        )))
    }

    fn default_connection(&self) -> ConnectionConfig {
        ConnectionConfig {
            base_url: Some(self.config.base_url.clone()),
            api_key: self.config.api_key.clone(),
        }
    }
}

mod mapping;
mod stream;
pub use mapping::*;
pub use stream::*;
