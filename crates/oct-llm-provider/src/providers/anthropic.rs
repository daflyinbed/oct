use async_stream::try_stream;
use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue, USER_AGENT};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::HashMap;

use crate::core::{
    ContentPart, FinishReason, Message, ModelError, ReasoningRequest, Role, StreamEvent,
    ToolResult, Usage,
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
    #[serde(skip)]
    prefix_fingerprint: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AnthropicRedactedThinkingBlock {
    data: String,
    #[serde(skip)]
    prefix_fingerprint: Option<String>,
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
    // Effort belongs to output_config, not to the thinking object.
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
    output_config: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
    #[serde(flatten)]
    other: Map<String, Value>,
    #[serde(skip)]
    reasoning_prefix: String,
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

    fn supports_adaptive_thinking(&self) -> bool {
        let model = self.info.model_id.replace('.', "-");
        let Some((_, name)) = model.split_once("claude-") else {
            // Unknown Anthropic-compatible models keep their manual budget.
            return false;
        };
        if name == "mythos-preview" || name.starts_with("mythos-preview-") {
            return true;
        }
        // Accept both current family-first names and older version-first
        // names, including provider prefixes and dated model IDs. A date
        // cannot be a u8, so a Claude 4 date suffix is not a minor version.
        let mut version = name.split('-');
        let Some(major) = version.find_map(|part| part.parse::<u8>().ok()) else {
            return false;
        };
        let minor = version
            .next()
            .and_then(|part| part.parse::<u8>().ok())
            .unwrap_or(0);
        major > 4 || (major == 4 && minor >= 6)
    }

    /// `GenerateOptions::reasoning` → anthropic `thinking` 块。
    /// - BudgetTokens → `{"type":"enabled","budget_tokens":n}`
    /// - Effort → adaptive thinking（`display:"summarized"`）；Opus 4.5 仍用 enabled
    /// - Enabled → 支持 adaptive 的模型使用 adaptive；旧模型使用 enabled +
    ///   保守默认预算（预算必须 ≥1024 且 < max_tokens）
    fn map_thinking(
        &self,
        req: &ChatRequest,
        max_output: u32,
    ) -> Result<Option<AnthropicThinkingConfig>, ModelError> {
        let Some(reasoning) = req.options.reasoning.as_ref() else {
            return Ok(None);
        };
        let use_adaptive = match reasoning {
            ReasoningRequest::Enabled => self.supports_adaptive_thinking(),
            ReasoningRequest::Effort { .. } => {
                !self.info.model_id.contains("claude-opus-4-5")
                    && !self.info.model_id.contains("claude-opus-4.5")
            }
            ReasoningRequest::BudgetTokens { .. } => false,
        };
        let thinking = match reasoning {
            ReasoningRequest::BudgetTokens { budget } => {
                if *budget < 1024 || *budget >= max_output {
                    return Err(ModelError::unsupported(
                        "anthropic thinking budget must be at least 1024 and smaller than max_tokens",
                    ));
                }
                AnthropicThinkingConfig {
                    thinking_type: "enabled".to_string(),
                    budget_tokens: Some(*budget),
                    display: None,
                }
            }
            ReasoningRequest::Enabled | ReasoningRequest::Effort { .. } if use_adaptive => {
                AnthropicThinkingConfig {
                    thinking_type: "adaptive".to_string(),
                    budget_tokens: None,
                    display: Some("summarized".to_string()),
                }
            }
            ReasoningRequest::Enabled | ReasoningRequest::Effort { .. } => {
                if max_output <= 1024 {
                    return Err(ModelError::unsupported(
                        "anthropic thinking requires max_tokens greater than 1024",
                    ));
                }
                let budget = (max_output / 2).clamp(1_024, 16_000).min(max_output - 1);
                AnthropicThinkingConfig {
                    thinking_type: "enabled".to_string(),
                    budget_tokens: Some(budget),
                    display: None,
                }
            }
        };
        Ok(Some(thinking))
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

            let mapped = map_anthropic_message(message)?;
            // An old reasoning-only row can become empty when its unsigned
            // reasoning is omitted. Empty assistant content is not valid wire.
            if !mapped.content.is_empty() {
                messages.push(mapped);
            }
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

        let max_output = req
            .options
            .max_output_tokens
            .or(self.info.limits.max_output_tokens)
            .unwrap_or(8192);
        let thinking = self.map_thinking(req, max_output)?;
        let mut other = req
            .options
            .provider_options
            .as_object()
            .cloned()
            .unwrap_or_default();
        let output_config =
            if let Some(ReasoningRequest::Effort { effort }) = &req.options.reasoning {
                let mut config = other
                    .remove("output_config")
                    .unwrap_or_else(|| Value::Object(Map::new()));
                config
                    .as_object_mut()
                    .ok_or_else(|| {
                        ModelError::unsupported("anthropic output_config must be an object")
                    })?
                    .insert("effort".into(), Value::String(effort.clone()));
                Some(config)
            } else {
                None
            };
        // Extended thinking 与 temperature≠1 互斥（API 400）——思考开启时
        // 丢弃 temperature，让思考档位优先。
        let temperature = if thinking.is_some() {
            None
        } else {
            req.options.temperature
        };

        let mut wire = AnthropicWireRequest {
            model: self.info.model_id.clone(),
            messages,
            system,
            temperature,
            top_p: req.options.top_p,
            top_k: None,
            max_tokens: Some(max_output),
            stop_sequences: req.options.stop_sequences.clone(),
            tools: req.tools.iter().map(map_anthropic_tool).collect(),
            tool_choice,
            metadata: None,
            thinking,
            output_config,
            stream: None,
            other,
            reasoning_prefix: String::new(),
        };
        replay::prepare(&mut wire)?;
        Ok(wire)
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

        let mut response = map_anthropic_response(payload)?;
        for part in &mut response.message.parts {
            if let ContentPart::ReasoningBlock(block) = part {
                block.bind_prefix(&wire.reasoning_prefix);
            }
        }
        Ok(response)
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

        let reasoning_prefix = wire.reasoning_prefix;
        let byte_stream = response.bytes_stream();
        let stream = try_stream! {
            // The generator is polled by the consumer, NOT inside the
            // caller's task-local scope, so the wire ctx (tags captured at
            // stream() entry) is moved in and every emission goes through
            // it. The guard reports llm_stream_end exactly once, including
            // the dropped-mid-flight (cancellation) case via Drop.
            let ctx = ctx;
            let mut decoder = SseDecoder::new();
            let mut state = stream::AnthropicStreamState::default();
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
                    for normalized in parse_anthropic_sse_with_wire(&event, &ctx, &guard, &mut state, &reasoning_prefix)? {
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
                for normalized in parse_anthropic_sse_with_wire(&event, &ctx, &guard, &mut state, &reasoning_prefix)? {
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
    state: &mut stream::AnthropicStreamState,
    reasoning_prefix: &str,
) -> Result<Vec<StreamEvent>, ModelError> {
    let mut events = parse_anthropic_sse_event_with_state(event, state).inspect_err(|err| {
        wire::log_error(ctx, "parse", None, &err.to_string(), None);
        guard.mark_error();
    })?;
    for event in &mut events {
        if let StreamEvent::ReasoningBlock(block) = event {
            block.bind_prefix(reasoning_prefix);
        }
    }
    Ok(events)
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
mod replay;
mod stream;
pub use mapping::*;
pub use stream::*;

#[cfg(test)]
mod reasoning_tests {
    use super::*;
    use crate::core::{GenerateOptions, Message, ReasoningRequest, Role};
    use crate::model::ChatRequest;
    use crate::provider::{ModelInfo, ModelLimits};

    fn model() -> AnthropicChatModel {
        AnthropicChatModel::new(
            ModelInfo::new("anthropic", "claude-sonnet-4-6").with_limits(ModelLimits {
                max_input_tokens: Some(200_000),
                max_output_tokens: Some(8_192),
                max_total_tokens: Some(200_000),
            }),
            AnthropicConfig {
                base_url: "http://localhost:1".to_string(),
                api_key: Some("test".to_string()),
            },
            reqwest::Client::new(),
        )
    }

    fn wire_req(reasoning: Option<ReasoningRequest>) -> AnthropicWireRequest {
        model()
            .to_wire_request(&ChatRequest {
                messages: vec![Message::text(Role::User, "hi")],
                tools: vec![],
                options: GenerateOptions {
                    reasoning,
                    temperature: Some(0.5),
                    ..GenerateOptions::default()
                },
            })
            .unwrap()
    }

    #[test]
    fn budget_tokens_maps_to_enabled_thinking() {
        let wire = wire_req(Some(ReasoningRequest::BudgetTokens { budget: 4_096 }));
        let thinking = wire.thinking.expect("thinking present");
        assert_eq!(thinking.thinking_type, "enabled");
        assert_eq!(thinking.budget_tokens, Some(4_096));
        // thinking 与 temperature≠1 互斥：temperature 必须被丢弃
        assert_eq!(wire.temperature, None);
    }

    #[test]
    fn effort_maps_to_adaptive_thinking() {
        let wire = wire_req(Some(ReasoningRequest::Effort {
            effort: "high".to_string(),
        }));
        let thinking = wire.thinking.expect("thinking present");
        assert_eq!(thinking.thinking_type, "adaptive");
        assert_eq!(wire.output_config.as_ref().unwrap()["effort"], "high");
        assert_eq!(thinking.budget_tokens, None);
        assert_eq!(thinking.display.as_deref(), Some("summarized"));
    }

    #[test]
    fn enabled_uses_adaptive_thinking_on_supported_claude_models() {
        for id in [
            "claude-sonnet-4-6",
            "claude-opus-4-6",
            "claude-opus-4-7",
            "claude-sonnet-5",
            "claude-haiku-5-5",
            "claude-opus-5-5",
            "claude-fable-5-1",
            "claude-mythos-preview",
            "anthropic.claude-haiku-5-5-v1:0",
            "claude-haiku-5.5",
        ] {
            let mut model = model();
            model.info.model_id = id.into();
            let request = ChatRequest {
                messages: vec![Message::text(Role::User, "hi")],
                options: GenerateOptions {
                    reasoning: Some(ReasoningRequest::Enabled),
                    temperature: Some(0.5),
                    ..Default::default()
                },
                ..Default::default()
            };
            let body = serde_json::to_value(model.to_wire_request(&request).unwrap()).unwrap();
            assert_eq!(
                body["thinking"],
                serde_json::json!({"type":"adaptive", "display":"summarized"}),
                "{id}",
            );
            assert!(body.get("output_config").is_none(), "{id}");
            assert!(body.get("temperature").is_none(), "{id}");
        }
    }

    #[test]
    fn legacy_enabled_maps_to_default_budget_within_output_limit() {
        for id in [
            "claude-opus-4-5",
            "claude-opus-4-5-20251101",
            "claude-sonnet-4-20250514",
            "claude-haiku-4-5",
            "claude-3-7-sonnet-20250219",
            "anthropic.claude-opus-4-5-v1:0",
            "claude-opus-4.5",
            "compatible-model",
        ] {
            let mut model = model();
            model.info.model_id = id.into();
            let request = ChatRequest {
                options: GenerateOptions {
                    reasoning: Some(ReasoningRequest::Enabled),
                    ..Default::default()
                },
                ..Default::default()
            };
            let thinking = model.to_wire_request(&request).unwrap().thinking.unwrap();
            assert_eq!(thinking.thinking_type, "enabled", "{id}");
            // 8192 输出上限 → 4096 预算（< max_tokens，≥1024）
            assert_eq!(thinking.budget_tokens, Some(4_096), "{id}");
        }
    }

    #[test]
    fn no_reasoning_leaves_thinking_off_and_temperature_intact() {
        let wire = wire_req(None);
        assert!(wire.thinking.is_none());
        assert_eq!(wire.temperature, Some(0.5));
    }

    #[test]
    fn adaptive_thinking_serializes_effort_in_output_config() {
        let wire = wire_req(Some(ReasoningRequest::Effort {
            effort: "low".to_string(),
        }));
        let body = serde_json::to_value(&wire).unwrap();
        assert_eq!(
            body["thinking"],
            serde_json::json!({"type": "adaptive", "display": "summarized"})
        );
        assert_eq!(body["output_config"]["effort"], "low");
        assert_eq!(body["max_tokens"], 8192);
    }

    #[test]
    fn manual_thinking_budget_must_fit_the_total_output_ceiling() {
        for budget in [512, 8192, 8193] {
            let request = ChatRequest {
                options: GenerateOptions {
                    reasoning: Some(ReasoningRequest::BudgetTokens { budget }),
                    max_output_tokens: Some(8192),
                    ..Default::default()
                },
                ..Default::default()
            };
            assert!(matches!(
                model().to_wire_request(&request),
                Err(ModelError::Unsupported(_))
            ));
        }
    }

    #[test]
    fn legacy_opus_effort_uses_manual_thinking_and_preserves_output_settings() {
        let mut model = model();
        model.info.model_id = "claude-opus-4-5".into();
        let request = ChatRequest {
            options: GenerateOptions {
                reasoning: Some(ReasoningRequest::Effort {
                    effort: "low".into(),
                }),
                provider_options: serde_json::json!({"output_config":{"format":{"type":"json_schema"}}}),
                ..Default::default()
            },
            ..Default::default()
        };
        let body = serde_json::to_value(model.to_wire_request(&request).unwrap()).unwrap();
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["output_config"]["effort"], "low");
        assert_eq!(body["output_config"]["format"]["type"], "json_schema");
    }

    #[test]
    fn generate_reasoning_blocks_survive_serialization_and_request_mapping() {
        let content = serde_json::json!([
            {"type":"thinking","thinking":"visible summary","signature":"opaque-signature"},
            {"type":"redacted_thinking","data":"opaque-redaction"},
            {"type":"text","text":"answer"},
        ]);
        let response = parse_anthropic_generate_body(&serde_json::json!({
            "id":"msg-test","type":"message","role":"assistant","model":"claude-sonnet-4-6",
            "content":content,"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1},
        }).to_string()).unwrap();
        let restored =
            serde_json::from_str(&serde_json::to_string(&response.message).unwrap()).unwrap();
        let request = ChatRequest {
            messages: vec![restored],
            ..Default::default()
        };
        let body = serde_json::to_value(model().to_wire_request(&request).unwrap()).unwrap();
        assert_eq!(body["messages"][0]["content"], content);
    }

    #[test]
    fn legacy_reasoning_without_provenance_is_filtered_only_for_bound_models() {
        let legacy: Message = serde_json::from_value(serde_json::json!({
            "role":"Assistant", "parts":[
                {"ReasoningBlock":{"Signed":{"text":"old summary", "signature":"old-signature"}}},
                {"ReasoningBlock":{"Redacted":{"data":"old-redaction"}}},
            ],
        }))
        .unwrap();
        let request = ChatRequest {
            messages: vec![
                Message::text(Role::User, "hi"),
                legacy,
                Message::text(Role::User, "continue"),
            ],
            ..Default::default()
        };
        for id in [
            "claude-opus-5-5",
            "anthropic.claude-sonnet-5-5-v1:0",
            "claude-haiku-5.5",
            "claude-fable-5-1",
        ] {
            let mut model = model();
            model.info.model_id = id.into();
            let wire = model.to_wire_request(&request).unwrap();
            assert_eq!(wire.messages.len(), 2, "{id}");
            assert!(
                wire.messages
                    .iter()
                    .all(|message| !message.content.is_empty())
            );
            assert!(wire.thinking.is_none());
        }
        // Manual thinking on older models still needs the complete blocks,
        // including rows written before request provenance was recorded.
        let mut manual = request;
        manual.options.reasoning = Some(ReasoningRequest::BudgetTokens { budget: 4096 });
        let wire = model().to_wire_request(&manual).unwrap();
        assert_eq!(wire.messages.len(), 3);
        assert_eq!(wire.messages[1].content.len(), 2);
        assert_eq!(wire.thinking.unwrap().thinking_type, "enabled");
    }

    #[test]
    fn unsigned_thinking_stream_fails_before_it_can_be_replayed() {
        let transcript = concat!(
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"summary\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        );
        assert!(parse_anthropic_sse_transcript(transcript).is_err());
    }

    #[test]
    fn legacy_unsigned_reasoning_never_becomes_empty_wire_content() {
        let request = ChatRequest {
            messages: vec![
                Message::text(Role::User, "hi"),
                Message::new(
                    Role::Assistant,
                    vec![ContentPart::Reasoning("old summary".into())],
                ),
                Message::text(Role::User, "continue"),
            ],
            ..Default::default()
        };
        let wire = model().to_wire_request(&request).unwrap();
        assert_eq!(wire.messages.len(), 2);
        assert!(
            wire.messages
                .iter()
                .all(|message| !message.content.is_empty())
        );
    }
}
