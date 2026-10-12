use async_stream::try_stream;
use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::header::{
    AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, USER_AGENT,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::HashMap;

use crate::core::{
    ContentPart, FinishReason, Message, ModelError, StreamEvent, ToolChoice, ToolSpec, Usage,
};
use crate::model::{ChatModel, ChatRequest, ChatResponse, ChatStream};
use crate::provider::ModelInfo;
use crate::transport::{SseDecoder, check_response_status};
use crate::wire;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OpenAiCompatibleConfig {
    pub provider_name: String,
    pub base_url: String,
    /// Materialized API key (a DB provider row, or the provider family's
    /// env-materialized default) — never read from the environment at
    /// request time. `None`/blank fails with `ModelError::Authentication`
    /// before any traffic.
    #[serde(default)]
    pub api_key: Option<String>,
    pub default_headers: Map<String, Value>,
    pub model_info: ModelInfo,
    pub use_responses_api: bool,
}

#[derive(Debug, Clone)]
pub struct OpenAiCompatibleChatModel {
    pub config: OpenAiCompatibleConfig,
    client: reqwest::Client,
}

#[derive(Debug, Clone, Default)]
struct OpenAiToolCallAccumulator {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

impl OpenAiCompatibleChatModel {
    pub fn new(config: OpenAiCompatibleConfig) -> Self {
        Self::new_with_client(config, reqwest::Client::new())
    }

    /// Construct over a caller-supplied (shared) HTTP client: reqwest
    /// clients clone cheaply and share their connection pool, so per-run
    /// model construction no longer pays a fresh pool.
    pub fn new_with_client(config: OpenAiCompatibleConfig, client: reqwest::Client) -> Self {
        Self { config, client }
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
        // The key was materialized at construction (DB row or the provider
        // family's env snapshot) — the adapter never reads the process
        // environment. Missing/blank means nobody configured one.
        let api_key = self
            .config
            .api_key
            .as_deref()
            .filter(|key| !key.is_empty())
            .ok_or(ModelError::Authentication)?;
        HeaderValue::from_str(&format!("Bearer {api_key}"))
            .map_err(|err| ModelError::transport(format!("invalid auth header: {err}")))
    }

    fn build_headers(&self) -> Result<HeaderMap, ModelError> {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(AUTHORIZATION, self.auth_header_value()?);
        // Identity default first: a caller-configured `User-Agent` in
        // default_headers is applied below and wins over it.
        headers.insert(USER_AGENT, crate::transport::user_agent());

        for (key, value) in &self.config.default_headers {
            let value = value.as_str().ok_or_else(|| {
                ModelError::provider(format!("default header {key} must be a string"))
            })?;
            let name = HeaderName::from_bytes(key.as_bytes()).map_err(|err| {
                ModelError::transport(format!("invalid header name {key}: {err}"))
            })?;
            let value = HeaderValue::from_str(value).map_err(|err| {
                ModelError::transport(format!("invalid header value for {key}: {err}"))
            })?;
            headers.insert(name, value);
        }

        Ok(headers)
    }

    pub fn to_wire_request(&self, req: &ChatRequest) -> Result<ChatCompletionRequest, ModelError> {
        let messages = map_request_messages(&req.messages)?;

        let tools = req.tools.iter().map(map_tool).collect();
        let tool_choice = req.options.tool_choice.as_ref().map(map_tool_choice);
        let response_format =
            req.options
                .json_schema
                .clone()
                .map(|schema| ChatCompletionResponseFormat {
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

    fn to_streaming_wire_request(
        &self,
        req: &ChatRequest,
    ) -> Result<ChatCompletionRequest, ModelError> {
        let mut wire = self.to_wire_request(req)?;
        wire.stream = true;
        wire.stream_options = Some(ChatCompletionStreamOptions {
            include_usage: true,
        });
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
        let ctx = wire::WireCtx::new(
            &self.config.provider_name,
            &self.config.model_info.model_id,
            self.endpoint(),
        );
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
        let response = match check_response_status(response, &self.config.provider_name).await {
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
        let payload: ChatCompletionResponse = serde_json::from_str(&body).map_err(|err| {
            let message = format!("invalid response json: {err}");
            wire::log_error(&ctx, "parse", Some(status_code), &message, Some(&body));
            ModelError::transport(message)
        })?;

        map_chat_completion_response(payload)
    }

    async fn stream(&self, req: ChatRequest) -> Result<ChatStream, ModelError> {
        let wire = self.to_streaming_wire_request(&req)?;
        let ctx = wire::WireCtx::new(
            &self.config.provider_name,
            &self.config.model_info.model_id,
            self.endpoint(),
        );
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
        let response = match check_response_status(response, &self.config.provider_name).await {
            Ok(response) => response,
            Err(err) => {
                wire::log_error(&ctx, "status", Some(status_code), &err.to_string(), None);
                return Err(err);
            }
        };

        let byte_stream = response.bytes_stream();
        let stream = try_stream! {
            // See anthropic.rs: the generator is polled outside the
            // caller's task-local scope, so the wire ctx moves in and the
            // guard reports llm_stream_end exactly once (incl. Drop).
            // try_stream! only supports `?`, so error paths log via map_err
            // side effects before propagating.
            let ctx = ctx;
            let mut decoder = SseDecoder::new();
            let mut tool_state = HashMap::new();
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
                    for normalized in parse_openai_sse_with_wire(&event, &ctx, &guard, &mut tool_state)? {
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
                for normalized in parse_openai_sse_with_wire(&event, &ctx, &guard, &mut tool_state)? {
                    yield normalized;
                }
            }
            guard.finish();
        };

        Ok(Box::pin(stream))
    }
}

/// Parse one framed SSE event with wire-log error reporting (see the
/// anthropic twin: log + mark the stream outcome before propagating).
fn parse_openai_sse_with_wire(
    event: &str,
    ctx: &wire::WireCtx,
    guard: &wire::StreamEndGuard,
    tool_state: &mut HashMap<String, OpenAiToolCallAccumulator>,
) -> Result<Vec<StreamEvent>, ModelError> {
    parse_openai_sse_event_with_state(event, tool_state).inspect_err(|err| {
        wire::log_error(ctx, "parse", None, &err.to_string(), None);
        guard.mark_error();
    })
}

mod mapping;
mod stream;
pub use mapping::*;
pub use stream::*;

mod wire_types;
pub use wire_types::*;
