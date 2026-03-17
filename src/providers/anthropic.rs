use async_trait::async_trait;
use async_stream::try_stream;
use futures_util::StreamExt;
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::HashMap;

use crate::core::{ContentPart, FinishReason, Message, ModelError, StreamEvent, Usage};
use crate::model::{ChatModel, ChatRequest, ChatResponse, ChatStream};
use crate::provider::{ModelCapabilities, ModelInfo, ModelLimits, Provider};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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

#[derive(Debug, Clone, Default)]
struct AnthropicToolAccumulator {
    name: Option<String>,
    arguments: String,
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
                reasoning: false,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct AnthropicRequestMessage {
    role: String,
    content: Vec<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct AnthropicToolDefinition {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    input_schema: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct AnthropicWireRequest {
    model: String,
    messages: Vec<AnthropicRequestMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    stop_sequences: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    tools: Vec<AnthropicToolDefinition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<Value>,
    #[serde(skip_serializing_if = "Map::is_empty", default)]
    provider_options: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct AnthropicResponse {
    id: Option<String>,
    #[serde(default)]
    content: Vec<Value>,
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    usage: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct AnthropicStreamEvent {
    #[serde(rename = "type")]
    event_type: String,
    #[serde(default)]
    delta: Option<Value>,
    #[serde(default)]
    content_block: Option<Value>,
    #[serde(default)]
    usage: Option<Value>,
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
        let mut system_parts = Vec::new();
        let mut messages = Vec::new();

        for message in &req.messages {
            if matches!(message.role, crate::core::Role::System) {
                for part in &message.parts {
                    match part {
                        ContentPart::Text(text) | ContentPart::Reasoning(text) => {
                            system_parts.push(text.clone())
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

        Ok(AnthropicWireRequest {
            model: self.info.model_id.clone(),
            messages,
            system: if system_parts.is_empty() {
                None
            } else {
                Some(system_parts.join("\n\n"))
            },
            temperature: req.options.temperature,
            top_p: req.options.top_p,
            max_tokens: req.options.max_output_tokens,
            stop_sequences: req.options.stop_sequences.clone(),
            tools: req.tools.iter().map(map_anthropic_tool).collect(),
            tool_choice: req.options.tool_choice.as_ref().map(map_anthropic_tool_choice),
            provider_options: req.options.provider_options.clone(),
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
        wire.provider_options.insert("stream".to_string(), Value::Bool(true));

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
            let mut tool_state = HashMap::new();
            futures_util::pin_mut!(byte_stream);

            while let Some(chunk) = byte_stream.next().await {
                let chunk = chunk
                    .map_err(|err| ModelError::transport(format!("stream read failed: {err}")))?;
                let text = std::str::from_utf8(&chunk)
                    .map_err(|err| ModelError::transport(format!("stream chunk was not valid utf-8: {err}")))?;
                buffer.push_str(text);

                while let Some(event) = take_sse_event(&mut buffer) {
                    for normalized in parse_anthropic_sse_event_with_state(&event, &mut tool_state)? {
                        yield normalized;
                    }
                }
            }

            for remainder in flush_sse_buffer(&mut buffer) {
                for normalized in parse_anthropic_sse_event_with_state(&remainder, &mut tool_state)? {
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
        crate::core::Role::User => "user",
        crate::core::Role::Assistant => "assistant",
        crate::core::Role::Tool => "user",
        crate::core::Role::System => unreachable!(),
    }
    .to_string();

    let mut content = Vec::with_capacity(message.parts.len());
    for part in &message.parts {
        content.push(match part {
            ContentPart::Text(text) => json!({ "type": "text", "text": text }),
            ContentPart::ImageUrl { url } => json!({
                "type": "image",
                "source": {
                    "type": "url",
                    "url": url,
                }
            }),
            ContentPart::Reasoning(text) => json!({ "type": "text", "text": text }),
            ContentPart::ToolCall(call) => json!({
                "type": "tool_use",
                "id": call.id,
                "name": call.name,
                "input": call.arguments,
            }),
            ContentPart::ToolResult(result) => json!({
                "type": "tool_result",
                "tool_use_id": result.call_id,
                "content": result.content,
                "is_error": result.is_error,
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

fn map_anthropic_tool_choice(choice: &crate::core::ToolChoice) -> Value {
    match choice {
        crate::core::ToolChoice::Auto => json!({ "type": "auto" }),
        crate::core::ToolChoice::None => json!({ "type": "none" }),
        crate::core::ToolChoice::Required => json!({ "type": "any" }),
        crate::core::ToolChoice::Named(name) => json!({ "type": "tool", "name": name }),
    }
}

fn map_anthropic_response(payload: AnthropicResponse) -> Result<ChatResponse, ModelError> {
    let usage = payload.usage.as_ref().map(map_anthropic_usage);
    let mut provider_metadata = Map::new();
    if let Some(raw_usage) = payload.usage {
        provider_metadata.insert("raw_usage".to_string(), raw_usage);
    }

    Ok(ChatResponse {
        message: Message {
            role: crate::core::Role::Assistant,
            parts: payload
                .content
                .into_iter()
                .map(parse_anthropic_part)
                .collect::<Result<Vec<_>, _>>()?,
        },
        finish_reason: anthropic_finish_reason(payload.stop_reason.as_deref()),
        usage,
        provider_response_id: payload.id,
        provider_metadata,
    })
}

fn parse_anthropic_part(part: Value) -> Result<ContentPart, ModelError> {
    let kind = part
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| ModelError::provider("anthropic content part missing type"))?;

    match kind {
        "text" => Ok(ContentPart::Text(
            part.get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        )),
        "tool_use" => Ok(ContentPart::ToolCall(crate::core::ToolCall {
            id: part
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            name: part
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            arguments: part.get("input").cloned().unwrap_or(Value::Null),
        })),
        "tool_result" => Ok(ContentPart::ToolResult(crate::core::ToolResult {
            call_id: part
                .get("tool_use_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            content: part.get("content").cloned().unwrap_or(Value::Null),
            is_error: part
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })),
        other => Err(ModelError::provider(format!(
            "unsupported anthropic content part type: {other}"
        ))),
    }
}

pub fn map_anthropic_usage(raw: &Value) -> Usage {
    let mut provider_details = Map::new();
    provider_details.insert("raw".to_string(), raw.clone());

    let input_tokens = raw.get("input_tokens").and_then(Value::as_u64).map(|v| v as u32);
    let output_tokens = raw.get("output_tokens").and_then(Value::as_u64).map(|v| v as u32);
    let total_tokens = match (input_tokens, output_tokens) {
        (Some(input), Some(output)) => Some(input + output),
        _ => None,
    };

    Usage {
        input_tokens,
        output_tokens,
        total_tokens,
        reasoning_tokens: None,
        cached_input_tokens: None,
        provider_details,
    }
}

pub fn anthropic_finish_reason(raw: Option<&str>) -> FinishReason {
    match raw {
        Some("end_turn") | Some("stop_sequence") => FinishReason::Stop,
        Some("max_tokens") => FinishReason::Length,
        Some("tool_use") => FinishReason::ToolCalls,
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

#[cfg(test)]
fn parse_anthropic_sse_event(event: &str) -> Result<Vec<StreamEvent>, ModelError> {
    parse_anthropic_sse_event_with_state(event, &mut HashMap::new())
}

fn parse_anthropic_sse_event_with_state(
    event: &str,
    tool_state: &mut HashMap<String, AnthropicToolAccumulator>,
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
    let event: AnthropicStreamEvent = serde_json::from_str(&payload)
        .map_err(|err| ModelError::transport(format!("invalid anthropic stream chunk: {err}")))?;
    normalize_anthropic_stream_event_with_state(event, tool_state)
}

#[cfg(test)]
fn normalize_anthropic_stream_event(event: AnthropicStreamEvent) -> Result<Vec<StreamEvent>, ModelError> {
    normalize_anthropic_stream_event_with_state(event, &mut HashMap::new())
}

fn normalize_anthropic_stream_event_with_state(
    event: AnthropicStreamEvent,
    tool_state: &mut HashMap<String, AnthropicToolAccumulator>,
) -> Result<Vec<StreamEvent>, ModelError> {
    let mut events = Vec::new();

    if let Some(usage) = event.usage.as_ref() {
        events.push(StreamEvent::Usage(map_anthropic_usage(usage)));
    }

    match event.event_type.as_str() {
        "content_block_delta" => {
            if let Some(delta) = event.delta {
                events.extend(parse_anthropic_delta(delta, tool_state)?);
            }
        }
        "content_block_start" => {
            if let Some(block) = event.content_block {
                if let Some(start_event) = parse_anthropic_content_block_start(block, tool_state)? {
                    events.push(start_event);
                }
            }
        }
        "content_block_stop" => {
            if let Some(delta) = event.delta {
                if let Some(call_id) = delta.get("id").and_then(Value::as_str) {
                    if let Some(acc) = tool_state.remove(call_id) {
                        events.push(StreamEvent::ToolCall(crate::core::ToolCall {
                            id: call_id.to_string(),
                            name: acc.name.unwrap_or_default(),
                            arguments: parse_json_string_or_raw(&acc.arguments),
                        }));
                    }
                }
            }
        }
        "message_stop" => {
            for (call_id, acc) in std::mem::take(tool_state) {
                events.push(StreamEvent::ToolCall(crate::core::ToolCall {
                    id: call_id,
                    name: acc.name.unwrap_or_default(),
                    arguments: parse_json_string_or_raw(&acc.arguments),
                }));
            }
            events.push(StreamEvent::Finish(FinishReason::Stop));
        }
        _ => {}
    }

    Ok(events)
}

fn parse_anthropic_delta(
    delta: Value,
    tool_state: &mut HashMap<String, AnthropicToolAccumulator>,
) -> Result<Vec<StreamEvent>, ModelError> {
    let kind = delta
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();

    match kind {
        "text_delta" => Ok(vec![StreamEvent::TextDelta(
            delta.get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        )]),
        "thinking_delta" => Ok(vec![StreamEvent::ReasoningDelta(
            delta.get("thinking")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        )]),
        "input_json_delta" => {
            let call_id = delta
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let partial = delta
                .get("partial_json")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let accumulator = tool_state.entry(call_id.clone()).or_default();
            accumulator.arguments.push_str(&partial);
            Ok(vec![StreamEvent::ToolCallDelta {
                call_id,
                name: None,
                arguments_delta: partial,
            }])
        }
        _ => Ok(Vec::new()),
    }
}

fn parse_anthropic_content_block_start(
    block: Value,
    tool_state: &mut HashMap<String, AnthropicToolAccumulator>,
) -> Result<Option<StreamEvent>, ModelError> {
    let kind = block
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();

    match kind {
        "tool_use" => {
            let id = block
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let name = block
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let input = block.get("input").cloned().unwrap_or(Value::Null);

            tool_state.insert(
                id.clone(),
                AnthropicToolAccumulator {
                    name: Some(name.clone()),
                    arguments: match &input {
                        Value::Null => String::new(),
                        _ => input.to_string(),
                    },
                },
            );

            Ok(Some(StreamEvent::ToolCall(crate::core::ToolCall {
                id,
                name,
                arguments: input,
            })))
        }
        _ => Ok(None),
    }
}

fn parse_json_string_or_raw(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::provider::Provider;

    use super::{
        anthropic_finish_reason, map_anthropic_response, map_anthropic_usage,
        normalize_anthropic_stream_event, parse_anthropic_sse_event, AnthropicProvider,
        AnthropicResponse, AnthropicStreamEvent,
    };

    #[test]
    fn maps_anthropic_usage_fields() {
        let usage = map_anthropic_usage(&json!({
            "input_tokens": 20,
            "output_tokens": 9
        }));
        assert_eq!(usage.input_tokens, Some(20));
        assert_eq!(usage.output_tokens, Some(9));
        assert_eq!(usage.total_tokens, Some(29));
    }

    #[test]
    fn returns_anthropic_model_with_effective_limits() {
        let provider = AnthropicProvider::default();
        let model = provider.chat_model("claude-sonnet-4").unwrap();
        assert_eq!(model.info().provider_name, "anthropic");
        assert_eq!(model.info().limits.max_input_tokens, Some(200_000));
        assert_eq!(anthropic_finish_reason(Some("tool_use")), crate::core::FinishReason::ToolCalls);
    }

    #[test]
    fn normalizes_anthropic_response() {
        let response = map_anthropic_response(AnthropicResponse {
            id: Some("msg_123".to_string()),
            content: vec![
                json!({ "type": "text", "text": "hello" }),
                json!({
                    "type": "tool_use",
                    "id": "toolu_1",
                    "name": "lookup",
                    "input": { "q": "rust" }
                }),
            ],
            stop_reason: Some("tool_use".to_string()),
            usage: Some(json!({ "input_tokens": 15, "output_tokens": 6 })),
        })
        .unwrap();

        assert_eq!(response.provider_response_id.as_deref(), Some("msg_123"));
        assert_eq!(response.finish_reason, crate::core::FinishReason::ToolCalls);
        assert_eq!(response.usage.as_ref().and_then(|u| u.total_tokens), Some(21));
        assert!(response.provider_metadata.contains_key("raw_usage"));
        assert_eq!(response.message.parts.len(), 2);
    }

    #[test]
    fn normalizes_anthropic_stream_events() {
        let events = normalize_anthropic_stream_event(AnthropicStreamEvent {
            event_type: "content_block_delta".to_string(),
            delta: Some(json!({
                "type": "text_delta",
                "text": "hello"
            })),
            content_block: None,
            usage: Some(json!({ "input_tokens": 10, "output_tokens": 1 })),
        })
        .unwrap();

        assert!(matches!(&events[0], crate::core::StreamEvent::Usage(_)));
        assert!(matches!(&events[1], crate::core::StreamEvent::TextDelta(text) if text == "hello"));
    }

    #[test]
    fn parses_anthropic_sse_message_stop() {
        let events = parse_anthropic_sse_event(
            r#"event: message_stop
data: {"type":"message_stop"}"#,
        )
        .unwrap();
        assert_eq!(events, vec![crate::core::StreamEvent::Finish(crate::core::FinishReason::Stop)]);
    }

    #[test]
    fn emits_final_anthropic_tool_call_on_block_stop() {
        let start = normalize_anthropic_stream_event(AnthropicStreamEvent {
            event_type: "content_block_start".to_string(),
            delta: None,
            content_block: Some(json!({
                "type": "tool_use",
                "id": "toolu_1",
                "name": "lookup",
                "input": {}
            })),
            usage: None,
        })
        .unwrap();

        assert!(matches!(&start[0], crate::core::StreamEvent::ToolCall(call) if call.name == "lookup"));
    }
}
